//! Shared update protocol and planning engine used by QAQ-Harness Installer and Updater.

mod apply;
mod install_root;
mod model;
mod planner;
mod source;
mod state;

pub use apply::{apply_bundle_zip, read_bundle_manifest_zip, rollback_bundle_zip};
pub use install_root::{
    INSTALL_ROOT_MARKER, is_reparse_or_symlink, safe_join_under_root, verify_install_root,
    write_install_root_marker,
};
pub use model::{
    Artifact, ArtifactKind, ArtifactPayload, ArtifactRequires, ArtifactStrategy, BundleFile,
    BundleManifest, Catalog, CatalogComponent, ComponentHealth, ComponentState, InstalledState,
    RestartPolicy, StagedArtifact, StagedOperation, UpdateAction, UpdateMode, UpdatePlan,
};
pub use planner::{build_delta_manifest, plan_delta, plan_update};
pub use source::{DirectoryUpdateSource, UpdateSource, sha256_reader, validate_relative_path};
pub use state::{
    commit_bundle_state, installation_id_for_path, load_installed_state, migrate_installed_state,
    write_installed_state,
};

use std::fmt;

#[derive(Debug)]
pub struct UpdateError(pub String);

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UpdateError {}

impl From<std::io::Error> for UpdateError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<serde_json::Error> for UpdateError {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, UpdateError>;

pub fn parse_bundle_manifest(bytes: &[u8]) -> Result<BundleManifest> {
    let manifest: BundleManifest = serde_json::from_slice(bytes)
        .map_err(|error| UpdateError(format!("parse bundle.json: {error}")))?;
    manifest.validate()?;
    Ok(manifest)
}

pub fn parse_catalog(bytes: &[u8]) -> Result<Catalog> {
    let catalog: Catalog = serde_json::from_slice(bytes)
        .map_err(|error| UpdateError(format!("parse catalog.json: {error}")))?;
    catalog.validate()?;
    Ok(catalog)
}
