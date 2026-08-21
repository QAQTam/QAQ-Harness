use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::{
    BundleFile, BundleManifest, Result, UpdateError, commit_bundle_state, parse_bundle_manifest,
    safe_join_under_root, sha256_reader,
};

pub fn apply_bundle_zip(path: &Path, target: &Path, operation_id: &str) -> Result<BundleManifest> {
    let file = fs::File::open(path)
        .map_err(|error| UpdateError(format!("open bundle '{}': {error}", path.display())))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| UpdateError(format!("open bundle ZIP '{}': {error}", path.display())))?;
    let manifest = read_manifest_from_archive(&mut archive)?;
    if manifest.requires_full_install && !target.join("QAQ-Harness.exe").is_file() {
        return Err(UpdateError(format!(
            "{} bundle requires an existing QAQ-Harness installation at {}",
            manifest.kind,
            target.display()
        )));
    }

    for expected in &manifest.files {
        let mut entry = archive.by_name(&expected.source).map_err(|error| {
            UpdateError(format!(
                "bundle is missing manifest file '{}': {error}",
                expected.source
            ))
        })?;
        if entry.is_dir() {
            return Err(UpdateError(format!(
                "manifest file is a directory: {}",
                expected.source
            )));
        }
        install_file(&mut entry, target, expected, manifest.kind != "full")?;
    }
    commit_bundle_state(target, &manifest, operation_id)?;
    Ok(manifest)
}

pub fn rollback_bundle_zip(path: &Path, target: &Path) -> Result<BundleManifest> {
    let file = fs::File::open(path)
        .map_err(|error| UpdateError(format!("open bundle '{}': {error}", path.display())))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| UpdateError(format!("open bundle ZIP '{}': {error}", path.display())))?;
    let manifest = read_manifest_from_archive(&mut archive)?;
    if manifest.kind == "full" {
        return Err(UpdateError(
            "full bundle rollback requires a versioned runtime layout".into(),
        ));
    }
    for expected in manifest.files.iter().rev() {
        let installed = safe_join_under_root(target, &expected.target)?;
        let parent = installed
            .parent()
            .ok_or_else(|| UpdateError(format!("target has no parent: {}", installed.display())))?;
        let file_name = installed
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| UpdateError(format!("invalid target name: {}", installed.display())))?;
        let previous = parent.join(format!("{file_name}.previous"));
        if previous.exists() {
            if installed.exists() {
                clear_readonly(&installed);
                retry_io(|| fs::remove_file(&installed))?;
            }
            retry_io(|| fs::rename(&previous, &installed))?;
        } else if installed.exists() {
            let (size, sha256) = sha256_reader(fs::File::open(&installed)?)?;
            if size == expected.size && sha256.eq_ignore_ascii_case(&expected.sha256) {
                clear_readonly(&installed);
                retry_io(|| fs::remove_file(&installed))?;
            }
        }
    }
    Ok(manifest)
}

pub fn read_bundle_manifest_zip(path: &Path) -> Result<BundleManifest> {
    let file = fs::File::open(path)
        .map_err(|error| UpdateError(format!("open bundle '{}': {error}", path.display())))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| UpdateError(format!("open bundle ZIP '{}': {error}", path.display())))?;
    read_manifest_from_archive(&mut archive)
}

fn read_manifest_from_archive<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Result<BundleManifest> {
    let mut entry = archive
        .by_name("bundle.json")
        .map_err(|error| UpdateError(format!("bundle is missing bundle.json: {error}")))?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    parse_bundle_manifest(&bytes)
}

