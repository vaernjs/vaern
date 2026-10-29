//! Trait and implementation for resolving and loading ES modules from disk.
//!
//! This module owns relative path resolution for ES module imports in M1a,
//! validating that imported specifiers are strictly relative local paths.

use crate::error::{Result, VaernError};
use std::path::{Path, PathBuf};

/// Trait defining ES-module resolution and loading behavior.
pub trait ModuleLoader: Send + Sync {
    /// Resolves an import specifier relative to the referrer module path.
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf>;

    /// Loads the source code for a resolved module path.
    fn load(&self, path: &Path) -> Result<String>;
}

/// Standard file-system module loader resolving relative local files.
#[derive(Debug, Default, Clone)]
pub struct FsModuleLoader;

impl ModuleLoader for FsModuleLoader {
    fn resolve(&self, specifier: &str, referrer: &Path) -> Result<PathBuf> {
        if !specifier.starts_with("./") && !specifier.starts_with("../") {
            return Err(VaernError::ModuleResolution {
                specifier: specifier.to_string(),
                referrer: referrer.display().to_string(),
                reason: "bare specifiers and non-relative imports are not supported in M1a"
                    .to_string(),
            });
        }

        let parent = match referrer.parent() {
            Some(p) => p,
            None => Path::new("."),
        };
        let resolved = parent.join(specifier);

        // Normalize path without requiring canonicalize on non-existent paths
        let normalized = resolved.components().collect::<PathBuf>();

        if !normalized.exists() {
            return Err(VaernError::ModuleResolution {
                specifier: specifier.to_string(),
                referrer: referrer.display().to_string(),
                reason: format!("target file '{}' does not exist", normalized.display()),
            });
        }

        Ok(normalized)
    }

    fn load(&self, path: &Path) -> Result<String> {
        std::fs::read_to_string(path).map_err(|err| VaernError::Io {
            path: path.display().to_string(),
            source: err,
        })
    }
}
