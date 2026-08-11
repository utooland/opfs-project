//! Central project struct that owns all state.

use std::io::Result;
use std::path::{Component, Path, PathBuf};

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
        tokio_fs_ext::set_current_dir(path).expect("failed to set current directory");
    }

    /// Get the current working directory.
    pub fn cwd(&self) -> PathBuf {
        tokio_fs_ext::current_dir().expect("failed to get current directory")
    }

    // ── path preparation ─────────────────────────────────────────────

    fn prepare_path(&self, path: &Path) -> PathBuf {
        let prepared = if path.is_absolute() {
            path.to_path_buf()
        } else {
            let cwd = self.cwd();
            if let Ok(stripped) = path.strip_prefix(".") {
                cwd.join(stripped)
            } else {
                cwd.join(path)
            }
        };
        lexical_normalize(&prepared)
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

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match normalized.components().next_back() {
                Some(Component::Normal(_)) => {
                    normalized.pop();
                }
                Some(Component::ParentDir) | None => normalized.push(".."),
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                Some(Component::CurDir) => {}
            },
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }

    if normalized.as_os_str().is_empty() {
        if path.is_absolute() {
            PathBuf::from("/")
        } else {
            PathBuf::from(".")
        }
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_lexical_normalize_collapses_relative_segments() {
        assert_eq!(
            lexical_normalize(Path::new(
                "/project/node_modules/pkg/dist/esm/packages/runtime/es/common/request/middleware/../../../../../../node_modules/tslib/tslib.es6.js"
            )),
            PathBuf::from("/project/node_modules/pkg/dist/esm/node_modules/tslib/tslib.es6.js")
        );
        assert_eq!(
            lexical_normalize(Path::new(
                "/project/node_modules/pkg/dist/esm/packages/runtime/es/common/request/middleware/../../../../runtime-shared/node_modules/eventemitter3/index.js"
            )),
            PathBuf::from(
                "/project/node_modules/pkg/dist/esm/packages/runtime-shared/node_modules/eventemitter3/index.js"
            )
        );
    }

    #[wasm_bindgen_test]
    async fn test_read_bundled_node_modules_via_parent_relative_import() {
        let base = Path::new("/test_read_bundled_node_modules");
        let package_dir = base.join("node_modules/ad-runtime-integration-h5ap");
        let target_dir = base.join("stores/ad-runtime-integration-h5ap-4.26.12");
        let tslib_file = target_dir.join("dist/esm/node_modules/tslib/tslib.es6.js");
        let eventemitter_file =
            target_dir.join("dist/esm/packages/runtime-shared/node_modules/eventemitter3/index.js");
        let tslib_import_path = package_dir.join(
            "dist/esm/packages/runtime/es/common/request/middleware/../../../../../../node_modules/tslib/tslib.es6.js",
        );
        let eventemitter_import_path = package_dir.join(
            "dist/esm/packages/runtime/es/common/request/middleware/../../../../runtime-shared/node_modules/eventemitter3/index.js",
        );

        let _ = tokio_fs_ext::remove_dir_all(base).await;
        tokio_fs_ext::create_dir_all(tslib_file.parent().unwrap())
            .await
            .unwrap();
        tokio_fs_ext::create_dir_all(eventemitter_file.parent().unwrap())
            .await
            .unwrap();
        tokio_fs_ext::write(&tslib_file, b"export var __awaiter = 1;")
            .await
            .unwrap();
        tokio_fs_ext::write(&eventemitter_file, b"export default EventEmitter;")
            .await
            .unwrap();

        let project = OpfsProject::default();
        project
            .fuse_fs()
            .create_fuse_link(&target_dir, &package_dir)
            .await
            .unwrap();

        let tslib_content = project.read(&tslib_import_path).await.unwrap();
        assert_eq!(&tslib_content[..], b"export var __awaiter = 1;");

        let eventemitter_content = project.read(&eventemitter_import_path).await.unwrap();
        assert_eq!(&eventemitter_content[..], b"export default EventEmitter;");

        let _ = tokio_fs_ext::remove_dir_all(base).await;
    }
}
