use std::fs;
use std::path::Path;

use crate::{
    Artifact, ArtifactKind, BundleFile, BundleManifest, Catalog, InstalledState, Result,
    UpdateAction, UpdateError, UpdateMode, UpdatePlan, sha256_reader,
};

/// 文件级增量规划（基线累积模型，docs/winui-update-design.md §4）。
///
/// 输入：基线（检查点完整包）manifest + 目标构建目录（collect-payload 的
/// files/ 内容）。输出：增量包变化文件清单。
///
/// 规则：目标目录每个文件与基线同 `target` 的 sha256 比对——
/// - 相同 → 跳过（未变化；客户端 sha256 匹配当前安装文件时本地复制 0 下载）；
/// - 不同 / 基线缺失 → 列入增量（`source` 带 `files/` 前缀，与完整包解压布局一致）。
///
/// 基线存在但目标已删除的文件**不列入**：增量模型无删除语义（覆盖式应用），
/// 删除残留由检查点完整包 / 重装清理（设计文档契约 5）。
pub fn plan_delta(baseline: &BundleManifest, target_dir: &Path) -> Result<Vec<BundleFile>> {
    let mut changed = Vec::new();
    collect_changed(baseline, target_dir, "", &mut changed)?;
    Ok(changed)
}

fn collect_changed(
    baseline: &BundleManifest,
    dir: &Path,
    prefix: &str,
    changed: &mut Vec<BundleFile>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_changed(baseline, &path, &rel, changed)?;
        } else if file_type.is_file() {
            let (size, sha256) = sha256_reader(fs::File::open(&path)?)?;
            let unchanged = baseline
                .files
                .iter()
                .any(|file| file.target == rel && file.size == size && file.sha256 == sha256);
            if !unchanged {
                changed.push(BundleFile {
                    source: format!("files/{rel}"),
                    target: rel,
                    size,
                    sha256,
                });
            }
        }
        // 符号链接 / 其它类型跳过（安装 payload 不应含链接）。
    }
    Ok(())
}

/// 由目标 manifest + 变化文件清单组装增量 BundleManifest（file-level-delta）。
///
/// 元数据（kind / buildId / appVersion / components…）继承目标构建，
/// `files` 替换为变化清单，`requires_full_install` 强制 `false`（增量必须在
/// 已有安装上应用，不携带安装引导语义）。
pub fn build_delta_manifest(target: &BundleManifest, changed: &[BundleFile]) -> BundleManifest {
    let mut delta = target.clone();
    delta.files = changed.to_vec();
    delta.requires_full_install = false;
    delta
}

pub fn plan_update(state: Option<&InstalledState>, catalog: &Catalog) -> Result<UpdatePlan> {
    catalog.validate()?;
    let Some(state) = state else {
        return full_plan(catalog, UpdateMode::Install);
    };

    let runtime_changed = component_changed(state, catalog, "runtime");
    let frontend_changed = component_changed(state, catalog, "frontend");
    let backend_changed = component_changed(state, catalog, "backend");

    if !runtime_changed && !frontend_changed && !backend_changed {
        return Ok(UpdatePlan {
            operation_id: operation_id(&catalog.release_id, &[]),
            release_id: catalog.release_id.clone(),
            mode: UpdateMode::Current,
            artifacts: Vec::new(),
            actions: Vec::new(),
        });
    }

    if runtime_changed {
        if let Some(runtime) = artifact(catalog, ArtifactKind::Runtime) {
            return component_plan(catalog, vec![runtime], UpdateMode::Upgrade);
        }
        return full_plan(catalog, UpdateMode::Upgrade);
    }

    let target_protocol = catalog
        .components
        .get("backend")
        .and_then(|component| component.control_protocol);
    let current_frontend_protocol = state
        .components
        .get("frontend")
        .and_then(|component| component.protocol);
    let current_backend_protocol = state
        .components
        .get("backend")
        .and_then(|component| component.protocol);
    let target_frontend_protocol = catalog
        .components
        .get("frontend")
        .and_then(|component| component.control_protocol);

    if frontend_changed && backend_changed {
        if target_frontend_protocol != target_protocol {
            return full_plan(catalog, UpdateMode::Upgrade);
        }
        if let (Some(frontend), Some(backend)) = (
            artifact(catalog, ArtifactKind::Frontend),
            artifact(catalog, ArtifactKind::Backend),
        ) {
            return component_plan(catalog, vec![frontend, backend], UpdateMode::Update);
        }
        return full_plan(catalog, UpdateMode::Upgrade);
    }

    if backend_changed {
        if target_protocol != current_frontend_protocol {
            return full_plan(catalog, UpdateMode::Upgrade);
        }
        if let Some(backend) = artifact(catalog, ArtifactKind::Backend) {
            return component_plan(catalog, vec![backend], UpdateMode::Update);
        }
    }

    if frontend_changed {
        if target_frontend_protocol != current_backend_protocol {
            return full_plan(catalog, UpdateMode::Upgrade);
        }
        if let Some(frontend) = artifact(catalog, ArtifactKind::Frontend) {
            return component_plan(catalog, vec![frontend], UpdateMode::Update);
        }
    }

    full_plan(catalog, UpdateMode::Upgrade)
}

