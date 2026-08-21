use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Result, UpdateError, validate_relative_path};

pub const INSTALL_ROOT_MARKER: &str = ".deepx-install-root.json";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallRootMarker {
    format_version: u32,
    product: String,
    canonical_root: String,
    root_id: String,
}

pub fn write_install_root_marker(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    reject_reparse_point(root)?;
    let canonical = fs::canonicalize(root).map_err(|error| {
        UpdateError(format!(
            "canonicalize install root '{}': {error}",
            root.display()
        ))
    })?;
    let canonical_root = normalized_root_text(&canonical);
    let marker = InstallRootMarker {
        format_version: 1,
        product: "QAQ-Harness".into(),
        root_id: root_id(&canonical_root),
        canonical_root,
    };
    let marker_path = canonical.join(INSTALL_ROOT_MARKER);
    let temporary = canonical.join(".deepx-install-root.json.qaqh-new");
    fs::write(&temporary, serde_json::to_vec_pretty(&marker)?)?;
    if marker_path.exists() {
        fs::remove_file(&marker_path)?;
    }
    fs::rename(&temporary, &marker_path)?;
    Ok(())
}

pub fn verify_install_root(root: &Path) -> Result<PathBuf> {
    if root.as_os_str().is_empty() {
        return Err(UpdateError("install root is empty".into()));
    }
    let canonical = fs::canonicalize(root).map_err(|error| {
        UpdateError(format!(
            "canonicalize install root '{}': {error}",
            root.display()
        ))
    })?;
    if canonical.parent().is_none() {
        return Err(UpdateError(
            "filesystem root cannot be an install root".into(),
        ));
    }
    reject_reparse_point(&canonical)?;

    let marker_path = canonical.join(INSTALL_ROOT_MARKER);
    reject_reparse_point(&marker_path)?;
    let marker: InstallRootMarker =
        serde_json::from_slice(&fs::read(&marker_path).map_err(|error| {
            UpdateError(format!(
                "read install root marker '{}': {error}",
                marker_path.display()
            ))
        })?)
        .map_err(|error| UpdateError(format!("parse install root marker: {error}")))?;
    let canonical_root = normalized_root_text(&canonical);
    if marker.format_version != 1
        || marker.product != "QAQ-Harness"
        || marker.canonical_root != canonical_root
        || marker.root_id != root_id(&canonical_root)
    {
        return Err(UpdateError(format!(
            "install root marker does not match '{}'",
            canonical.display()
        )));
    }
    Ok(canonical)
}

pub fn safe_join_under_root(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative_path(relative)?;
    reject_reparse_point_if_present(root)?;

    let segments = relative.split('/').collect::<Vec<_>>();
    let mut current = root.to_path_buf();
    for segment in segments.iter().take(segments.len().saturating_sub(1)) {
        current.push(segment);
        reject_reparse_point_if_present(&current)?;
    }
    Ok(root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

pub fn is_reparse_or_symlink(path: &Path) -> std::io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return Ok(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0);
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

fn reject_reparse_point(path: &Path) -> Result<()> {
    if is_reparse_or_symlink(path).map_err(UpdateError::from)? {
        return Err(UpdateError(format!(
            "symbolic link or reparse point is not allowed: {}",
            path.display()
        )));
    }
    Ok(())
}

fn reject_reparse_point_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => reject_reparse_point(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn normalized_root_text(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    let value = if let Some(rest) = value.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else {
        value
            .strip_prefix("//?/")
            .map(str::to_owned)
            .unwrap_or(value)
    };
    if cfg!(windows) {
        value.trim_end_matches('/').to_ascii_lowercase()
    } else {
        value.trim_end_matches('/').to_string()
    }
}

fn root_id(canonical_root: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in canonical_root.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("root-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn marker_binds_to_its_canonical_directory() {
        let root = test_root("marker");
        let install = root.join("QAQ-Harness");
        fs::create_dir_all(&install).expect("create install root");
        write_install_root_marker(&install).expect("write marker");
        assert_eq!(
            verify_install_root(&install).expect("verify marker"),
            fs::canonicalize(&install).expect("canonical install root")
        );
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn copied_marker_cannot_authorize_another_directory() {
        let root = test_root("copied-marker");
        let source = root.join("QAQ-Harness-A");
        let destination = root.join("QAQ-Harness-B");
        fs::create_dir_all(&source).expect("create source install root");
        fs::create_dir_all(&destination).expect("create destination install root");
        write_install_root_marker(&source).expect("write source marker");
        fs::copy(
            source.join(INSTALL_ROOT_MARKER),
            destination.join(INSTALL_ROOT_MARKER),
        )
        .expect("copy marker");
        assert!(verify_install_root(&destination).is_err());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn safe_join_rejects_windows_separator_and_parent_escape() {
        let root = Path::new("QAQ-Harness");
        assert!(safe_join_under_root(root, "resources/app.asar").is_ok());
        assert!(safe_join_under_root(root, "../outside").is_err());
        assert!(safe_join_under_root(root, r"resources\..\outside").is_err());
    }

    fn test_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "qaqh-install-root-{label}-{}-{nonce}",
            std::process::id()
        ))
    }
}
