//! Central project struct that owns all state.

use std::io::Result;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use tokio_fs_ext::DirEntry;

use crate::config::Config;
use crate::error::OpfsError;
use crate::fuse_fs::FuseFs;
use crate::package_lock::PackageLock;
use crate::package_manager::{self, InstallOptions};
use crate::store::Store;

/// The main API entry point for opfs-project.
///
/// Owns all state: caches, config, store, and the current working directory.
/// Instances are typically created once and shared behind an `Arc` or `RwLock`.
pub struct OpfsProject {
    config: Config,
    fuse_fs: FuseFs,
    store: Store,
}

impl Default for OpfsProject {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl OpfsProject {
    /// Create a new project with the given config.
    pub fn new(config: Config) -> Self {
        let fuse_fs = FuseFs::new(config.fuse_cache_max_entries);
        let store = Store::new(&config);
        Self {
            config,
            fuse_fs,
            store,
        }
    }

    // ── cwd ──────────────────────────────────────────────────────────

    /// Set the current working directory.
    ///
    /// Delegates to `tokio_fs_ext::set_current_dir` so that all code paths
    /// (including those outside `OpfsProject`) see the same cwd.
    pub fn set_cwd(&self, path: impl AsRef<Path>) {
        tokio_fs_ext::set_current_dir(path).unwrap();
    }

    /// Get the current working directory.
    pub fn cwd(&self) -> PathBuf {
        tokio_fs_ext::current_dir().unwrap()
    }

    // ── path preparation ─────────────────────────────────────────────

    fn prepare_path(&self, path: &Path) -> PathBuf {
        if path.starts_with("/") {
            path.to_path_buf()
        } else {
            let cwd = self.cwd();
            if let Ok(stripped) = path.strip_prefix(".") {
                cwd.join(stripped)
            } else {
                cwd.join(path)
            }
        }
    }

    // ── fuse-aware reads ─────────────────────────────────────────────

    /// Read file content, transparently resolving fuse links.
    pub async fn read(&self, path: impl AsRef<Path>) -> Result<Bytes> {
        let prepared = self.prepare_path(path.as_ref());

        if let Some(content) = self.fuse_fs.try_read(&prepared).await? {
            return Ok(content);
        }

        let raw = tokio_fs_ext::read(&prepared).await?;
        Ok(Bytes::from(raw))
    }

    /// Read directory contents, transparently merging fuse-link entries.
    pub async fn read_dir(&self, path: impl AsRef<Path>) -> Result<Vec<DirEntry>> {
        let prepared = self.prepare_path(path.as_ref());

        if let Some(entries) = self.fuse_fs.try_read_dir(&prepared).await? {
            return Ok(entries);
        }

        tokio_fs_ext::read_dir(&prepared).await?.collect()
    }

    /// Get file/directory metadata, transparently resolving fuse links.
    pub async fn metadata(&self, path: impl AsRef<Path>) -> Result<tokio_fs_ext::Metadata> {
        let prepared = self.prepare_path(path.as_ref());

        if let Some(meta) = self.fuse_fs.try_metadata(&prepared).await? {
            return Ok(meta);
        }

        tokio_fs_ext::metadata(&prepared).await
    }

    // ── package management ───────────────────────────────────────────

    /// Install packages from a parsed `PackageLock`.
    pub async fn install(
        &self,
        lock: &PackageLock,
        opts: &InstallOptions,
    ) -> std::result::Result<(), OpfsError> {
        package_manager::install(self, lock, opts).await
    }

    // ── accessors for internal subsystems ─────────────────────────────

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn fuse_fs(&self) -> &FuseFs {
        &self.fuse_fs
    }
}