fn component_changed(state: &InstalledState, catalog: &Catalog, name: &str) -> bool {
    let Some(target) = catalog.components.get(name) else {
        return false;
    };
    let current = state
        .components
        .get(name)
        .map(|component| &component.current);
    current != Some(&target.build_id)
}

fn artifact(catalog: &Catalog, kind: ArtifactKind) -> Option<&Artifact> {
    catalog
        .artifacts
        .iter()
        .find(|artifact| artifact.kind == kind)
}

fn full_plan(catalog: &Catalog, mode: UpdateMode) -> Result<UpdatePlan> {
    let full = artifact(catalog, ArtifactKind::Full)
        .ok_or_else(|| UpdateError("catalog has no applicable artifact or full fallback".into()))?;
    component_plan(catalog, vec![full], mode)
}

fn component_plan(
    catalog: &Catalog,
    artifacts: Vec<&Artifact>,
    mode: UpdateMode,
) -> Result<UpdatePlan> {
    let ids = artifacts
        .iter()
        .map(|artifact| artifact.id.clone())
        .collect::<Vec<_>>();
    let mut actions = vec![UpdateAction::Stage];
    for artifact in artifacts {
        match artifact.kind {
            ArtifactKind::Backend => actions.extend([
                UpdateAction::PrepareBackend,
                UpdateAction::ApplyBackend,
                UpdateAction::RestartBackend,
                UpdateAction::VerifyBackend,
            ]),
            ArtifactKind::Frontend | ArtifactKind::Shell => actions.extend([
                UpdateAction::PrepareFrontend,
                UpdateAction::ApplyFrontend,
                UpdateAction::RestartElectron,
            ]),
            ArtifactKind::Renderer => {
                actions.extend([UpdateAction::PrepareFrontend, UpdateAction::ApplyFrontend])
            }
            ArtifactKind::Runtime => {
                actions.extend([UpdateAction::PrepareFrontend, UpdateAction::ApplyRuntime])
            }
            ArtifactKind::Full => actions.push(UpdateAction::ApplyFull),
        }
    }
    actions.extend([UpdateAction::VerifyInstallation, UpdateAction::Commit]);
    Ok(UpdatePlan {
        operation_id: operation_id(&catalog.release_id, &ids),
        release_id: catalog.release_id.clone(),
        mode,
        artifacts: ids,
        actions,
    })
}

