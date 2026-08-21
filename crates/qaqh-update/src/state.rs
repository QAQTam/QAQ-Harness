use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::{
    BundleManifest, CatalogComponent, ComponentHealth, ComponentState, InstalledState, Result,
    UpdateError,
};

pub fn load_installed_state(path: &Path, installation_id: &str) -> Result<Option<InstalledState>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(UpdateError(format!(
                "read installed state '{}': {error}",
                path.display()
            )));
        }
    };
    let value: Value = serde_json::from_slice(&bytes)?;
    let state = migrate_installed_state(value, installation_id)?;
    Ok(Some(state))
}

pub fn migrate_installed_state(value: Value, installation_id: &str) -> Result<InstalledState> {
    if value.get("formatVersion").and_then(Value::as_u64) == Some(2) {
        return serde_json::from_value(value)
            .map_err(|error| UpdateError(format!("parse installed state v2: {error}")));
    }

    let object = value
        .as_object()
        .ok_or_else(|| UpdateError("legacy installed state must be an object".into()))?;
    let app_version = string(object, "appVersion").unwrap_or_default();
    let full = string(object, "fullBuild");
    let frontend = string(object, "frontendBuild").or_else(|| full.clone());
    let backend = string(object, "backendBuild").or_else(|| full.clone());
    if frontend.is_none() && backend.is_none() {
        return Err(UpdateError(
            "legacy installed state contains no component builds".into(),
        ));
    }

    let mut components = BTreeMap::new();
    if let Some(current) = frontend {
        components.insert(
            "frontend".into(),
            legacy_component(
                current,
                string(object, "previousFrontendBuild"),
                app_version.clone(),
            ),
        );
    }
    if let Some(current) = backend {
        components.insert(
            "backend".into(),
            legacy_component(
                current,
                string(object, "previousBackendBuild"),
                app_version.clone(),
            ),
        );
    }
    if let Some(current) = full {
        components.insert(
            "runtime".into(),
            legacy_component(current, None, app_version.clone()),
        );
    }

    Ok(InstalledState {
        format_version: 2,
        installation_id: installation_id.into(),
        release_id: "legacy".into(),
        channel: "local".into(),
        components,
        last_committed_operation: None,
    })
}

pub fn write_installed_state(path: &Path, state: &InstalledState) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| UpdateError(format!("state path has no parent: {}", path.display())))?;
    fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(state)?;
    let temp = parent.join(".install-state.json.qaqh-new");
    fs::write(&temp, bytes)?;
    if path.exists() {
        retry_io(|| fs::remove_file(path))?;
    }
    retry_io(|| fs::rename(&temp, path))?;
    Ok(())
}

pub fn installation_id_for_path(path: &Path) -> String {
    let normalized = path.to_string_lossy().to_ascii_lowercase();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in normalized.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("install-{hash:016x}")
}

pub fn commit_bundle_state(
    target: &Path,
    manifest: &BundleManifest,
    operation_id: &str,
) -> Result<InstalledState> {
    fs::create_dir_all(target)?;
    let state_path = target.join("install-state.json");
    let installation_id = installation_id_for_path(target);
    let mut state =
        load_installed_state(&state_path, &installation_id)?.unwrap_or_else(|| InstalledState {
            format_version: 2,
            installation_id,
            release_id: String::new(),
            channel: String::new(),
            components: BTreeMap::new(),
            last_committed_operation: None,
        });
    for (name, component) in bundle_components(manifest) {
        let previous = state.components.get(&name).and_then(|installed| {
            (installed.current != component.build_id).then(|| installed.current.clone())
        });
        state.components.insert(
            name,
            ComponentState {
                current: component.build_id,
                previous,
                version: component.version,
                protocol: component.control_protocol,
                health: ComponentHealth::Healthy,
            },
        );
    }
    state.format_version = 2;
    state.release_id = if manifest.release_id.is_empty() {
        manifest.build_id.clone()
    } else {
        manifest.release_id.clone()
    };
    state.channel = if manifest.channel.is_empty() {
        "local".into()
    } else {
        manifest.channel.clone()
    };
    state.last_committed_operation = Some(operation_id.into());
    write_installed_state(&state_path, &state)?;
    Ok(state)
}

fn bundle_components(manifest: &BundleManifest) -> BTreeMap<String, CatalogComponent> {
    if !manifest.components.is_empty() {
        return manifest.components.clone();
    }
    let component = CatalogComponent {
        build_id: manifest.build_id.clone(),
        version: manifest.app_version.clone(),
        control_protocol: None,
    };
    if manifest.kind == "full" {
        return ["runtime", "frontend", "backend"]
            .into_iter()
            .map(|name| (name.into(), component.clone()))
            .collect();
    }
    [(manifest.kind.clone(), component)].into_iter().collect()
}

fn retry_io<T>(mut operation: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut last_error = None;
    for attempt in 0..20 {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt < 19 {
                    thread::sleep(Duration::from_millis(150));
                }
            }
        }
    }
    Err(last_error.expect("retry loop always records an error"))
}

fn legacy_component(current: String, previous: Option<String>, version: String) -> ComponentState {
    ComponentState {
        current,
        previous,
        version,
        protocol: None,
        health: ComponentHealth::Unknown,
    }
}

fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn migrates_flat_installer_state() {
        let migrated = migrate_installed_state(
            json!({
                "formatVersion": 1,
                "appVersion": "0.9.0",
                "fullBuild": "full-1",
                "frontendBuild": "frontend-2",
                "backendBuild": "backend-3",
                "previousBackendBuild": "backend-2"
            }),
            "installation",
        )
        .unwrap();
        assert_eq!(migrated.format_version, 2);
        assert_eq!(migrated.installation_id, "installation");
        assert_eq!(migrated.components["frontend"].current, "frontend-2");
        assert_eq!(
            migrated.components["backend"].previous.as_deref(),
            Some("backend-2")
        );
    }

    #[test]
    fn installation_id_is_stable_and_case_insensitive_on_windows_paths() {
        assert_eq!(
            installation_id_for_path(Path::new(r"C:\Users\Test\App")),
            installation_id_for_path(Path::new(r"c:\users\test\app"))
        );
    }
}