fn install_file(
    mut source: impl Read,
    root: &Path,
    expected: &BundleFile,
    preserve_previous: bool,
) -> Result<()> {
    let target = safe_join_under_root(root, &expected.target)?;
    let parent = target
        .parent()
        .ok_or_else(|| UpdateError(format!("target has no parent: {}", target.display())))?;
    fs::create_dir_all(parent)?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| UpdateError(format!("invalid target file name: {}", target.display())))?;
    let temporary = parent.join(format!(".{file_name}.qaqh-new-{}", std::process::id()));
    if temporary.exists() {
        retry_io(|| fs::remove_file(&temporary))?;
    }

    let mut output = fs::File::create(&temporary)?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        copied += read as u64;
    }
    output.sync_all()?;
    drop(output);
    let sha256 = hex::encode(hasher.finalize());
    if copied != expected.size || !sha256.eq_ignore_ascii_case(&expected.sha256) {
        let _ = fs::remove_file(&temporary);
        return Err(UpdateError(format!(
            "bundle file verification failed for {}: expected {} bytes/{}, got {} bytes/{}",
            expected.target, expected.size, expected.sha256, copied, sha256
        )));
    }

    let backup = parent.join(format!("{file_name}.previous"));
    if target.exists() {
        clear_readonly(&target);
        if preserve_previous {
            if backup.exists() {
                clear_readonly(&backup);
                retry_io(|| fs::remove_file(&backup))?;
            }
            retry_io(|| fs::rename(&target, &backup))?;
        } else {
            retry_io(|| fs::remove_file(&target))?;
        }
    }
    if let Err(error) = retry_io(|| fs::rename(&temporary, &target)) {
        if preserve_previous && backup.exists() && !target.exists() {
            let _ = retry_io(|| fs::rename(&backup, &target));
        }
        let _ = fs::remove_file(&temporary);
        return Err(UpdateError(format!(
            "activate new file '{}': {error}",
            target.display()
        )));
    }
    Ok(())
}