fn operation_id(release_id: &str, artifacts: &[String]) -> String {
    let suffix = if artifacts.is_empty() {
        "current".into()
    } else {
        artifacts.join("+")
    };
    let raw = format!("op-{release_id}-{suffix}");
    raw.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '+') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::{
        ArtifactPayload, ArtifactRequires, ArtifactStrategy, CatalogComponent, ComponentHealth,
        ComponentState, RestartPolicy,
    };

    fn catalog() -> Catalog {
        Catalog {
            format_version: 1,
            release_id: "release-2".into(),
            channel: "local".into(),
            published_at: "2026-07-27T00:00:00Z".into(),
            components: BTreeMap::from([
                (
                    "runtime".into(),
                    CatalogComponent {
                        build_id: "runtime-1".into(),
                        version: "43".into(),
                        control_protocol: None,
                    },
                ),
                (
                    "frontend".into(),
                    CatalogComponent {
                        build_id: "frontend-2".into(),
                        version: "0.9".into(),
                        control_protocol: Some(1),
                    },
                ),
                (
                    "backend".into(),
                    CatalogComponent {
                        build_id: "backend-2".into(),
                        version: "0.9".into(),
                        control_protocol: Some(1),
                    },
                ),
            ]),
            artifacts: vec![
                artifact("frontend", ArtifactKind::Frontend),
                artifact("backend", ArtifactKind::Backend),
                artifact("full", ArtifactKind::Full),
            ],
        }
    }

    fn artifact(id: &str, kind: ArtifactKind) -> Artifact {
        Artifact {
            id: id.into(),
            kind,
            strategy: ArtifactStrategy::ComponentFull,
            baseline: None,
            targets: BTreeMap::from([(id.into(), format!("{id}-2"))]),
            requires: ArtifactRequires::default(),
            restart_policy: RestartPolicy::Full,
            payload: ArtifactPayload {
                path: format!("bundles/{id}.zip"),
                size: 1,
                sha256: "a".repeat(64),
            },
        }
    }

    fn state(frontend: &str, backend: &str) -> InstalledState {
        InstalledState {
            format_version: 2,
            installation_id: "installation".into(),
            release_id: "release-1".into(),
            channel: "local".into(),
            components: BTreeMap::from([
                ("runtime".into(), component("runtime-1", None)),
                ("frontend".into(), component(frontend, Some(1))),
                ("backend".into(), component(backend, Some(1))),
            ]),
            last_committed_operation: None,
        }
    }

    fn component(build: &str, protocol: Option<u16>) -> ComponentState {
        ComponentState {
            current: build.into(),
            previous: None,
            version: "0.9".into(),
            protocol,
            health: ComponentHealth::Healthy,
        }
    }

    #[test]
    fn no_state_uses_full_install() {
        let plan = plan_update(None, &catalog()).unwrap();
        assert_eq!(plan.mode, UpdateMode::Install);
        assert_eq!(plan.artifacts, ["full"]);
    }

    #[test]
    fn backend_only_uses_backend_artifact() {
        let plan = plan_update(Some(&state("frontend-2", "backend-1")), &catalog()).unwrap();
        assert_eq!(plan.mode, UpdateMode::Update);
        assert_eq!(plan.artifacts, ["backend"]);
        assert!(plan.actions.contains(&UpdateAction::RestartBackend));
    }

    #[test]
    fn frontend_and_backend_use_two_component_artifacts() {
        let plan = plan_update(Some(&state("frontend-1", "backend-1")), &catalog()).unwrap();
        assert_eq!(plan.artifacts, ["frontend", "backend"]);
    }

    #[test]
    fn protocol_mismatch_falls_back_to_full() {
        let mut next = catalog();
        next.components.get_mut("backend").unwrap().control_protocol = Some(2);
        let plan = plan_update(Some(&state("frontend-2", "backend-1")), &next).unwrap();
        assert_eq!(plan.artifacts, ["full"]);
    }

    #[test]
    fn component_only_catalog_ignores_omitted_components() {
        let mut next = catalog();
        next.components.retain(|name, _| name == "frontend");
        next.artifacts
            .retain(|artifact| artifact.kind == ArtifactKind::Frontend);
        let plan = plan_update(Some(&state("frontend-1", "backend-1")), &next).unwrap();
        assert_eq!(plan.artifacts, ["frontend"]);
        assert!(!plan.actions.contains(&UpdateAction::ApplyFull));
    }

    // ── plan_delta（文件级增量）──────────────────────────────────

    fn file_sha(content: &[u8]) -> String {
        crate::sha256_reader(std::io::Cursor::new(content))
            .unwrap()
            .1
    }

    fn delta_file(source: &str, target: &str, content: &[u8]) -> BundleFile {
        BundleFile {
            source: source.into(),
            target: target.into(),
            size: content.len() as u64,
            sha256: file_sha(content),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("qaqh-update-delta-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn delta_manifest_inherits_metadata_and_forces_no_full_install() {
        let target = BundleManifest {
            format_version: 1,
            kind: "runtime".into(),
            build_id: "runtime-new".into(),
            app_version: "1.0.1".into(),
            release_id: "release-delta".into(),
            channel: "stable".into(),
            components: BTreeMap::from([(
                "runtime".into(),
                CatalogComponent {
                    build_id: "runtime-new".into(),
                    version: "1.0.1".into(),
                    control_protocol: Some(1),
                },
            )]),
            requires_full_install: true,
            files: vec![delta_file("files/a.txt", "a.txt", b"target")],
        };
        let changed = vec![delta_file("files/b.txt", "b.txt", b"new")];
        let delta = build_delta_manifest(&target, &changed);
        assert_eq!(delta.kind, "runtime");
        assert_eq!(delta.build_id, "runtime-new");
        assert!(
            !delta.requires_full_install,
            "delta must not require full install"
        );
        assert_eq!(delta.files.len(), 1);
        assert_eq!(delta.files[0].target, "b.txt");
        assert!(delta.components.contains_key("runtime"));
    }

    #[test]
    fn catalog_upsert_adds_and_replaces_artifact() {
        let mut c = catalog();
        let base_count = c.artifacts.len();
        let mut delta_artifact = artifact("runtime", ArtifactKind::Runtime);
        delta_artifact.strategy = ArtifactStrategy::FileLevelDelta;
        delta_artifact.baseline = Some("1.0.0".into());
        // 新增
        c.upsert_artifact(delta_artifact.clone());
        assert_eq!(c.artifacts.len(), base_count + 1);
        // 同 id 替换
        delta_artifact.baseline = Some("1.1.0".into());
        c.upsert_artifact(delta_artifact.clone());
        assert_eq!(c.artifacts.len(), base_count + 1);
        assert_eq!(
            c.artifacts
                .iter()
                .find(|a| a.id == "runtime")
                .unwrap()
                .baseline
                .as_deref(),
            Some("1.1.0")
        );
    }

    #[test]
    fn delta_lists_changed_and_added_only() {
        let dir = temp_dir("changed");
        fs::write(dir.join("a.txt"), b"same").unwrap();
        fs::write(dir.join("b.txt"), b"new").unwrap();
        fs::write(dir.join("c.txt"), b"added").unwrap();
        let baseline = BundleManifest {
            format_version: 1,
            kind: "runtime".into(),
            build_id: "baseline-1".into(),
            app_version: "1.0.0".into(),
            release_id: String::new(),
            channel: "local".into(),
            components: BTreeMap::new(),
            requires_full_install: false,
            files: vec![
                delta_file("files/a.txt", "a.txt", b"same"),
                delta_file("files/b.txt", "b.txt", b"old"),
                delta_file("files/gone.txt", "gone.txt", b"deleted"),
            ],
        };

        let delta = plan_delta(&baseline, &dir).unwrap();
        let targets: Vec<&str> = delta.iter().map(|f| f.target.as_str()).collect();
        // 变化 + 新增列入；未变跳过；已删除不列。
        assert!(targets.contains(&"b.txt"), "changed file listed");
        assert!(targets.contains(&"c.txt"), "added file listed");
        assert!(!targets.contains(&"a.txt"), "unchanged file skipped");
        assert!(!targets.contains(&"gone.txt"), "deleted file not listed");
        // source 带 files/ 前缀；sha256 为目标实际内容。
        let b = delta.iter().find(|f| f.target == "b.txt").unwrap();
        assert_eq!(b.source, "files/b.txt");
        assert_eq!(b.sha256, file_sha(b"new"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delta_empty_when_unchanged() {
        let dir = temp_dir("same");
        fs::write(dir.join("a.txt"), b"same").unwrap();
        let baseline = BundleManifest {
            format_version: 1,
            kind: "backend".into(),
            build_id: "baseline-1".into(),
            app_version: "1.0.0".into(),
            release_id: String::new(),
            channel: "local".into(),
            components: BTreeMap::new(),
            requires_full_install: false,
            files: vec![delta_file("files/a.txt", "a.txt", b"same")],
        };
        let delta = plan_delta(&baseline, &dir).unwrap();
        assert!(delta.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delta_handles_subdirectories() {
        let dir = temp_dir("subdir");
        fs::create_dir_all(dir.join("resources")).unwrap();
        fs::write(dir.join("resources/daemon.exe"), b"v2").unwrap();
        fs::write(dir.join("app.exe"), b"same").unwrap();
        let baseline = BundleManifest {
            format_version: 1,
            kind: "backend".into(),
            build_id: "baseline-1".into(),
            app_version: "1.0.0".into(),
            release_id: String::new(),
            channel: "local".into(),
            components: BTreeMap::new(),
            requires_full_install: false,
            files: vec![
                delta_file("files/resources/daemon.exe", "resources/daemon.exe", b"v1"),
                delta_file("files/app.exe", "app.exe", b"same"),
            ],
        };
        let delta = plan_delta(&baseline, &dir).unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].target, "resources/daemon.exe");
        assert_eq!(delta[0].source, "files/resources/daemon.exe");
        fs::remove_dir_all(&dir).unwrap();
    }
}
