use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DATA_ROOT_MARKER: &str = ".deepx-data-root.json";

/// 对外产品版本号（User-Agent 使用）：不带 rc/预发布后缀，正式发布时手工 bump。
/// 与 cargo 包版本（`CARGO_PKG_VERSION`，如 `1.0.0-rc.6`）解耦——UA 里暴露的是
/// 面向服务的稳定版本标识，而非内部打包版本。
macro_rules! qaqh_ua_version {
    () => {
        "1.0.0"
    };
}

/// 对外产品版本号（与 `QAQH_USER_AGENT` 同源，见上）。
pub const QAQH_UA_VERSION: &str = qaqh_ua_version!();

/// 统一产品 User-Agent：`qaqharness/1.0.0/`。
///
/// 用于所有对外 API 请求（gate chat/responses、provider 模型目录拉取），
/// 便于服务端识别客户端与版本。
/// 网页抓取（`qaqh-workspace::web`）是独立的浏览器伪装 UA，不使用本常量。
pub const QAQH_USER_AGENT: &str = concat!("qaqharness/", qaqh_ua_version!(), "/");

/// data-root marker（`<data>/.deepx-data-root.json`）— 权威契约来自后端 `qaqh_types::platform`。
/// 前端禁止自建 FNV 公式；统一通过 `normalized_path_text` / `data_root_id` / `DataRootMarker` 复用。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataRootMarker {
    pub format_version: u32,
    pub product: String,
    pub canonical_root: String,
    pub owner_home: String,
    pub root_id: String,
}

/// Cross-platform home directory.
/// - Windows: `USERPROFILE`
/// - Unix: `HOME`
pub fn home_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default()
    } else {
        std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
    }
}

/// qaqh data directory (config, sessions, plans).
/// - Windows: `%USERPROFILE%\.deepx`
/// - Unix: `$XDG_CONFIG_HOME/qaqh` or `$HOME/.config/qaqh`
pub fn data_dir() -> PathBuf {
    // `QAQH_DATA_DIR` (full data root, e.g. `F:\QAQ-Harness\.deepx-test-home\.deepx`)
    // overrides when set — used by test harnesses and multi-instance shells.
    // The daemon resolves paths through this same function, so shell and
    // daemon stay on the same data root.
    if let Ok(dir) = std::env::var("QAQH_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if cfg!(windows) {
        home_dir().join(".deepx")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".config"))
            .join("qaqh")
    }
}

/// Create or verify the QAQ-Harness-owned user data root.
///
/// The marker binds the directory to both its canonical path and the current
/// user's canonical home. Destructive maintenance must call `verify_data_root`
/// and must never infer ownership from the directory name alone.
///
/// Legacy `DeepX` markers are migrated in place only when every identity field
/// matches the current user/path; anything else fails closed.
pub fn ensure_data_root() -> io::Result<PathBuf> {
    let root = data_dir();
    if root.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "QAQ-Harness data root is empty",
        ));
    }
    let owner_home = canonical_home()?;
    validate_data_root_location(&root, &owner_home)?;
    fs::create_dir_all(&root)?;
    reject_link(&root)?;
    let canonical_root = fs::canonicalize(&root)?;
    let marker_path = canonical_root.join(DATA_ROOT_MARKER);
    if marker_path.exists() {
        // Safe legacy migration happens before the strict QAQ-Harness check.
        let _ = migrate_legacy_data_root_marker_at(&canonical_root, &owner_home)?;
        return verify_data_root(&canonical_root);
    }

    write_data_root_marker(&canonical_root, &owner_home)?;
    verify_data_root_paths(&canonical_root, &canonical_root, &owner_home)
}

/// Migrate a legacy `DeepX` data-root marker to `QAQ-Harness` when it is safe.
///
/// Uses the current `data_dir()` and current user home. Returns `Some(path)` if
/// the marker was rewritten, or `None` if no migration was needed (marker absent
/// or already `QAQ-Harness`).
pub fn migrate_legacy_data_root_marker() -> io::Result<Option<PathBuf>> {
    let root = data_dir();
    if root.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "QAQ-Harness data root is empty",
        ));
    }
    let owner_home = canonical_home()?;
    validate_data_root_location(&root, &owner_home)?;
    fs::create_dir_all(&root)?;
    reject_link(&root)?;
    let canonical_root = fs::canonicalize(&root)?;
    migrate_legacy_data_root_marker_at(&canonical_root, &owner_home)
}

