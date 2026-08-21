use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Result, UpdateError, validate_relative_path};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub format_version: u32,
    pub release_id: String,
    pub channel: String,
    pub published_at: String,
    pub components: BTreeMap<String, CatalogComponent>,
    pub artifacts: Vec<Artifact>,
}

impl Catalog {
    pub fn validate(&self) -> Result<()> {
        if self.format_version != 1 {
            return Err(UpdateError(format!(
                "unsupported catalog format version {}",
                self.format_version
            )));
        }
        if self.release_id.trim().is_empty() {
            return Err(UpdateError("catalog releaseId is empty".into()));
        }
        if self.artifacts.is_empty() {
            return Err(UpdateError("catalog contains no artifacts".into()));
        }
        for artifact in &self.artifacts {
            artifact.validate()?;
            for (name, target) in &artifact.targets {
                if let Some(component) = self.components.get(name) {
                    if &component.build_id != target {
                        return Err(UpdateError(format!(
                            "artifact {} targets {}={}, but catalog target is {}",
                            artifact.id, name, target, component.build_id
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// 按 id 插入 / 替换 artifact（同 id 覆盖，保持 catalog 其余字段）。
    /// 用于打包 CLI：增量包 / 完整包产出后把 artifact 条目写回 catalog。
    pub fn upsert_artifact(&mut self, artifact: Artifact) {
        if let Some(slot) = self.artifacts.iter_mut().find(|a| a.id == artifact.id) {
            *slot = artifact;
        } else {
            self.artifacts.push(artifact);
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogComponent {
    pub build_id: String,
    pub version: String,
    #[serde(default)]
    pub control_protocol: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub id: String,
    pub kind: ArtifactKind,
    pub strategy: ArtifactStrategy,
    /// 增量参照的检查点版本（仅 `file-level-delta` artifact 携带；
    /// 完整包/组件全量不填）。客户端升级决策只看 sha256，此字段仅展示用。
    #[serde(default)]
    pub baseline: Option<String>,
    pub targets: BTreeMap<String, String>,
    #[serde(default)]
    pub requires: ArtifactRequires,
    pub restart_policy: RestartPolicy,
    pub payload: ArtifactPayload,
}

impl Artifact {
    fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(UpdateError("artifact id is empty".into()));
        }
        if self.targets.is_empty() {
            return Err(UpdateError(format!(
                "artifact {} contains no targets",
                self.id
            )));
        }
        validate_relative_path(&self.payload.path)?;
        if self.payload.sha256.len() != 64
            || !self
                .payload
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(UpdateError(format!(
                "artifact {} has invalid SHA-256",
                self.id
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactKind {
    Renderer,
    Shell,
    Frontend,
    Backend,
    Runtime,
    Full,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Renderer => "renderer",
            Self::Shell => "shell",
            Self::Frontend => "frontend",
            Self::Backend => "backend",
            Self::Runtime => "runtime",
            Self::Full => "full",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactStrategy {
    ComponentFull,
    /// 文件级增量（基线累积模型，docs/winui-update-design.md）：
    /// bundle `files[]` 只列相对 `baseline` 检查点变化的文件；
    /// 客户端按 sha256 自适应（匹配当前安装文件则本地复制，0 下载）。
    FileLevelDelta,
    BinaryDelta,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    None,
    Renderer,
    Electron,
    Daemon,
    Full,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRequires {
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub control_protocol: Option<u16>,
    #[serde(default)]
    pub base_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArtifactPayload {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleManifest {
    pub format_version: u32,
    pub kind: String,
    pub build_id: String,
    pub app_version: String,
    #[serde(default)]
    pub release_id: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub components: BTreeMap<String, CatalogComponent>,
    pub requires_full_install: bool,
    pub files: Vec<BundleFile>,
}

impl BundleManifest {
    pub fn validate(&self) -> Result<()> {
        if self.format_version != 1 {
            return Err(UpdateError(format!(
                "unsupported bundle format version {}",
                self.format_version
            )));
        }
        if !matches!(
            self.kind.as_str(),
            "full" | "frontend" | "backend" | "renderer" | "shell" | "runtime"
        ) {
            return Err(UpdateError(format!(
                "unsupported bundle kind {}",
                self.kind
            )));
        }
        if self.build_id.trim().is_empty() || self.files.is_empty() {
            return Err(UpdateError(
                "bundle must contain buildId and at least one file".into(),
            ));
        }
        for (name, component) in &self.components {
            if name.trim().is_empty()
                || component.build_id.trim().is_empty()
                || component.version.trim().is_empty()
            {
                return Err(UpdateError(
                    "bundle components must contain a name, buildId, and version".into(),
                ));
            }
        }
        for file in &self.files {
            validate_relative_path(&file.source)?;
            validate_relative_path(&file.target)?;
            if file.sha256.len() != 64 || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(UpdateError(format!(
                    "bundle file {} has invalid SHA-256",
                    file.source
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BundleFile {
    pub source: String,
    pub target: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledState {
    pub format_version: u32,
    pub installation_id: String,
    pub release_id: String,
    pub channel: String,
    pub components: BTreeMap<String, ComponentState>,
    #[serde(default)]
    pub last_committed_operation: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ComponentState {
    pub current: String,
    #[serde(default)]
    pub previous: Option<String>,
    pub version: String,
    #[serde(default)]
    pub protocol: Option<u16>,
    pub health: ComponentHealth,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentHealth {
    Unknown,
    Healthy,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    Install,
    Update,
    Upgrade,
    Current,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatePlan {
    pub operation_id: String,
    pub release_id: String,
    pub mode: UpdateMode,
    pub artifacts: Vec<String>,
    pub actions: Vec<UpdateAction>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedOperation {
    pub format_version: u32,
    pub operation_id: String,
    pub release_id: String,
    pub source: String,
    pub plan: UpdatePlan,
    #[serde(default)]
    pub previous_state: Option<InstalledState>,
    pub artifacts: Vec<StagedArtifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedArtifact {
    pub id: String,
    pub kind: ArtifactKind,
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpdateAction {
    Stage,
    PrepareBackend,
    ApplyBackend,
    RestartBackend,
    VerifyBackend,
    PrepareFrontend,
    ApplyFrontend,
    RestartElectron,
    ApplyRuntime,
    ApplyFull,
    VerifyInstallation,
    Commit,
}
