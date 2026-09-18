use crate::device_events::DeviceEventHub;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::{sleep, timeout};

pub const ROOT_ITEM_IDENTIFIER: &str = "root";
const LOCAL_STORAGE_APP_GROUP_IDENTIFIER: &str = "group.com.apple.FileProvider.LocalStorage";
const FILE_PROVIDER_STORAGE_DIRECTORY: &str = "File Provider Storage";
const HOST_STATE_RELATIVE_PATH: &str = "Library/Application Support/SimDeck Host Files";
const SIMULATOR_HOME_TIMEOUT: Duration = Duration::from_secs(3);
const FILE_MONITOR_INTERVAL: Duration = Duration::from_millis(500);
const STALE_STAGING_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_FILE_NAME_BYTES: usize = 255;
const ITEM_ID_HEX_LENGTH: usize = 32;
static ITEM_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileItem {
    pub id: String,
    pub parent_id: String,
    pub name: String,
    pub kind: FileItemKind,
    pub content_type: Option<String>,
    pub size: u64,
    pub created_at: u64,
    pub modified_at: u64,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileItemKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredItem {
    item: FileItem,
    relative_path: PathBuf,
}

#[derive(Clone, Default)]
pub struct SimulatorFiles {
    monitors: Arc<Mutex<FileMonitors>>,
    snapshots: Arc<Mutex<HashMap<String, HashMap<String, FileItem>>>>,
}

#[derive(Default)]
struct FileMonitors {
    clients: HashMap<String, usize>,
    running: HashSet<String>,
}

/// Keeps the file-provider poll loop alive for one control connection; the loop
/// stops once the last connection for the device drops.
pub struct FileMonitor {
    files: SimulatorFiles,
    udid: String,
}

impl Drop for FileMonitor {
    fn drop(&mut self) {
        let mut monitors = self.files.monitors.lock().unwrap();
        if let Some(count) = monitors.clients.get_mut(&self.udid) {
            *count -= 1;
            if *count == 0 {
                monitors.clients.remove(&self.udid);
            }
        }
    }
}

impl SimulatorFiles {
    pub async fn store_for_device(&self, udid: &str) -> Result<ProviderStore, FilesError> {
        let group_container = resolve_local_storage_group(udid).await?;
        ProviderStore::open_in_group_container(&group_container).await
    }

    pub fn monitor(&self, udid: String, events: DeviceEventHub) -> FileMonitor {
        let should_start = {
            let mut monitors = self.monitors.lock().unwrap();
            *monitors.clients.entry(udid.clone()).or_default() += 1;
            monitors.running.insert(udid.clone())
        };
        if should_start {
            let files = self.clone();
            let udid = udid.clone();
            tokio::spawn(async move {
                while files.keep_monitoring(&udid) {
                    if let Ok(store) = files.store_for_device(&udid).await {
                        if let Ok(items) = store.list(None).await {
                            files.publish_snapshot_changes(&udid, items, &events);
                        }
                    }
                    sleep(FILE_MONITOR_INTERVAL).await;
                }
            });
        }
        FileMonitor {
            files: self.clone(),
            udid,
        }
    }

    fn keep_monitoring(&self, udid: &str) -> bool {
        let mut monitors = self.monitors.lock().unwrap();
        if monitors.clients.contains_key(udid) {
            return true;
        }
        monitors.running.remove(udid);
        false
    }

    pub async fn refresh_snapshot(&self, udid: &str) -> Result<(), FilesError> {
        let items = self.store_for_device(udid).await?.list(None).await?;
        self.snapshots.lock().unwrap().insert(
            udid.to_owned(),
            items
                .into_iter()
                .map(|item| (item.id.clone(), item))
                .collect(),
        );
        Ok(())
    }

    fn publish_snapshot_changes(&self, udid: &str, items: Vec<FileItem>, events: &DeviceEventHub) {
        let next = items
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect::<HashMap<_, _>>();
        let previous = self
            .snapshots
            .lock()
            .unwrap()
            .insert(udid.to_owned(), next.clone());
        let Some(previous) = previous else {
            return;
        };
        for (id, item) in &next {
            let event_type = match previous.get(id) {
                None => Some("file.created"),
                Some(before) if before != item => Some("file.changed"),
                Some(_) => None,
            };
            if let Some(event_type) = event_type {
                events.publish(
                    udid,
                    json!({
                        "type": event_type,
                        "udid": udid,
                        "item": item,
                        "source": "native",
                    }),
                );
            }
        }
        for (id, item) in previous {
            if !next.contains_key(&id) {
                events.publish(
                    udid,
                    json!({
                        "type": "file.deleted",
                        "udid": udid,
                        "item": item,
                        "source": "native",
                    }),
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProviderStore {
    root: PathBuf,
    metadata: PathBuf,
    staging: PathBuf,
}

impl ProviderStore {
    pub async fn open_in_group_container(group_container: &Path) -> Result<Self, FilesError> {
        let group_container = canonical_directory(group_container).await?;
        let root = group_container.join(FILE_PROVIDER_STORAGE_DIRECTORY);
        ensure_directory(&root).await?;
        let root = fs::canonicalize(&root).await.map_err(FilesError::Io)?;
        if !root.starts_with(&group_container) {
            return Err(FilesError::UnsafeStorage(
                "The native Files root resolves outside its app group.".to_owned(),
            ));
        }

        let state = group_container.join(HOST_STATE_RELATIVE_PATH);
        let metadata = state.join("metadata");
        let staging = state.join("staging");
        for directory in [&metadata, &staging] {
            ensure_directory(directory).await?;
            let canonical = fs::canonicalize(directory).await.map_err(FilesError::Io)?;
            if !canonical.starts_with(&group_container) {
                return Err(FilesError::UnsafeStorage(format!(
                    "Files state directory {} resolves outside its app group.",
                    directory.display()
                )));
            }
        }
        cleanup_stale_staging(&staging).await?;

        Ok(Self {
            root,
            metadata,
            staging,
        })
    }

    #[cfg(test)]
    async fn open_test_root(group_container: &Path) -> Result<Self, FilesError> {
        ensure_directory(group_container).await?;
        Self::open_in_group_container(group_container).await
    }

    pub async fn list(&self, parent_id: Option<&str>) -> Result<Vec<FileItem>, FilesError> {
        let mut items = self.reconcile().await?;
        if let Some(parent_id) = parent_id {
            validate_parent_identifier(parent_id)?;
            if parent_id != ROOT_ITEM_IDENTIFIER
                && !items.iter().any(|item| {
                    item.item.id == parent_id && item.item.kind == FileItemKind::Directory
                })
            {
                return Err(FilesError::NotFound(format!(
                    "Unknown Files directory {parent_id}."
                )));
            }
            items.retain(|item| item.item.parent_id == parent_id);
        }
        let mut items = items.into_iter().map(|item| item.item).collect::<Vec<_>>();
        items.sort_by(|left, right| {
            let left_directory = left.kind == FileItemKind::Directory;
            let right_directory = right.kind == FileItemKind::Directory;
            right_directory
                .cmp(&left_directory)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(items)
    }

    pub async fn create_directory(
        &self,
        parent_id: &str,
        name: &str,
    ) -> Result<FileItem, FilesError> {
        validate_file_name(name)?;
        self.reconcile().await?;
        let relative_path = self.destination_relative_path(parent_id, name).await?;
        let path = self.safe_path(&relative_path, false).await?;
        if fs::symlink_metadata(&path).await.is_ok() {
            return Err(FilesError::Conflict(format!(
                "An item named {name:?} already exists in this directory."
            )));
        }
        fs::create_dir(&path).await.map_err(FilesError::Io)?;
        let metadata = fs::metadata(&path).await.map_err(FilesError::Io)?;
        let stored = StoredItem {
            item: FileItem {
                id: new_opaque_id(parent_id, name),
                parent_id: parent_id.to_owned(),
                name: name.to_owned(),
                kind: FileItemKind::Directory,
                content_type: None,
                size: 0,
                created_at: system_time_ms(metadata.created().unwrap_or(SystemTime::UNIX_EPOCH)),
                modified_at: system_time_ms(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
                version: 1,
            },
            relative_path,
        };
        self.write_stored_item(&stored).await?;
        Ok(stored.item)
    }

    pub async fn begin_upload(&self, transfer_id: &str) -> Result<StagedUpload, FilesError> {
        validate_transfer_identifier(transfer_id)?;
        let path = self.staging.join(format!("{transfer_id}.partial"));
        reject_symlink_if_present(&path).await?;
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .await
            .map_err(FilesError::Io)?;
        Ok(StagedUpload {
            path,
            file: Some(file),
            bytes_written: 0,
            committed: false,
        })
    }

    pub async fn commit_upload(
        &self,
        mut upload: StagedUpload,
        parent_id: &str,
        name: &str,
        content_type: &str,
    ) -> Result<FileItem, FilesError> {
        validate_file_name(name)?;
        validate_content_type(content_type)?;
        self.reconcile().await?;
        let relative_path = self.destination_relative_path(parent_id, name).await?;
        let path = self.safe_path(&relative_path, false).await?;
        if fs::symlink_metadata(&path).await.is_ok() {
            return Err(FilesError::Conflict(format!(
                "An item named {name:?} already exists in this directory."
            )));
        }
        upload.finish_writing().await?;
        fs::rename(&upload.path, &path)
            .await
            .map_err(FilesError::Io)?;
        upload.committed = true;
        let metadata = fs::metadata(&path).await.map_err(FilesError::Io)?;
        let stored = StoredItem {
            item: FileItem {
                id: new_opaque_id(parent_id, name),
                parent_id: parent_id.to_owned(),
                name: name.to_owned(),
                kind: FileItemKind::File,
                content_type: Some(content_type.to_owned()),
                size: upload.bytes_written,
                created_at: system_time_ms(metadata.created().unwrap_or(SystemTime::UNIX_EPOCH)),
                modified_at: system_time_ms(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
                version: 1,
            },
            relative_path,
        };
        if let Err(error) = self.write_stored_item(&stored).await {
            let _ = fs::remove_file(&path).await;
            return Err(error);
        }
        Ok(stored.item)
    }

    pub async fn update(
        &self,
        id: &str,
        name: Option<&str>,
        parent_id: Option<&str>,
    ) -> Result<FileItem, FilesError> {
        let all_items = self.reconcile().await?;
        let mut stored = all_items
            .iter()
            .find(|item| item.item.id == id)
            .cloned()
            .ok_or_else(|| FilesError::NotFound(format!("Unknown Files item {id}.")))?;
        let next_name = name.unwrap_or(&stored.item.name);
        let next_parent = parent_id.unwrap_or(&stored.item.parent_id);
        validate_file_name(next_name)?;
        if stored.item.kind == FileItemKind::Directory {
            let descendant_ids = self.descendant_ids(&all_items, id);
            if descendant_ids.contains(next_parent) {
                return Err(FilesError::InvalidInput(
                    "A directory cannot be moved inside itself.".to_owned(),
                ));
            }
        }

        let next_relative_path = self
            .destination_relative_path(next_parent, next_name)
            .await?;
        if next_relative_path != stored.relative_path {
            let source = self.safe_path(&stored.relative_path, true).await?;
            let destination = self.safe_path(&next_relative_path, false).await?;
            if fs::symlink_metadata(&destination).await.is_ok() {
                return Err(FilesError::Conflict(format!(
                    "An item named {next_name:?} already exists in this directory."
                )));
            }
            fs::rename(&source, &destination)
                .await
                .map_err(FilesError::Io)?;

            let old_relative_path = stored.relative_path.clone();
            for mut descendant in all_items.into_iter().filter(|candidate| {
                candidate.item.id != id && candidate.relative_path.starts_with(&old_relative_path)
            }) {
                let suffix = descendant
                    .relative_path
                    .strip_prefix(&old_relative_path)
                    .map_err(|_| {
                        FilesError::UnsafeStorage(
                            "Unable to update a moved directory descendant.".to_owned(),
                        )
                    })?;
                descendant.relative_path = next_relative_path.join(suffix);
                self.write_stored_item(&descendant).await?;
            }
        }
        stored.relative_path = next_relative_path;
        stored.item.name = next_name.to_owned();
        stored.item.parent_id = next_parent.to_owned();
        stored.item.modified_at = now_ms();
        stored.item.version = stored.item.version.saturating_add(1);
        self.write_stored_item(&stored).await?;
        Ok(stored.item)
    }

    pub async fn delete(&self, id: &str) -> Result<Vec<FileItem>, FilesError> {
        let all_items = self.reconcile().await?;
        let stored = all_items
            .iter()
            .find(|item| item.item.id == id)
            .cloned()
            .ok_or_else(|| FilesError::NotFound(format!("Unknown Files item {id}.")))?;
        let mut deleted = all_items
            .into_iter()
            .filter(|candidate| {
                candidate.item.id == id
                    || candidate.relative_path.starts_with(&stored.relative_path)
            })
            .collect::<Vec<_>>();
        deleted.sort_by_key(|item| std::cmp::Reverse(item.relative_path.components().count()));

        let path = self.safe_path(&stored.relative_path, true).await?;
        match stored.item.kind {
            FileItemKind::File => fs::remove_file(path).await.map_err(FilesError::Io)?,
            FileItemKind::Directory => fs::remove_dir_all(path).await.map_err(FilesError::Io)?,
        }
        for item in &deleted {
            remove_regular_file_if_present(&self.metadata_path(&item.item.id)?).await?;
        }
        Ok(deleted.into_iter().map(|item| item.item).collect())
    }

    pub async fn open_content(&self, id: &str) -> Result<(FileItem, File), FilesError> {
        let stored = self.stored_item(id).await?;
        if stored.item.kind != FileItemKind::File {
            return Err(FilesError::InvalidInput(
                "Directories cannot be downloaded.".to_owned(),
            ));
        }
        let path = self.safe_path(&stored.relative_path, true).await?;
        let metadata = fs::symlink_metadata(&path).await.map_err(FilesError::Io)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(FilesError::UnsafeStorage(
                "Stored file content is not a regular file.".to_owned(),
            ));
        }
        Ok((stored.item, File::open(path).await.map_err(FilesError::Io)?))
    }

    async fn stored_item(&self, id: &str) -> Result<StoredItem, FilesError> {
        validate_item_identifier(id)?;
        self.reconcile()
            .await?
            .into_iter()
            .find(|item| item.item.id == id)
            .ok_or_else(|| FilesError::NotFound(format!("Unknown Files item {id}.")))
    }

    async fn destination_relative_path(
        &self,
        parent_id: &str,
        name: &str,
    ) -> Result<PathBuf, FilesError> {
        validate_parent_identifier(parent_id)?;
        validate_file_name(name)?;
        if parent_id == ROOT_ITEM_IDENTIFIER {
            return Ok(PathBuf::from(name));
        }
        let parent = self
            .read_stored_items()
            .await?
            .into_iter()
            .find(|item| item.item.id == parent_id)
            .ok_or_else(|| FilesError::NotFound(format!("Unknown Files directory {parent_id}.")))?;
        if parent.item.kind != FileItemKind::Directory {
            return Err(FilesError::InvalidInput(
                "The selected parent is not a directory.".to_owned(),
            ));
        }
        Ok(parent.relative_path.join(name))
    }

    fn descendant_ids(&self, items: &[StoredItem], id: &str) -> HashSet<String> {
        let mut descendants = HashSet::from([id.to_owned()]);
        loop {
            let before = descendants.len();
            for item in items {
                if descendants.contains(&item.item.parent_id) {
                    descendants.insert(item.item.id.clone());
                }
            }
            if descendants.len() == before {
                return descendants;
            }
        }
    }

    async fn reconcile(&self) -> Result<Vec<StoredItem>, FilesError> {
        let stored = self.read_stored_items().await?;
        let mut by_relative_path = stored
            .into_iter()
            .map(|item| (item.relative_path.clone(), item))
            .collect::<HashMap<_, _>>();
        let mut scanned = self.scan_visible_root().await?;
        scanned.sort_by_key(|item| item.relative_path.components().count());

        let mut id_by_relative_path =
            HashMap::from([(PathBuf::new(), ROOT_ITEM_IDENTIFIER.to_owned())]);
        let mut reconciled = Vec::with_capacity(scanned.len());
        for scanned in scanned {
            let parent_relative_path = scanned
                .relative_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .to_path_buf();
            let parent_id = id_by_relative_path
                .get(&parent_relative_path)
                .cloned()
                .ok_or_else(|| {
                    FilesError::UnsafeStorage(format!(
                        "Files item {} has no visible parent.",
                        scanned.relative_path.display()
                    ))
                })?;
            let name = scanned
                .relative_path
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    FilesError::UnsafeStorage("Files contains a non-Unicode name.".to_owned())
                })?
                .to_owned();
            let previous = by_relative_path.remove(&scanned.relative_path);
            let id = previous
                .as_ref()
                .map(|item| item.item.id.clone())
                .unwrap_or_else(|| new_opaque_id(&parent_id, &name));
            let content_type = match scanned.kind {
                FileItemKind::Directory => None,
                FileItemKind::File => previous
                    .as_ref()
                    .and_then(|item| item.item.content_type.clone())
                    .or_else(|| content_type_for_name(&name).map(str::to_owned)),
            };
            let changed = previous.as_ref().is_some_and(|item| {
                item.item.name != name
                    || item.item.parent_id != parent_id
                    || item.item.kind != scanned.kind
                    || item.item.size != scanned.size
                    || item.item.modified_at != scanned.modified_at
                    || item.item.content_type != content_type
            });
            let item = StoredItem {
                item: FileItem {
                    id: id.clone(),
                    parent_id,
                    name,
                    kind: scanned.kind,
                    content_type,
                    size: scanned.size,
                    created_at: previous
                        .as_ref()
                        .map(|item| item.item.created_at)
                        .unwrap_or(scanned.created_at),
                    modified_at: scanned.modified_at,
                    version: previous
                        .as_ref()
                        .map(|item| item.item.version.saturating_add(u64::from(changed)))
                        .unwrap_or(1),
                },
                relative_path: scanned.relative_path.clone(),
            };
            if previous.as_ref() != Some(&item) {
                self.write_stored_item(&item).await?;
            }
            id_by_relative_path.insert(scanned.relative_path, id);
            reconciled.push(item);
        }
        for removed in by_relative_path.into_values() {
            remove_regular_file_if_present(&self.metadata_path(&removed.item.id)?).await?;
        }
        Ok(reconciled)
    }

    async fn scan_visible_root(&self) -> Result<Vec<ScannedItem>, FilesError> {
        let mut directories = vec![self.root.clone()];
        let mut items = Vec::new();
        while let Some(directory) = directories.pop() {
            let mut entries = fs::read_dir(&directory).await.map_err(FilesError::Io)?;
            while let Some(entry) = entries.next_entry().await.map_err(FilesError::Io)? {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with('.') {
                    continue;
                }
                let file_type = entry.file_type().await.map_err(FilesError::Io)?;
                if file_type.is_symlink() || (!file_type.is_file() && !file_type.is_dir()) {
                    continue;
                }
                let path = entry.path();
                let relative_path = path
                    .strip_prefix(&self.root)
                    .map_err(|_| {
                        FilesError::UnsafeStorage("Files entry escaped its root.".to_owned())
                    })?
                    .to_path_buf();
                validate_relative_path(&relative_path)?;
                let metadata = entry.metadata().await.map_err(FilesError::Io)?;
                let kind = if file_type.is_dir() {
                    directories.push(path);
                    FileItemKind::Directory
                } else {
                    FileItemKind::File
                };
                items.push(ScannedItem {
                    relative_path,
                    kind,
                    size: if kind == FileItemKind::File {
                        metadata.len()
                    } else {
                        0
                    },
                    created_at: system_time_ms(
                        metadata.created().unwrap_or(SystemTime::UNIX_EPOCH),
                    ),
                    modified_at: system_time_ms(
                        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    ),
                });
            }
        }
        Ok(items)
    }

    async fn read_stored_items(&self) -> Result<Vec<StoredItem>, FilesError> {
        let mut entries = fs::read_dir(&self.metadata).await.map_err(FilesError::Io)?;
        let mut items = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(FilesError::Io)? {
            if entry
                .file_type()
                .await
                .map_err(FilesError::Io)?
                .is_symlink()
            {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let metadata = entry.metadata().await.map_err(FilesError::Io)?;
            if !metadata.is_file() {
                continue;
            }
            let item = serde_json::from_slice::<StoredItem>(
                &fs::read(&path).await.map_err(FilesError::Io)?,
            )
            .map_err(FilesError::Json)?;
            validate_item(&item.item)?;
            validate_relative_path(&item.relative_path)?;
            items.push(item);
        }
        Ok(items)
    }

    async fn write_stored_item(&self, item: &StoredItem) -> Result<(), FilesError> {
        validate_item(&item.item)?;
        validate_relative_path(&item.relative_path)?;
        let path = self.metadata_path(&item.item.id)?;
        reject_symlink_if_present(&path).await?;
        let temporary = self.metadata.join(format!(
            ".{}.{}.tmp",
            item.item.id,
            ITEM_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut guard = TemporaryPath::new(temporary.clone());
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await
            .map_err(FilesError::Io)?;
        file.write_all(&serde_json::to_vec(item).map_err(FilesError::Json)?)
            .await
            .map_err(FilesError::Io)?;
        file.sync_data().await.map_err(FilesError::Io)?;
        drop(file);
        fs::rename(&temporary, &path)
            .await
            .map_err(FilesError::Io)?;
        guard.disarm();
        Ok(())
    }

    fn metadata_path(&self, id: &str) -> Result<PathBuf, FilesError> {
        validate_item_identifier(id)?;
        Ok(self.metadata.join(format!("{id}.json")))
    }

    async fn safe_path(
        &self,
        relative_path: &Path,
        must_exist: bool,
    ) -> Result<PathBuf, FilesError> {
        validate_relative_path(relative_path)?;
        let path = self.root.join(relative_path);
        let parent = path.parent().ok_or_else(|| {
            FilesError::UnsafeStorage("Files item has no parent directory.".to_owned())
        })?;
        let canonical_parent = fs::canonicalize(parent).await.map_err(FilesError::Io)?;
        if !canonical_parent.starts_with(&self.root) {
            return Err(FilesError::UnsafeStorage(
                "Files item resolves outside the native Files root.".to_owned(),
            ));
        }
        if must_exist {
            let canonical = fs::canonicalize(&path).await.map_err(FilesError::Io)?;
            if !canonical.starts_with(&self.root) {
                return Err(FilesError::UnsafeStorage(
                    "Files item resolves outside the native Files root.".to_owned(),
                ));
            }
            return Ok(canonical);
        }
        Ok(path)
    }
}

#[derive(Debug)]
struct ScannedItem {
    relative_path: PathBuf,
    kind: FileItemKind,
    size: u64,
    created_at: u64,
    modified_at: u64,
}

pub struct StagedUpload {
    path: PathBuf,
    file: Option<File>,
    bytes_written: u64,
    committed: bool,
}

impl StagedUpload {
    pub async fn write(&mut self, chunk: Bytes) -> Result<u64, FilesError> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| FilesError::InvalidInput("Upload is already finalized.".to_owned()))?;
        file.write_all(&chunk).await.map_err(FilesError::Io)?;
        self.bytes_written = self.bytes_written.saturating_add(chunk.len() as u64);
        Ok(self.bytes_written)
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    async fn finish_writing(&mut self) -> Result<(), FilesError> {
        let Some(file) = self.file.take() else {
            return Err(FilesError::InvalidInput(
                "Upload is already finalized.".to_owned(),
            ));
        };
        file.sync_data().await.map_err(FilesError::Io)
    }
}

impl Drop for StagedUpload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct TemporaryPath {
    path: PathBuf,
    armed: bool,
}

impl TemporaryPath {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Error)]
pub enum FilesError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    ProviderUnavailable(String),
    #[error("{0}")]
    UnsafeStorage(String),
    #[error("Files metadata serialization failed: {0}")]
    Json(serde_json::Error),
    #[error("Files storage operation failed: {0}")]
    Io(std::io::Error),
}

pub fn validate_file_name(name: &str) -> Result<(), FilesError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > MAX_FILE_NAME_BYTES
        || name.contains('/')
        || name.contains('\0')
        || name.chars().any(char::is_control)
    {
        return Err(FilesError::InvalidInput(
            "File names must be a single non-empty Unicode path component up to 255 bytes."
                .to_owned(),
        ));
    }
    Ok(())
}

pub fn validate_content_type(content_type: &str) -> Result<(), FilesError> {
    let valid = !content_type.is_empty()
        && content_type.len() <= 127
        && content_type.split_once('/').is_some_and(|(kind, subtype)| {
            !kind.is_empty()
                && !subtype.is_empty()
                && kind.bytes().all(is_mime_token_byte)
                && subtype.bytes().all(is_mime_token_byte)
        });
    if !valid {
        return Err(FilesError::InvalidInput(
            "Content type must be a valid MIME type.".to_owned(),
        ));
    }
    Ok(())
}

fn is_mime_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte)
}

fn validate_item(item: &FileItem) -> Result<(), FilesError> {
    validate_item_identifier(&item.id)?;
    validate_parent_identifier(&item.parent_id)?;
    validate_file_name(&item.name)?;
    match item.kind {
        FileItemKind::File => {
            if let Some(content_type) = item.content_type.as_deref() {
                validate_content_type(content_type)?;
            }
            Ok(())
        }
        FileItemKind::Directory if item.content_type.is_some() || item.size != 0 => {
            Err(FilesError::UnsafeStorage(
                "Directory metadata contains file content fields.".to_owned(),
            ))
        }
        FileItemKind::Directory => Ok(()),
    }
}

fn validate_item_identifier(id: &str) -> Result<(), FilesError> {
    if id.len() != ITEM_ID_HEX_LENGTH || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(FilesError::InvalidInput(
            "Files item identifier is invalid.".to_owned(),
        ));
    }
    Ok(())
}

fn validate_parent_identifier(id: &str) -> Result<(), FilesError> {
    if id == ROOT_ITEM_IDENTIFIER {
        Ok(())
    } else {
        validate_item_identifier(id)
    }
}

fn validate_transfer_identifier(id: &str) -> Result<(), FilesError> {
    validate_item_identifier(id)
}

fn validate_relative_path(path: &Path) -> Result<(), FilesError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(FilesError::UnsafeStorage(
            "Files metadata contains an unsafe relative path.".to_owned(),
        ));
    }
    Ok(())
}

async fn resolve_local_storage_group(udid: &str) -> Result<PathBuf, FilesError> {
    if udid.is_empty() || udid.contains('\0') {
        return Err(FilesError::InvalidInput(
            "Simulator UDID is invalid.".to_owned(),
        ));
    }
    let output = timeout(
        SIMULATOR_HOME_TIMEOUT,
        Command::new("xcrun")
            .args(["simctl", "getenv", udid, "HOME"])
            .output(),
    )
    .await
    .map_err(|_| {
        FilesError::ProviderUnavailable(
            "Timed out locating the simulator Files storage.".to_owned(),
        )
    })?
    .map_err(FilesError::Io)?;
    if !output.status.success() {
        return Err(FilesError::ProviderUnavailable(
            "The simulator Files storage is unavailable until the device is booted.".to_owned(),
        ));
    }
    let home = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    find_local_storage_group(&home).await
}

async fn find_local_storage_group(simulator_home: &Path) -> Result<PathBuf, FilesError> {
    let app_groups = simulator_home.join("Containers/Shared/AppGroup");
    let mut entries = fs::read_dir(&app_groups).await.map_err(|error| {
        FilesError::ProviderUnavailable(format!(
            "Unable to inspect native Files app groups at {}: {error}",
            app_groups.display()
        ))
    })?;
    while let Some(entry) = entries.next_entry().await.map_err(FilesError::Io)? {
        if !entry.file_type().await.map_err(FilesError::Io)?.is_dir() {
            continue;
        }
        let container = entry.path();
        let metadata_path = container.join(".com.apple.mobile_container_manager.metadata.plist");
        let Ok(bytes) = fs::read(metadata_path).await else {
            continue;
        };
        let Ok(plist) = plist::Value::from_reader(Cursor::new(bytes)) else {
            continue;
        };
        let identifier = plist
            .as_dictionary()
            .and_then(|dictionary| dictionary.get("MCMMetadataIdentifier"))
            .and_then(plist::Value::as_string);
        if identifier == Some(LOCAL_STORAGE_APP_GROUP_IDENTIFIER) {
            return canonical_directory(&container).await;
        }
    }
    Err(FilesError::ProviderUnavailable(
        "The native On My iPhone storage has not been initialized for this simulator.".to_owned(),
    ))
}

async fn canonical_directory(path: &Path) -> Result<PathBuf, FilesError> {
    let metadata = fs::symlink_metadata(path).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FilesError::ProviderUnavailable(format!(
                "Files storage directory {} does not exist.",
                path.display()
            ))
        } else {
            FilesError::Io(error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FilesError::UnsafeStorage(format!(
            "Files storage directory {} is not a regular directory.",
            path.display()
        )));
    }
    fs::canonicalize(path).await.map_err(FilesError::Io)
}