/// Lower-level migration helper for callers that already have canonical paths.
///
/// Only rewrites a legacy `DeepX` marker when `canonical_root`, `owner_home`, and
/// the marker's `root_id` all match. Unknown products or mismatched identities
/// are rejected without modifying the marker file.
pub fn migrate_legacy_data_root_marker_at(
    canonical_root: &Path,
    owner_home: &Path,
) -> io::Result<Option<PathBuf>> {
    reject_link(canonical_root)?;
    let marker_path = canonical_root.join(DATA_ROOT_MARKER);
    if !marker_path.exists() {
        return Ok(None);
    }
    reject_link(&marker_path)?;
    let mut marker: DataRootMarker =
        serde_json::from_slice(&fs::read(&marker_path)?).map_err(invalid_data)?;
    let canonical_root_text = normalized_path_text(canonical_root);
    let owner_home_text = normalized_path_text(owner_home);
    if marker.canonical_root != canonical_root_text
        || marker.owner_home != owner_home_text
        || marker.root_id != data_root_id(&canonical_root_text, &owner_home_text)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "data root marker does not match the current user and path",
        ));
    }
    if marker.product == "QAQ-Harness" && marker.format_version == 1 {
        return Ok(None);
    }
    if marker.product == "DeepX" && marker.format_version == 1 {
        marker.product = "QAQ-Harness".to_string();
        write_data_root_marker_at(canonical_root, &marker)?;
        return Ok(Some(canonical_root.to_path_buf()));
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unsupported data root marker product '{}'", marker.product),
    ))
}

fn write_data_root_marker(canonical_root: &Path, owner_home: &Path) -> io::Result<()> {
    let canonical_root_text = normalized_path_text(canonical_root);
    let owner_home = normalized_path_text(owner_home);
    let marker = DataRootMarker {
        format_version: 1,
        product: "QAQ-Harness".into(),
        root_id: data_root_id(&canonical_root_text, &owner_home),
        canonical_root: canonical_root_text,
        owner_home,
    };
    write_data_root_marker_at(canonical_root, &marker)
}

fn write_data_root_marker_at(canonical_root: &Path, marker: &DataRootMarker) -> io::Result<()> {
    let marker_path = canonical_root.join(DATA_ROOT_MARKER);
    let temporary = canonical_root.join(".deepx-data-root.json.qaqh-new");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(marker).map_err(invalid_data)?,
    )?;
    fs::rename(&temporary, &marker_path)?;
    Ok(())
}

pub fn verify_data_root(root: &Path) -> io::Result<PathBuf> {
    if root.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "QAQ-Harness data root is empty",
        ));
    }
    reject_link(root)?;
    let owner_home = canonical_home()?;
    let configured = data_dir();
    validate_data_root_location(&configured, &owner_home)?;
    let expected = fs::canonicalize(configured)?;
    let canonical = fs::canonicalize(root)?;
    verify_data_root_paths(&canonical, &expected, &owner_home)
}

fn validate_data_root_location(root: &Path, owner_home: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let expected = owner_home.join(".deepx");
        let parent = root.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "QAQ-Harness data root has no parent directory",
            )
        })?;
        let canonical_parent = fs::canonicalize(parent)?;
        if normalized_path_text(&canonical_parent) != normalized_path_text(owner_home)
            || root
                .file_name()
                .is_none_or(|name| !name.eq_ignore_ascii_case(".deepx"))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "QAQ-Harness data root must be the current user's direct .deepx directory: {}",
                    expected.display()
                ),
            ));
        }
    }
    #[cfg(not(windows))]
    {
        let parent = root.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "QAQ-Harness data root has no parent directory",
            )
        })?;
        if parent.parent().is_none() || root.file_name().is_none_or(|name| name != "qaqh") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("unsafe QAQ-Harness data root '{}'", root.display()),
            ));
        }
        let _ = owner_home;
    }
    Ok(())
}

fn verify_data_root_paths(
    canonical: &Path,
    expected: &Path,
    owner_home: &Path,
) -> io::Result<PathBuf> {
    if normalized_path_text(canonical) != normalized_path_text(expected) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "data root '{}' is not the current QAQ-Harness data directory",
                canonical.display()
            ),
        ));
    }
    reject_link(&canonical)?;
    let marker_path = canonical.join(DATA_ROOT_MARKER);
    reject_link(&marker_path)?;
    let marker: DataRootMarker =
        serde_json::from_slice(&fs::read(&marker_path)?).map_err(invalid_data)?;
    let canonical_root = normalized_path_text(&canonical);
    let owner_home = normalized_path_text(owner_home);
    if marker.format_version != 1
        || marker.product != "QAQ-Harness"
        || marker.canonical_root != canonical_root
        || marker.owner_home != owner_home
        || marker.root_id != data_root_id(&canonical_root, &owner_home)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "QAQ-Harness data root marker does not match the current user and path",
        ));
    }
    Ok(canonical.to_path_buf())
}

