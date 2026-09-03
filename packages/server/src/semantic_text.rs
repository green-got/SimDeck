use plist::{Dictionary, Value as PlistValue};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout, Instant};

const ARTIFACT_BUILD_STRATEGY: &str = "iphonesimulator-sdk-v1";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone)]
struct Artifact {
    derived_data: PathBuf,
    xctestrun: PathBuf,
}

#[derive(Default)]
struct RunnerSession {
    child: Option<Child>,
    port: Option<u16>,
}

#[derive(Default)]
struct RunnerManager {
    artifact: Mutex<Option<Artifact>>,
    sessions: Mutex<HashMap<String, Arc<Mutex<RunnerSession>>>>,
}

#[derive(Deserialize)]
struct RunnerResponse {
    #[serde(default)]
    ok: bool,
    error: Option<String>,
}

static MANAGER: OnceLock<RunnerManager> = OnceLock::new();

fn manager() -> &'static RunnerManager {
    MANAGER.get_or_init(RunnerManager::default)
}

pub fn prewarm(udid: String) {
    tokio::spawn(async move {
        let session = session_for(&udid).await;
        let mut session = session.lock().await;
        if let Err(error) = ensure_ready(&udid, &mut session).await {
            tracing::debug!("Failed to prewarm semantic text for {udid}: {error}");
        }
    });
}

pub async fn shutdown_all() {
    let sessions = {
        let mut sessions = manager().sessions.lock().await;
        sessions
            .drain()
            .map(|(_, session)| session)
            .collect::<Vec<_>>()
    };
    for session in sessions {
        let mut session = session.lock().await;
        stop_session(&mut session).await;
    }
}

pub async fn type_text(udid: &str, bundle_id: &str, text: &str) -> Result<(), String> {
    let request = json!({
        "command": "typeText",
        "bundleId": bundle_id,
        "text": text,
    });
    let request_timeout = Duration::from_millis(
        (3_000_u64 + text.chars().count() as u64 * 100).clamp(10_000, 30_000),
    );
    run_command(udid, request, request_timeout, "XCTest could not type text").await
}

pub async fn type_key(
    udid: &str,
    bundle_id: &str,
    key: &str,
    modifiers: u32,
) -> Result<(), String> {
    if key.chars().count() != 1 {
        return Err("Semantic key input requires exactly one character".to_string());
    }
    let request = json!({
        "command": "typeKey",
        "bundleId": bundle_id,
        "key": key,
        "modifiers": modifiers,
    });
    run_command(
        udid,
        request,
        Duration::from_secs(10),
        "XCTest could not type key",
    )
    .await
}

async fn run_command(
    udid: &str,
    request: Value,
    request_timeout: Duration,
    fallback_error: &str,
) -> Result<(), String> {
    let session = session_for(udid).await;
    let mut session = session.lock().await;

    ensure_ready(udid, &mut session).await?;
    let port = session.port.ok_or("XCTest text runner has no port")?;
    match send_command(port, &request, request_timeout).await {
        Ok(response) if response.ok => Ok(()),
        Ok(response) => Err(response.error.unwrap_or_else(|| fallback_error.to_string())),
        Err(error) => {
            stop_session(&mut session).await;
            Err(error)
        }
    }
}

async fn session_for(udid: &str) -> Arc<Mutex<RunnerSession>> {
    let mut sessions = manager().sessions.lock().await;
    sessions
        .entry(udid.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(RunnerSession::default())))
        .clone()
}

async fn ensure_ready(udid: &str, session: &mut RunnerSession) -> Result<(), String> {
    if let (Some(child), Some(port)) = (session.child.as_mut(), session.port) {
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_none()
        {
            if let Ok(response) = send_command(
                port,
                &json!({ "command": "status" }),
                Duration::from_secs(1),
            )
            .await
            {
                if response.ok {
                    return Ok(());
                }
            }
        }
        stop_session(session).await;
    }

    let artifact = ensure_artifact().await?;
    let port = free_port()?;
    let configured = configure_xctestrun(&artifact.xctestrun, udid, port)?;
    let mut command = Command::new("xcodebuild");
    command
        .args([
            "test-without-building",
            "-only-testing",
            "SimDeckTextRunnerUITests/RunnerTests/testCommand",
            "-parallel-testing-enabled",
            "NO",
            "-test-timeouts-enabled",
            "NO",
            "-collect-test-diagnostics",
            "never",
            "-maximum-concurrent-test-simulator-destinations",
            "1",
            "-destination-timeout",
            "20",
            "-xctestrun",
        ])
        .arg(&configured)
        .arg("-derivedDataPath")
        .arg(&artifact.derived_data)
        .arg("-destination")
        .arg(format!("platform=iOS Simulator,id={udid}"))
        .env("SIMDECK_XCTEST_PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to start XCTest text runner: {error}"))?;
    if let Some(stderr) = child.stderr.take() {
        let log_udid = udid.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::trace!("XCTest text runner {log_udid}: {line}");
            }
        });
    }
    session.child = Some(child);
    session.port = Some(port);

    if let Err(error) = wait_for_ready(port).await {
        stop_session(session).await;
        return Err(error);
    }
    Ok(())
}

