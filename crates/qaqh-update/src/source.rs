use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Catalog, Result, UpdateError, parse_catalog};

pub trait UpdateSource {
    fn describe(&self) -> &'static str;
    fn read_catalog(&self) -> Result<Vec<u8>>;
    fn open_artifact(&self, relative_path: &str) -> Result<Box<dyn Read>>;

    fn catalog(&self) -> Result<Catalog> {
        parse_catalog(&self.read_catalog()?)
    }
}

pub struct DirectoryUpdateSource {
    root: PathBuf,
}

impl DirectoryUpdateSource {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            return Err(UpdateError(format!(
                "update source directory does not exist: {}",
                root.display()
            )));
        }
        Ok(Self { root })
    }

    fn resolve(&self, relative_path: &str) -> Result<PathBuf> {
        validate_relative_path(relative_path)?;
        Ok(self
            .root
            .join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR)))
    }
}

impl UpdateSource for DirectoryUpdateSource {
    fn describe(&self) -> &'static str {
        "local-directory"
    }

    fn read_catalog(&self) -> Result<Vec<u8>> {
        fs::read(self.root.join("catalog.json")).map_err(|error| {
            UpdateError(format!(
                "read catalog '{}': {error}",
                self.root.join("catalog.json").display()
            ))
        })
    }

    fn open_artifact(&self, relative_path: &str) -> Result<Box<dyn Read>> {
        let path = self.resolve(relative_path)?;
        let file = fs::File::open(&path)
            .map_err(|error| UpdateError(format!("open artifact '{}': {error}", path.display())))?;
        Ok(Box::new(file))
    }
}

pub fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\\')
        || value.contains(':')
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(UpdateError(format!("unsafe relative path: {value}")));
    }
    Ok(())
}

pub fn sha256_reader(mut reader: impl Read) -> Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((size, hex::encode(hasher.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_rejects_escape_and_absolute_paths() {
        assert!(validate_relative_path("bundles/backend.zip").is_ok());
        assert!(validate_relative_path("../backend.zip").is_err());
        assert!(validate_relative_path(r"C:\backend.zip").is_err());
        assert!(validate_relative_path("/backend.zip").is_err());
        assert!(validate_relative_path(r"bundles\backend.zip").is_err());
        assert!(validate_relative_path("bundles/./backend.zip").is_err());
        assert!(validate_relative_path("bundles//backend.zip").is_err());
    }
}