fn canonical_home() -> io::Result<PathBuf> {
    let home = home_dir();
    if home.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "user home directory is empty",
        ));
    }
    fs::canonicalize(home)
}

fn reject_link(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("linked data path is not allowed: {}", path.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("reparse data path is not allowed: {}", path.display()),
            ));
        }
    }
    Ok(())
}

pub fn normalized_path_text(path: &Path) -> String {
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

pub fn data_root_id(canonical_root: &str, owner_home: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in canonical_root.bytes().chain([0]).chain(owner_home.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("data-{hash:016x}")
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod data_root_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn copied_data_marker_cannot_authorize_another_directory() {
        let root = test_root();
        let home = root.join("home");
        let first = home.join(".deepx-a");
        let second = home.join(".deepx-b");
        fs::create_dir_all(&first).expect("create first data root");
        fs::create_dir_all(&second).expect("create second data root");
        let home = fs::canonicalize(&home).expect("canonical home");
        let first = fs::canonicalize(&first).expect("canonical first");
        let second = fs::canonicalize(&second).expect("canonical second");

        write_data_root_marker(&first, &home).expect("write data marker");
        assert!(verify_data_root_paths(&first, &first, &home).is_ok());
        fs::copy(first.join(DATA_ROOT_MARKER), second.join(DATA_ROOT_MARKER))
            .expect("copy data marker");
        assert!(verify_data_root_paths(&second, &second, &home).is_err());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn marker_for_another_user_cannot_authorize_deletion() {
        let root = test_root();
        let first_home = root.join("first-home");
        let second_home = root.join("second-home");
        let data = first_home.join(".deepx");
        fs::create_dir_all(&data).expect("create data root");
        fs::create_dir_all(&second_home).expect("create second home");
        let first_home = fs::canonicalize(&first_home).expect("canonical first home");
        let second_home = fs::canonicalize(&second_home).expect("canonical second home");
        let data = fs::canonicalize(&data).expect("canonical data");

        write_data_root_marker(&data, &first_home).expect("write data marker");
        assert!(verify_data_root_paths(&data, &data, &second_home).is_err());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn legacy_deepx_marker_is_migrated_when_safe() {
        let root = test_root();
        let home = root.join("home");
        let data = home.join(".deepx");
        fs::create_dir_all(&data).expect("create data root");
        let home = fs::canonicalize(&home).expect("canonical home");
        let data = fs::canonicalize(&data).expect("canonical data");

        let canonical_root_text = normalized_path_text(&data);
        let owner_home_text = normalized_path_text(&home);
        let legacy = DataRootMarker {
            format_version: 1,
            product: "DeepX".into(),
            canonical_root: canonical_root_text.clone(),
            owner_home: owner_home_text.clone(),
            root_id: data_root_id(&canonical_root_text, &owner_home_text),
        };
        fs::write(
            data.join(DATA_ROOT_MARKER),
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy"),
        )
        .expect("write legacy marker");

        let migrated = migrate_legacy_data_root_marker_at(&data, &home).expect("migrate ok");
        assert_eq!(migrated, Some(data.clone()));
        let marker: DataRootMarker = serde_json::from_slice(
            &fs::read(data.join(DATA_ROOT_MARKER)).expect("read migrated marker"),
        )
        .expect("parse migrated marker");
        assert_eq!(marker.product, "QAQ-Harness");
        assert!(verify_data_root_paths(&data, &data, &home).is_ok());
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn legacy_deepx_marker_is_rejected_on_identity_mismatch() {
        let root = test_root();
        let home = root.join("home");
        let data = home.join(".deepx");
        let other_home = root.join("other-home");
        fs::create_dir_all(&data).expect("create data root");
        fs::create_dir_all(&other_home).expect("create other home");
        let home = fs::canonicalize(&home).expect("canonical home");
        let other_home = fs::canonicalize(&other_home).expect("canonical other home");
        let data = fs::canonicalize(&data).expect("canonical data");

        let canonical_root_text = normalized_path_text(&data);
        let owner_home_text = normalized_path_text(&home);
        let legacy = DataRootMarker {
            format_version: 1,
            product: "DeepX".into(),
            canonical_root: canonical_root_text,
            owner_home: owner_home_text,
            root_id: data_root_id(&normalized_path_text(&data), &normalized_path_text(&home)),
        };
        fs::write(
            data.join(DATA_ROOT_MARKER),
            serde_json::to_vec_pretty(&legacy).expect("serialize legacy"),
        )
        .expect("write legacy marker");
        let before = fs::read(data.join(DATA_ROOT_MARKER)).expect("read before");

        let err = migrate_legacy_data_root_marker_at(&data, &other_home)
            .expect_err("mismatched owner must fail");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let after = fs::read(data.join(DATA_ROOT_MARKER)).expect("read after");
        assert_eq!(before, after, "failed migration must not modify the marker");
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn current_qaqh_marker_is_idempotent() {
        let root = test_root();
        let home = root.join("home");
        let data = home.join(".deepx");
        fs::create_dir_all(&data).expect("create data root");
        let home = fs::canonicalize(&home).expect("canonical home");
        let data = fs::canonicalize(&data).expect("canonical data");
        write_data_root_marker(&data, &home).expect("write qaqh marker");

        let before = fs::read(data.join(DATA_ROOT_MARKER)).expect("read before");
        let migrated = migrate_legacy_data_root_marker_at(&data, &home).expect("migrate ok");
        assert_eq!(migrated, None);
        let after = fs::read(data.join(DATA_ROOT_MARKER)).expect("read after");
        assert_eq!(before, after, "current marker must remain byte-identical");
        fs::remove_dir_all(root).expect("remove test root");
    }

    #[test]
    fn unknown_product_marker_is_rejected_without_write() {
        let root = test_root();
        let home = root.join("home");
        let data = home.join(".deepx");
        fs::create_dir_all(&data).expect("create data root");
        let home = fs::canonicalize(&home).expect("canonical home");
        let data = fs::canonicalize(&data).expect("canonical data");

        let canonical_root_text = normalized_path_text(&data);
        let owner_home_text = normalized_path_text(&home);
        let marker = DataRootMarker {
            format_version: 1,
            product: "OtherProduct".into(),
            canonical_root: canonical_root_text,
            owner_home: owner_home_text,
            root_id: data_root_id(&normalized_path_text(&data), &normalized_path_text(&home)),
        };
        fs::write(
            data.join(DATA_ROOT_MARKER),
            serde_json::to_vec_pretty(&marker).expect("serialize marker"),
        )
        .expect("write marker");
        let before = fs::read(data.join(DATA_ROOT_MARKER)).expect("read before");

        let err = migrate_legacy_data_root_marker_at(&data, &home)
            .expect_err("unknown product must fail closed");
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        let after = fs::read(data.join(DATA_ROOT_MARKER)).expect("read after");
        assert_eq!(before, after, "unknown product must not rewrite marker");
        fs::remove_dir_all(root).expect("remove test root");
    }

    fn test_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "qaqh-data-root-test-{}-{nonce}",
            std::process::id()
        ))
    }
}

/// qaqh config file path.
pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

/// qaqh daemon discovery file path.
pub fn daemon_discovery_path() -> PathBuf {
    data_dir().join("daemon.json")
}

pub fn daemon_lock_path() -> PathBuf {
    data_dir().join("daemon.lock")
}

/// qaqh sessions directory.
pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// qaqh plans directory.
pub fn plans_dir() -> PathBuf {
    data_dir().join("plans")
}

/// Kill a process by PID (cross-platform).
/// - Windows: `taskkill /F /PID`
/// - Unix: `kill -9`
pub fn kill_process(pid: u32) {
    if cfg!(target_os = "windows") {
        let mut command = background_command("taskkill");
        drop(command.args(["/F", "/PID", &pid.to_string()]).output());
    } else {
        drop(
            std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output(),
        );
    }
}

/// Return whether a process id currently exists without mutating it.
pub fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if cfg!(target_os = "windows") {
        background_command("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line.split(',')
                        .nth(1)
                        .is_some_and(|field| field.trim_matches('"').trim() == pid.to_string())
                })
            })
    } else {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

fn background_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Convert days since epoch 0000-01-01 to (year, month, day).
/// Algorithm from Howard Hinnant's civil_from_days.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