fn clear_readonly(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::{
        ComponentHealth, ComponentState, InstalledState, installation_id_for_path,
        load_installed_state, write_installed_state,
    };

    #[test]
    fn component_bundle_preserves_previous_and_commits_v2_state() {
        let root = test_root("apply");
        let target = root.join("QAQ-Harness");
        fs::create_dir_all(target.join("resources")).unwrap();
        fs::write(target.join("QAQ-Harness.exe"), b"launcher").unwrap();
        fs::write(target.join("resources/app.asar"), b"old").unwrap();
        let installation_id = installation_id_for_path(&target);
        write_installed_state(
            &target.join("install-state.json"),
            &InstalledState {
                format_version: 2,
                installation_id: installation_id.clone(),
                release_id: "release-old".into(),
                channel: "test".into(),
                components: BTreeMap::from([(
                    "frontend".into(),
                    ComponentState {
                        current: "frontend-old".into(),
                        previous: None,
                        version: "0.8.0".into(),
                        protocol: Some(1),
                        health: ComponentHealth::Healthy,
                    },
                )]),
                last_committed_operation: None,
            },
        )
        .unwrap();
        let bundle = root.join("frontend.zip");
        write_bundle(&bundle, b"new");

        apply_bundle_zip(&bundle, &target, "op-test").unwrap();

        assert_eq!(fs::read(target.join("resources/app.asar")).unwrap(), b"new");
        assert_eq!(
            fs::read(target.join("resources/app.asar.previous")).unwrap(),
            b"old"
        );
        let state = load_installed_state(&target.join("install-state.json"), &installation_id)
            .unwrap()
            .unwrap();
        assert_eq!(state.format_version, 2);
        assert_eq!(state.components["frontend"].current, "frontend-new");
        assert_eq!(
            state.components["frontend"].previous.as_deref(),
            Some("frontend-old")
        );
        assert_eq!(state.last_committed_operation.as_deref(), Some("op-test"));
        rollback_bundle_zip(&bundle, &target).unwrap();
        assert_eq!(fs::read(target.join("resources/app.asar")).unwrap(), b"old");
        assert!(!target.join("resources/app.asar.previous").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delta_bundle_applies_changed_files_only_and_rolls_back() {
        // 增量包（strategy=file-level-delta 产物）：files[] 只列变化文件。
        // apply_bundle_zip 是覆盖式——未列文件完全不动，增量天然成立。
        let root = test_root("delta-apply");
        let target = root.join("QAQ-Harness");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("QAQ-Harness.exe"), b"launcher").unwrap();
        fs::write(target.join("a.txt"), b"same").unwrap(); // 未变化（不在增量）
        fs::write(target.join("b.txt"), b"old").unwrap(); // 变化（在增量）
        // 初始安装态：runtime 当前 = runtime-old（1.0.0 检查点安装）。
        let installation_id = installation_id_for_path(&target);
        write_installed_state(
            &target.join("install-state.json"),
            &InstalledState {
                format_version: 2,
                installation_id: installation_id.clone(),
                release_id: "release-old".into(),
                channel: "test".into(),
                components: BTreeMap::from([(
                    "runtime".into(),
                    ComponentState {
                        current: "runtime-old".into(),
                        previous: None,
                        version: "1.0.0".into(),
                        protocol: Some(1),
                        health: ComponentHealth::Healthy,
                    },
                )]),
                last_committed_operation: None,
            },
        )
        .unwrap();
        let bundle = root.join("delta.zip");
        write_delta_bundle(&bundle, b"new");

        apply_bundle_zip(&bundle, &target, "op-delta").unwrap();

        // 变化文件替换 + 旧版本保留 .previous；未变化文件保持不动。
        assert_eq!(fs::read(target.join("b.txt")).unwrap(), b"new");
        assert_eq!(fs::read(target.join("b.txt.previous")).unwrap(), b"old");
        assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"same");
        assert_eq!(
            fs::read(target.join("QAQ-Harness.exe")).unwrap(),
            b"launcher"
        );
        // state 推进：runtime current=new，previous=old。
        let state = load_installed_state(&target.join("install-state.json"), &installation_id)
            .unwrap()
            .unwrap();
        assert_eq!(state.components["runtime"].current, "runtime-new");
        assert_eq!(
            state.components["runtime"].previous.as_deref(),
            Some("runtime-old")
        );

        // 回滚：变化文件复原，未变化文件仍不动。
        rollback_bundle_zip(&bundle, &target).unwrap();
        assert_eq!(fs::read(target.join("b.txt")).unwrap(), b"old");
        assert!(!target.join("b.txt.previous").exists());
        assert_eq!(fs::read(target.join("a.txt")).unwrap(), b"same");
        fs::remove_dir_all(root).unwrap();
    }

    fn write_delta_bundle(path: &Path, payload: &[u8]) {
        let digest = hex::encode(Sha256::digest(payload));
        let manifest = json!({
            "formatVersion": 1,
            "kind": "runtime",
            "buildId": "runtime-new",
            "appVersion": "1.0.1",
            "releaseId": "release-delta",
            "channel": "test",
            "components": {
                "runtime": {
                    "buildId": "runtime-new",
                    "version": "1.0.1",
                    "controlProtocol": 1
                }
            },
            "requiresFullInstall": false,
            "files": [{
                "source": "files/b.txt",
                "target": "b.txt",
                "size": payload.len(),
                "sha256": digest
            }]
        });
        let file = fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("bundle.json", SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        archive
            .start_file("files/b.txt", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(payload).unwrap();
        archive.finish().unwrap();
    }

    fn write_bundle(path: &Path, payload: &[u8]) {
        let digest = hex::encode(Sha256::digest(payload));
        let manifest = json!({
            "formatVersion": 1,
            "kind": "frontend",
            "buildId": "bundle-new",
            "appVersion": "0.9.0",
            "releaseId": "release-new",
            "channel": "test",
            "components": {
                "frontend": {
                    "buildId": "frontend-new",
                    "version": "0.9.0",
                    "controlProtocol": 1
                }
            },
            "requiresFullInstall": true,
            "files": [{
                "source": "files/resources/app.asar",
                "target": "resources/app.asar",
                "size": payload.len(),
                "sha256": digest
            }]
        });
        let file = fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("bundle.json", SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        archive
            .start_file("files/resources/app.asar", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(payload).unwrap();
        archive.finish().unwrap();
    }

    fn test_root(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "qaqh-update-{label}-{}-{nonce}",
            std::process::id()
        ))
    }
}