async fn stop_session(session: &mut RunnerSession) {
    if let Some(port) = session.port.take() {
        let _ = send_command(
            port,
            &json!({ "command": "shutdown" }),
            Duration::from_millis(500),
        )
        .await;
    }
    if let Some(mut child) = session.child.take() {
        let _ = child.start_kill();
        let _ = timeout(Duration::from_secs(2), child.wait()).await;
    }
}

async fn ensure_artifact() -> Result<Artifact, String> {
    let mut artifact = manager().artifact.lock().await;
    if let Some(artifact) = artifact.as_ref() {
        return Ok(artifact.clone());
    }
    let built = build_or_reuse_artifact().await?;
    *artifact = Some(built.clone());
    Ok(built)
}

async fn build_or_reuse_artifact() -> Result<Artifact, String> {
    let project = runner_project_path()?;
    let xcode_version = Command::new("xcodebuild")
        .arg("-version")
        .output()
        .await
        .map_err(|error| format!("Failed to inspect Xcode: {error}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&xcode_version.stdout);
    hasher.update(ARTIFACT_BUILD_STRATEGY);
    hash_directory(
        project.parent().ok_or("Invalid XCTest project path")?,
        &mut hasher,
    )?;
    let fingerprint = hex::encode(hasher.finalize());
    let cache_root = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is unavailable for the XCTest cache")?
        .join("Library/Caches/simdeck/xctest")
        .join(&fingerprint[..16]);
    if let Some(xctestrun) = find_xctestrun(&cache_root)? {
        return Ok(Artifact {
            derived_data: cache_root,
            xctestrun,
        });
    }
    fs::create_dir_all(&cache_root)
        .map_err(|error| format!("Failed to create XCTest cache: {error}"))?;

    let mut build = Command::new("xcodebuild");
    build
        .arg("-project")
        .arg(&project)
        .args([
            "-scheme",
            "SimDeckTextRunner",
            "-sdk",
            "iphonesimulator",
            "-derivedDataPath",
        ])
        .arg(&cache_root)
        .args([
            "build-for-testing",
            "CODE_SIGNING_ALLOWED=NO",
            "SUPPORTED_PLATFORMS=iphonesimulator",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = timeout(Duration::from_secs(180), build.output())
        .await
        .map_err(|_| "Timed out building the SimDeck XCTest text runner".to_string())?
        .map_err(|error| format!("Failed to build the SimDeck XCTest text runner: {error}"))?;
    if !output.status.success() {
        let _ = fs::remove_dir_all(&cache_root);
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if error.is_empty() {
            "Failed to build the SimDeck XCTest text runner".to_string()
        } else {
            error
        });
    }
    let xctestrun =
        find_xctestrun(&cache_root)?.ok_or("Xcode produced no .xctestrun artifact for SimDeck")?;
    Ok(Artifact {
        derived_data: cache_root,
        xctestrun,
    })
}

fn runner_project_path() -> Result<PathBuf, String> {
    runner_project_candidates(
        env::current_dir().ok().as_deref(),
        env::current_exe().ok().as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .into_iter()
    .find(|candidate| candidate.is_dir())
    .ok_or_else(|| "SimDeck XCTest text runner project is missing".to_string())
}

fn runner_project_candidates(
    current_directory: Option<&Path>,
    executable: Option<&Path>,
    manifest_directory: &Path,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(executable_directory) = executable.and_then(Path::parent) {
        candidates.push(executable_directory.join("text-runner/SimDeckTextRunner.xcodeproj"));
    }
    if let Some(current_directory) = current_directory {
        candidates.push(
            current_directory
                .join("packages/server/native/text-runner/SimDeckTextRunner.xcodeproj"),
        );
    }
    candidates.push(manifest_directory.join("native/text-runner/SimDeckTextRunner.xcodeproj"));
    candidates
}

fn hash_directory(root: &Path, hasher: &mut Sha256) -> Result<(), String> {
    fn visit(root: &Path, directory: &Path, hasher: &mut Sha256) -> Result<(), String> {
        let mut entries = fs::read_dir(directory)
            .map_err(|error| format!("Failed to read {}: {error}", directory.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, hasher)?;
            } else {
                hasher.update(
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .as_os_str()
                        .as_encoded_bytes(),
                );
                hasher.update(
                    fs::read(&path)
                        .map_err(|error| format!("Failed to read {}: {error}", path.display()))?,
                );
            }
        }
        Ok(())
    }
    visit(root, root, hasher)
}

fn find_xctestrun(root: &Path) -> Result<Option<PathBuf>, String> {
    if !root.exists() {
        return Ok(None);
    }
    let mut directories = vec![root.to_path_buf()];
    let mut matches = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("Failed to inspect XCTest cache: {error}"))?
        {
            let path = entry.map_err(|error| error.to_string())?.path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("xctestrun") {
                matches.push(path);
            }
        }
    }
    matches.sort_by(|left, right| {
        let left_canonical = left
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("SimDeckTextRunner_"));
        let right_canonical = right
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("SimDeckTextRunner_"));
        right_canonical.cmp(&left_canonical).then(left.cmp(right))
    });
    Ok(matches.into_iter().next())
}