async fn ensure_directory(path: &Path) -> Result<(), FilesError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(FilesError::UnsafeStorage(format!(
                "Files storage path {} is not a regular directory.",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).await.map_err(FilesError::Io)
        }
        Err(error) => Err(FilesError::Io(error)),
    }
}

async fn reject_symlink_if_present(path: &Path) -> Result<(), FilesError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(FilesError::UnsafeStorage(
            format!("Files storage path {} is a symbolic link.", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FilesError::Io(error)),
    }
}

async fn remove_regular_file_if_present(path: &Path) -> Result<(), FilesError> {
    match fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(FilesError::UnsafeStorage(format!(
                "Refusing to remove non-regular Files storage path {}.",
                path.display()
            )))
        }
        Ok(_) => fs::remove_file(path).await.map_err(FilesError::Io),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FilesError::Io(error)),
    }
}

async fn cleanup_stale_staging(staging: &Path) -> Result<(), FilesError> {
    let mut entries = fs::read_dir(staging).await.map_err(FilesError::Io)?;
    while let Some(entry) = entries.next_entry().await.map_err(FilesError::Io)? {
        if !entry.file_name().to_string_lossy().ends_with(".partial")
            || entry
                .file_type()
                .await
                .map_err(FilesError::Io)?
                .is_symlink()
        {
            continue;
        }
        let metadata = entry.metadata().await.map_err(FilesError::Io)?;
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_STAGING_AGE);
        if stale && metadata.is_file() {
            fs::remove_file(entry.path())
                .await
                .map_err(FilesError::Io)?;
        }
    }
    Ok(())
}

