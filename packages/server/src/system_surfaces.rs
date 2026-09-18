use crate::device_events::DeviceEventHub;
use crate::uikit_services::{
    application_service_details, application_services, service_bundle_identifier,
    UIKitApplicationService, UIKitApplicationServiceDetails,
};
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SURFACE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const DOCUMENT_PICKER_BUNDLE_IDENTIFIERS: &[&str] = &[
    "com.apple.DocumentsApp.DocumentsViewService",
    "com.apple.DocumentManager.DocumentPickerViewService",
    "com.apple.DocumentManager.Service",
];
const PHOTO_PICKER_BUNDLE_IDENTIFIERS: &[&str] = &[
    "com.apple.Photos.PhotosUIService",
    "com.apple.mobileslideshow.photo-picker",
    "com.apple.mobileslideshow.photospicker",
];

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemSurface {
    pub kind: String,
    pub process_identifier: i64,
    pub session_id: String,
}

#[derive(Clone, Default)]
pub struct SystemSurfaceRegistry {
    inner: Arc<Mutex<SystemSurfaceRegistryState>>,
}

#[derive(Default)]
struct SystemSurfaceRegistryState {
    current: HashMap<String, SystemSurface>,
    clients: HashMap<String, usize>,
    monitoring: HashSet<String>,
}

/// Keeps the picker poll loop alive for one control connection. Polling costs a
/// `simctl spawn` per tick per device, so it only runs while a client is attached.
pub struct SystemSurfaceMonitor {
    registry: SystemSurfaceRegistry,
    udid: String,
}

impl Drop for SystemSurfaceMonitor {
    fn drop(&mut self) {
        self.registry.release(&self.udid);
    }
}

impl SystemSurfaceRegistry {
    pub fn current(&self, udid: &str) -> Option<SystemSurface> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .current
            .get(udid)
            .cloned()
    }

    pub async fn probe(
        &self,
        udid: &str,
        events: &DeviceEventHub,
    ) -> Result<Option<SystemSurface>, String> {
        let surface = detect_system_surface(udid).await?;
        self.set(udid, surface.clone(), events);
        Ok(surface)
    }

    pub fn monitor(&self, udid: String, events: DeviceEventHub) -> SystemSurfaceMonitor {
        let should_start = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *state.clients.entry(udid.clone()).or_default() += 1;
            state.monitoring.insert(udid.clone())
        };
        if should_start {
            let registry = self.clone();
            let udid = udid.clone();
            tokio::spawn(async move {
                while registry.keep_monitoring(&udid) {
                    if let Err(error) = registry.probe(&udid, &events).await {
                        tracing::debug!(
                            "Unable to inspect UIKit system surface for {udid}: {error}"
                        );
                    }
                    tokio::time::sleep(SURFACE_POLL_INTERVAL).await;
                }
            });
        }
        SystemSurfaceMonitor {
            registry: self.clone(),
            udid,
        }
    }

    #[cfg(test)]
    pub fn is_monitoring(&self, udid: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .monitoring
            .contains(udid)
    }

    fn release(&self, udid: &str) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = state.clients.get_mut(udid) {
            *count -= 1;
            if *count == 0 {
                state.clients.remove(udid);
            }
        }
    }

    // Decides under the same lock as `monitor` whether the loop stays alive, so a
    // client reconnecting during the last tick cannot start a second loop.
    fn keep_monitoring(&self, udid: &str) -> bool {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.clients.contains_key(udid) {
            return true;
        }
        state.monitoring.remove(udid);
        false
    }

    pub fn clear(&self, udid: &str, events: &DeviceEventHub) {
        self.set(udid, None, events);
    }

    fn set(&self, udid: &str, surface: Option<SystemSurface>, events: &DeviceEventHub) {
        let changed = {
            let mut state = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = state.current.get(udid).cloned();
            if previous == surface {
                false
            } else {
                match &surface {
                    Some(surface) => {
                        state.current.insert(udid.to_owned(), surface.clone());
                    }
                    None => {
                        state.current.remove(udid);
                    }
                }
                true
            }
        };
        if changed {
            events.publish(
                udid,
                json!({
                    "type": "system-surface.changed",
                    "udid": udid,
                    "systemSurface": surface,
                }),
            );
        }
    }
}

pub fn is_document_picker_service(details: &UIKitApplicationServiceDetails) -> bool {
    details.spawn_role.contains("ui focal")
        && document_picker_identifier(details.bundle_identifier.as_deref())
            .or_else(|| {
                document_picker_identifier(service_bundle_identifier(&details.service_name))
            })
            .is_some()
}

pub fn is_photo_picker_service(details: &UIKitApplicationServiceDetails) -> bool {
    details.spawn_role.contains("ui focal")
        && photo_picker_identifier(details.bundle_identifier.as_deref())
            .or_else(|| photo_picker_identifier(service_bundle_identifier(&details.service_name)))
            .is_some()
}

async fn detect_system_surface(udid: &str) -> Result<Option<SystemSurface>, String> {
    let services = application_services(udid).await?;
    let mut best: Option<UIKitApplicationServiceDetails> = None;
    for service in services
        .iter()
        .filter(|service| service_might_be_system_surface(service))
    {
        let Some(details) = application_service_details(udid, service).await? else {
            continue;
        };
        if !is_document_picker_service(&details) && !is_photo_picker_service(&details) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|current| details.active_count > current.active_count)
        {
            best = Some(details);
        }
    }

    Ok(best.map(|details| SystemSurface {
        kind: if is_photo_picker_service(&details) {
            "photoPicker"
        } else {
            "documentPicker"
        }
        .to_owned(),
        process_identifier: details.process_identifier,
        session_id: surface_session_id(udid, &details),
    }))
}