fn configure_xctestrun(source: &Path, udid: &str, port: u16) -> Result<PathBuf, String> {
    let mut value = PlistValue::from_file(source)
        .map_err(|error| format!("Failed to read XCTest configuration: {error}"))?;
    configure_plist_value(&mut value, port);
    let output = source
        .parent()
        .ok_or("Invalid XCTest configuration path")?
        .join(format!("{udid}-{port}.xctestrun"));
    value
        .to_file_xml(&output)
        .map_err(|error| format!("Failed to configure XCTest runner: {error}"))?;
    Ok(output)
}

fn configure_plist_value(value: &mut PlistValue, port: u16) {
    match value {
        PlistValue::Dictionary(dictionary) => {
            if dictionary.contains_key("TestBundlePath") {
                configure_test_target(dictionary, port);
            }
            for value in dictionary.values_mut() {
                configure_plist_value(value, port);
            }
        }
        PlistValue::Array(values) => {
            for value in values {
                configure_plist_value(value, port);
            }
        }
        _ => {}
    }
}

fn configure_test_target(target: &mut Dictionary, port: u16) {
    for key in [
        "EnvironmentVariables",
        "UITestEnvironmentVariables",
        "UITargetAppEnvironmentVariables",
        "TestingEnvironmentVariables",
    ] {
        if !target.contains_key(key) {
            target.insert(key.to_string(), PlistValue::Dictionary(Dictionary::new()));
        }
        let environment = target.get_mut(key).expect("environment was inserted");
        if let PlistValue::Dictionary(environment) = environment {
            for value in environment.values_mut() {
                if let PlistValue::String(value) = value {
                    *value = value.replace(
                        "__PLATFORMS__/MacOSX.platform",
                        "__PLATFORMS__/iPhoneSimulator.platform",
                    );
                }
            }
            environment.insert(
                "SIMDECK_XCTEST_PORT".to_string(),
                PlistValue::String(port.to_string()),
            );
        }
    }
    target.insert(
        "PreferredScreenCaptureFormat".to_string(),
        PlistValue::String("screenshots".to_string()),
    );
    target.insert(
        "SystemAttachmentLifetime".to_string(),
        PlistValue::String("keepNever".to_string()),
    );
    target.insert(
        "UserAttachmentLifetime".to_string(),
        PlistValue::String("keepNever".to_string()),
    );
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("Failed to allocate XCTest port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("Failed to inspect XCTest port: {error}"))
}

async fn wait_for_ready(port: u16) -> Result<(), String> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut last_error = "Timed out starting the XCTest text runner".to_string();
    while Instant::now() < deadline {
        match send_command(
            port,
            &json!({ "command": "status" }),
            Duration::from_secs(1),
        )
        .await
        {
            Ok(response) if response.ok => return Ok(()),
            Ok(response) => {
                last_error = response
                    .error
                    .unwrap_or_else(|| "XCTest text runner is not ready".to_string());
            }
            Err(error) => last_error = error,
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(last_error)
}

async fn send_command(
    port: u16,
    body: &Value,
    command_timeout: Duration,
) -> Result<RunnerResponse, String> {
    timeout(command_timeout, async {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|error| error.to_string())?;
        let body = serde_json::to_vec(body).map_err(|error| error.to_string())?;
        let header = format!(
            "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        stream
            .write_all(&body)
            .await
            .map_err(|error| error.to_string())?;
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .map_err(|error| error.to_string())?;
        let boundary = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or("XCTest runner returned an invalid HTTP response")?;
        serde_json::from_slice(&response[boundary + 4..]).map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| "XCTest text command timed out".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xctestrun_configuration_sets_semantic_text_environment() {
        let mut target = Dictionary::new();
        target.insert(
            "TestBundlePath".to_string(),
            PlistValue::String("runner.xctest".to_string()),
        );
        let mut value = PlistValue::Dictionary(target);
        configure_plist_value(&mut value, 43123);
        let target = value.as_dictionary().unwrap();
        let environment = target
            .get("EnvironmentVariables")
            .and_then(PlistValue::as_dictionary)
            .unwrap();
        assert_eq!(
            environment
                .get("SIMDECK_XCTEST_PORT")
                .and_then(PlistValue::as_string),
            Some("43123")
        );
    }

    #[test]
    fn installed_text_runner_is_resolved_next_to_the_executable() {
        let candidates = runner_project_candidates(
            Some(Path::new("/workspace")),
            Some(Path::new("/release/build/simdeck-bin")),
            Path::new("/workspace/packages/server"),
        );

        assert_eq!(
            candidates.first().map(PathBuf::as_path),
            Some(Path::new(
                "/release/build/text-runner/SimDeckTextRunner.xcodeproj"
            ))
        );
    }
}