fn content_type_for_name(name: &str) -> Option<&'static str> {
    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => Some("application/pdf"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "heic" | "heif" => Some("image/heic"),
        "gif" => Some("image/gif"),
        "mp4" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "txt" => Some("text/plain"),
        "json" => Some("application/json"),
        "csv" => Some("text/csv"),
        _ => Some("application/octet-stream"),
    }
}

fn new_opaque_id(parent_id: &str, name: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(parent_id.as_bytes());
    digest.update([0]);
    digest.update(name.as_bytes());
    digest.update([0]);
    digest.update(now_nanos().to_le_bytes());
    digest.update(ITEM_SEQUENCE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    digest.update(std::process::id().to_le_bytes());
    hex::encode(digest.finalize())[..ITEM_ID_HEX_LENGTH].to_owned()
}

pub fn new_transfer_id() -> String {
    new_opaque_id("transfer", "upload")
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as u64
}

fn now_ms() -> u64 {
    system_time_ms(SystemTime::now())
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos()
}

#[cfg(test)]
mod tests {
    use super::{
        find_local_storage_group, validate_content_type, validate_file_name, FileItemKind,
        ProviderStore, ROOT_ITEM_IDENTIFIER,
    };
    use bytes::Bytes;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "simdeck-files-test-{}-{name}-{}",
            std::process::id(),
            TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    async fn store(name: &str) -> (PathBuf, ProviderStore) {
        let root = test_root(name);
        tokio::fs::create_dir_all(&root).await.unwrap();
        let store = ProviderStore::open_test_root(&root).await.unwrap();
        (root, store)
    }

    #[test]
    fn validates_unicode_names_and_mime_types() {
        assert!(validate_file_name("Décompte 🌱.pdf").is_ok());
        assert!(validate_file_name("../secret.pdf").is_err());
        assert!(validate_file_name("nested/file.pdf").is_err());
        assert!(validate_content_type("application/pdf").is_ok());
        assert!(validate_content_type("not a mime type").is_err());
    }

    #[tokio::test]
    async fn resolves_the_native_local_storage_app_group() {
        let home = test_root("resolve");
        let group = home.join("Containers/Shared/AppGroup/LOCAL");
        tokio::fs::create_dir_all(&group).await.unwrap();
        tokio::fs::write(
            group.join(".com.apple.mobile_container_manager.metadata.plist"),
            br#"<?xml version="1.0" encoding="UTF-8"?>
            <plist version="1.0"><dict><key>MCMMetadataIdentifier</key>
            <string>group.com.apple.FileProvider.LocalStorage</string></dict></plist>"#,
        )
        .await
        .unwrap();
        assert_eq!(
            find_local_storage_group(&home).await.unwrap(),
            tokio::fs::canonicalize(&group).await.unwrap()
        );
        tokio::fs::remove_dir_all(home).await.unwrap();
    }

    #[tokio::test]
    async fn creates_visible_uploads_renames_moves_and_deletes_trees() {
        let (root, store) = store("crud").await;
        let folder = store
            .create_directory(ROOT_ITEM_IDENTIFIER, "Statements")
            .await
            .unwrap();
        let mut upload = store
            .begin_upload("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .await
            .unwrap();
        upload.write(Bytes::from_static(b"pdf-data")).await.unwrap();
        let file = store
            .commit_upload(upload, &folder.id, "Décompte.pdf", "application/pdf")
            .await
            .unwrap();

        assert_eq!(file.size, 8);
        assert!(store.root.join("Statements/Décompte.pdf").is_file());
        assert_eq!(
            store.list(Some(&folder.id)).await.unwrap(),
            vec![file.clone()]
        );
        let renamed = store
            .update(&file.id, Some("Annual statement.pdf"), None)
            .await
            .unwrap();
        assert_eq!(renamed.version, 2);
        let (download, mut contents) = store.open_content(&file.id).await.unwrap();
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut contents, &mut bytes)
            .await
            .unwrap();
        assert_eq!(download.name, "Annual statement.pdf");
        assert_eq!(bytes, b"pdf-data");

        let deleted = store.delete(&folder.id).await.unwrap();
        assert_eq!(deleted.len(), 2);
        assert!(store.list(None).await.unwrap().is_empty());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn discovers_files_created_by_ios_and_keeps_roots_isolated() {
        let (first_root, first) = store("first").await;
        let (second_root, second) = store("second").await;
        tokio::fs::write(first.root.join("Native save.pdf"), b"native")
            .await
            .unwrap();
        let native = first.list(None).await.unwrap();
        assert_eq!(native.len(), 1);
        assert_eq!(native[0].name, "Native save.pdf");
        assert_eq!(native[0].content_type.as_deref(), Some("application/pdf"));
        assert!(second.list(None).await.unwrap().is_empty());
        tokio::fs::remove_dir_all(first_root).await.unwrap();
        tokio::fs::remove_dir_all(second_root).await.unwrap();
    }

    #[tokio::test]
    async fn cleans_interrupted_staging() {
        let (root, store) = store("staging").await;
        let mut upload = store
            .begin_upload("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .await
            .unwrap();
        upload.write(Bytes::from_static(b"partial")).await.unwrap();
        drop(upload);
        assert!(tokio::fs::read_dir(&store.staging)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ignores_symlinks_in_native_storage() {
        use std::os::unix::fs::symlink;

        let (root, store) = store("symlink").await;
        symlink("/etc/passwd", store.root.join("unsafe.txt")).unwrap();
        assert!(store.list(None).await.unwrap().is_empty());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn directories_cannot_be_moved_into_descendants() {
        let (root, store) = store("cycle").await;
        let parent = store
            .create_directory(ROOT_ITEM_IDENTIFIER, "Parent")
            .await
            .unwrap();
        let child = store.create_directory(&parent.id, "Child").await.unwrap();
        assert!(store
            .update(&parent.id, None, Some(&child.id))
            .await
            .is_err());
        assert_eq!(child.kind, FileItemKind::Directory);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