fn service_might_be_system_surface(service: &UIKitApplicationService) -> bool {
    let identifier = service_bundle_identifier(&service.service_name);
    document_picker_identifier(identifier).is_some()
        || photo_picker_identifier(identifier).is_some()
}

fn photo_picker_identifier(identifier: Option<&str>) -> Option<&str> {
    let identifier = identifier?;
    PHOTO_PICKER_BUNDLE_IDENTIFIERS
        .contains(&identifier)
        .then_some(identifier)
}

fn document_picker_identifier(identifier: Option<&str>) -> Option<&str> {
    let identifier = identifier?;
    DOCUMENT_PICKER_BUNDLE_IDENTIFIERS
        .contains(&identifier)
        .then_some(identifier)
}

fn surface_session_id(udid: &str, details: &UIKitApplicationServiceDetails) -> String {
    let mut hasher = DefaultHasher::new();
    udid.hash(&mut hasher);
    details.service_name.hash(&mut hasher);
    details.process_identifier.hash(&mut hasher);
    details.active_count.hash(&mut hasher);
    format!("surface-{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::{
        is_document_picker_service, is_photo_picker_service, surface_session_id,
        SystemSurfaceRegistry,
    };
    use crate::device_events::DeviceEventHub;
    use crate::uikit_services::{
        application_service_details_from_output, parse_application_service_line,
    };
    use std::time::Duration;

    const IOS_18_DOCUMENT_PICKER_LIST_TRACE: &str =
        "  54831 - UIKitApplication:com.apple.DocumentsApp.DocumentsViewService[beef][rb-legacy]";
    const IOS_18_DOCUMENT_PICKER_DETAIL_TRACE: &str = r#"
        active count = 2
        state = running
        program = /System/Applications/Files.app/PlugIns/DocumentsViewService.appex/DocumentsViewService
        bundle id = com.apple.DocumentsApp.DocumentsViewService
        spawn role = ui focal (1)
        pid = 54831
    "#;
    const IOS_18_PHOTO_PICKER_LIST_TRACE: &str =
        "  54833 - UIKitApplication:com.apple.Photos.PhotosUIService[cafe][rb-legacy]";
    const IOS_18_PHOTO_PICKER_DETAIL_TRACE: &str = r#"
        active count = 1
        state = running
        program = /Applications/PhotosUIService.app/PhotosUIService
        bundle id = com.apple.Photos.PhotosUIService
        spawn role = ui focal (1)
        pid = 54833
    "#;

    #[tokio::test]
    async fn monitor_stops_after_last_client_releases() {
        let registry = SystemSurfaceRegistry::default();
        let events = DeviceEventHub::default();
        let first = registry.monitor("device-a".to_owned(), events.clone());
        let second = registry.monitor("device-a".to_owned(), events.clone());
        assert!(registry.is_monitoring("device-a"));

        drop(first);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(registry.is_monitoring("device-a"));

        drop(second);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while registry.is_monitoring("device-a") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "monitor kept polling"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let _third = registry.monitor("device-a".to_owned(), events);
        assert!(registry.is_monitoring("device-a"));
    }

    #[test]
    fn recognizes_focal_documents_view_service_trace() {
        let service = parse_application_service_line(IOS_18_DOCUMENT_PICKER_LIST_TRACE).unwrap();
        let details =
            application_service_details_from_output(&service, IOS_18_DOCUMENT_PICKER_DETAIL_TRACE)
                .unwrap();

        assert!(is_document_picker_service(&details));
        assert_eq!(
            surface_session_id("device-a", &details),
            surface_session_id("device-a", &details)
        );
        assert_ne!(
            surface_session_id("device-a", &details),
            surface_session_id("device-b", &details)
        );
    }

    #[test]
    fn rejects_nonfocal_and_unknown_services() {
        let service = parse_application_service_line(IOS_18_DOCUMENT_PICKER_LIST_TRACE).unwrap();
        let background = application_service_details_from_output(
            &service,
            &IOS_18_DOCUMENT_PICKER_DETAIL_TRACE.replace("ui focal (1)", "non-ui (3)"),
        )
        .unwrap();
        assert!(!is_document_picker_service(&background));

        let unknown_service = parse_application_service_line(
            "  54832 - UIKitApplication:com.apple.SomeViewService[beef][rb-legacy]",
        )
        .unwrap();
        let unknown = application_service_details_from_output(
            &unknown_service,
            &IOS_18_DOCUMENT_PICKER_DETAIL_TRACE.replace(
                "com.apple.DocumentsApp.DocumentsViewService",
                "com.apple.SomeViewService",
            ),
        )
        .unwrap();
        assert!(!is_document_picker_service(&unknown));
    }

    #[test]
    fn recognizes_focal_photo_picker_service_trace() {
        let service = parse_application_service_line(IOS_18_PHOTO_PICKER_LIST_TRACE).unwrap();
        let details =
            application_service_details_from_output(&service, IOS_18_PHOTO_PICKER_DETAIL_TRACE)
                .unwrap();
        assert!(is_photo_picker_service(&details));

        let background = application_service_details_from_output(
            &service,
            &IOS_18_PHOTO_PICKER_DETAIL_TRACE.replace("ui focal (1)", "non-ui (3)"),
        )
        .unwrap();
        assert!(!is_photo_picker_service(&background));
    }
}
