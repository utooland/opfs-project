//! Fuse-link filesystem layer.
//!
//! A "fuse link" is a small file (`fuse.link`) placed inside a
//! `node_modules/<pkg>/` directory. Its content points to a tgz in the
//! store plus an optional prefix, enabling lazy on-demand extraction.

use std::collections::{HashMap, HashSet};
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use tokio_fs_ext::DirEntry;
use tracing::warn;

use crate::tar_index::{self, TarIndex};

// ── FuseLink (typed representation) ──────────────────────────────────────

/// Parsed representation of a `fuse.link` file.
///
/// Format on disk: `<tgz_path>` or `<tgz_path>|<prefix>`
///
/// Wrapped in `Arc` when cached to avoid cloning `PathBuf` + `String`
/// on every cache hit.
#[derive(Debug, Clone)]
pub struct FuseLink {
    /// Absolute OPFS path to the tgz in the store.
    pub tgz_path: PathBuf,
    /// Optional prefix directory inside the tgz (e.g. `"package"`).
    pub prefix: Option<String>,
}

impl FuseLink {
    /// Parse the first line of a fuse.link file.
    pub fn parse(content: &str) -> Option<Self> {
        let line = content.lines().next()?.trim();
        if line.is_empty() {
            return None;
        }
        if let Some((tgz, prefix)) = line.split_once('|') {
            Some(Self {
                tgz_path: PathBuf::from(tgz),
                prefix: Some(prefix.to_string()),
            })
        } else {
            Some(Self {
                tgz_path: PathBuf::from(line),
                prefix: None,
            })
        }
    }

    /// Serialise back to the on-disk format.
    pub fn to_content(&self) -> String {
        match &self.prefix {
            Some(p) => format!("{}|{p}\n", self.tgz_path.display()),
            None => format!("{}\n", self.tgz_path.display()),
        }
    }

    /// Resolve a relative path to a file path *inside* the tgz.
    ///
    /// Returns `None` if relative is empty (root = directory) or no prefix.
    pub fn file_in_tgz(&self, relative: &Path) -> Option<PathBuf> {
        let prefix = self.prefix.as_ref()?;
        if relative.as_os_str().is_empty() {
            None
        } else {
            Some(Path::new(prefix).join(relative))
        }
    }

    /// Resolve a relative path to a directory path *inside* the tgz.
    ///
    /// For root (`relative == ""`), returns empty `PathBuf`.
    /// Returns `None` when no prefix.
    pub fn dir_in_tgz(&self, relative: &Path) -> Option<PathBuf> {
        let prefix = self.prefix.as_ref()?;
        if relative.as_os_str().is_empty() {
            Some(PathBuf::new())
        } else {
            Some(Path::new(prefix).join(relative))
        }
    }

    /// Whether this link points into a tgz (has a prefix).
    pub fn is_tgz_mode(&self) -> bool {
        self.prefix.is_some()
    }
}

/// Max concurrent OPFS writes during tgz extraction.
const EXTRACTION_CONCURRENCY: usize = 32;

// ── FuseFs ───────────────────────────────────────────────────────────────

/// Fuse-link aware filesystem overlay.
///
/// Owns both the fuse-link path cache and the tar index.
pub struct FuseFs {
    /// Cache: fuse.link file path → parsed FuseLink (Arc to avoid cloning)
    link_cache: RwLock<HashMap<PathBuf, Arc<FuseLink>>>,
    tar_index: RwLock<TarIndex>,
    /// Tgz paths currently being loaded (prevents duplicate decompress).
    loading: RwLock<HashSet<PathBuf>>,
    max_link_cache: usize,
}

impl FuseFs {
    pub fn new(tar_cache_max_bytes: usize, fuse_cache_max_entries: usize) -> Self {
        Self {
            link_cache: RwLock::new(HashMap::new()),
            tar_index: RwLock::new(TarIndex::new(tar_cache_max_bytes)),
            loading: RwLock::new(HashSet::new()),
            max_link_cache: fuse_cache_max_entries,
        }
    }

    /// Create a fuse link file on disk and cache the mapping.
    pub async fn create_fuse_link(
        &self,
        tgz_path: &Path,
        dst: &Path,
        prefix: Option<&str>,
    ) -> Result<()> {
        tokio_fs_ext::create_dir_all(dst).await?;

        let fuse_link_path = locate_fuse_link_file(dst).ok_or_else(|| {
            Error::new(ErrorKind::InvalidInput, "cannot determine fuse.link path")
        })?;

        let link = Arc::new(FuseLink {
            tgz_path: tgz_path.to_path_buf(),
            prefix: prefix.map(String::from),
        });

        tokio_fs_ext::write(&fuse_link_path, link.to_content().as_bytes()).await?;

        if let Ok(mut cache) = self.link_cache.write() {
            if cache.len() >= self.max_link_cache {
                let to_remove: Vec<_> = cache.keys().take(cache.len() / 2).cloned().collect();
                for k in to_remove {
                    cache.remove(&k);
                }
            }
            cache.insert(fuse_link_path, link);
        } else {
            warn!("fuse link cache write lock poisoned");
        }
        Ok(())
    }

    /// Try to read a file through fuse-link indirection.
    ///
    /// Returns `Ok(None)` if the path has no fuse link.
    pub async fn try_read(&self, path: &Path) -> Result<Option<Bytes>> {
        let resolved = match self.resolve(path).await? {
            Some(r) => r,
            None => return Ok(None),
        };

        if !resolved.link.is_tgz_mode() {
            // Non-lazy: link points to an extracted directory, join relative path
            let real_path = resolved.link.tgz_path.join(&resolved.relative);
            return match tokio_fs_ext::read(&real_path).await {
                Ok(v) => Ok(Some(Bytes::from(v))),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            };
        }

        let file_path = match resolved.link.file_in_tgz(&resolved.relative) {
            Some(p) => p,
            None => {
                return Err(Error::new(
                    ErrorKind::IsADirectory,
                    "cannot read directory as file",
                ));
            }
        };

        self.extract_file(&resolved.link.tgz_path, &file_path)
            .await
            .map(Some)
    }

    /// Try to read a directory through fuse-link indirection.
    ///
    /// Returns `Ok(None)` if the path has no fuse link.
    pub async fn try_read_dir(&self, path: &Path) -> Result<Option<Vec<DirEntry>>> {
        let resolved = match self.resolve(path).await? {
            Some(r) => r,
            None => return Ok(None),
        };

        let target_entries = if let Some(dir_path) = resolved.link.dir_in_tgz(&resolved.relative) {
            self.list_dir_in_tgz(&resolved.link.tgz_path, &dir_path)
                .await
                .ok()
        } else {
            // Non-lazy: link points to an extracted directory, join relative path
            let real_dir = resolved.link.tgz_path.join(&resolved.relative);
            read_dir_direct(&real_dir).await.ok()
        };

        let Some(target_entries) = target_entries else {
            return Ok(None);
        };

        // Merge with any real files in the directory (excluding fuse.link itself)
        match read_dir_direct(path).await {
            Ok(original) => {
                let mut combined: Vec<_> = original
                    .into_iter()
                    .filter(|e| e.file_name().to_string_lossy() != "fuse.link")
                    .collect();
                combined.extend(target_entries);
                Ok(Some(combined))
            }
            Err(_) => Ok(Some(target_entries)),
        }
    }

    /// Pre-populate the link cache for a known fuse link (avoids disk IO on cold read).
    ///
    /// Called during install after `create_fuse_link` to ensure the cache is warm.
    pub fn warm_link_cache(&self, dst: &Path, tgz_path: &Path, prefix: Option<&str>) {
        let Some(fuse_link_path) = locate_fuse_link_file(dst) else {
            return;
        };
        let link = Arc::new(FuseLink {
            tgz_path: tgz_path.to_path_buf(),
            prefix: prefix.map(String::from),
        });
        if let Ok(mut cache) = self.link_cache.write() {
            cache.insert(fuse_link_path, link);
        }
    }

    /// Extract all files from a tgz into a real directory on disk.
    ///
    /// Used by non-lazy mode: after extraction, fuse links point to the
    /// extracted directory, so reads are plain filesystem reads.
    ///
    /// Returns the extraction root directory (tgz path with `.tgz` stripped).
    pub async fn extract_tgz_to_dir(&self, tgz_path: &Path) -> Result<PathBuf> {
        self.ensure_tgz_cached(tgz_path).await?;

        let out_dir = tgz_path.with_extension(""); // strip .tgz

        let files: Vec<(String, Bytes)> = {
            let idx = self.tar_index.read().map_err(|e| {
                warn!("tar index read lock poisoned: {e}");
                Error::other("tar index lock poisoned")
            })?;
            idx.all_files(tgz_path)
                .unwrap_or_default()
                .into_iter()
                .collect()
        };

        use futures::stream::{self, StreamExt};
        let out = out_dir.clone();

        stream::iter(files)
            .map(|(name, content)| {
                let dir = out.clone();
                async move {
                    let path = dir.join(&name);
                    if let Some(parent) = path.parent() {
                        let _ = tokio_fs_ext::create_dir_all(parent).await;
                    }
                    let _ = tokio_fs_ext::write(&path, &content).await;
                }
            })
            .buffer_unordered(EXTRACTION_CONCURRENCY)
            .collect::<Vec<()>>()
            .await;

        Ok(out_dir)
    }

    /// Clear both the tar index and the fuse-link cache.
    pub fn clear(&self) {
        if let Ok(mut idx) = self.tar_index.write() {
            idx.clear();
        }
        if let Ok(mut lc) = self.link_cache.write() {
            lc.clear();
        }
    }

    // ── private ──────────────────────────────────────────────────────

    /// Resolve a path to its fuse-link target (if one exists).
    async fn resolve(&self, path: &Path) -> Result<Option<Resolved>> {
        let fuse_link_path = match locate_fuse_link_file(path) {
            Some(p) => p,
            None => return Ok(None),
        };

        let link = match self.read_fuse_link(&fuse_link_path).await? {
            Some(l) => l,
            None => return Ok(None),
        };

        let fuse_dir = fuse_link_path
            .parent()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "invalid fuse.link path"))?;

        let relative = path
            .strip_prefix(fuse_dir)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "path not under fuse.link dir"))?
            .to_path_buf();

        Ok(Some(Resolved { link, relative }))
    }

    /// Read and parse a fuse.link file, using cache when available.
    ///
    /// Returns `Arc<FuseLink>` — cheap to clone (refcount bump only).
    async fn read_fuse_link(&self, fuse_link_path: &Path) -> Result<Option<Arc<FuseLink>>> {
        // Cache hit — read lock only
        if let Ok(cache) = self.link_cache.read()
            && let Some(link) = cache.get(fuse_link_path)
        {
            return Ok(Some(Arc::clone(link)));
        }

        // Cache miss — read from disk.
        // Any error means the fuse.link is absent or unreadable — treat as "no link".
        let content = match tokio_fs_ext::read_to_string(fuse_link_path).await {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };

        let link = match FuseLink::parse(&content) {
            Some(l) => Arc::new(l),
            None => return Ok(None),
        };

        // Populate cache — write lock (evict half when full, matching create_fuse_link)
        if let Ok(mut cache) = self.link_cache.write() {
            if cache.len() >= self.max_link_cache && !cache.contains_key(fuse_link_path) {
                let to_remove: Vec<_> = cache.keys().take(cache.len() / 2).cloned().collect();
                for k in to_remove {
                    cache.remove(&k);
                }
            }
            cache.insert(fuse_link_path.to_path_buf(), Arc::clone(&link));
        }

        Ok(Some(link))
    }

    /// Ensure a tgz is loaded into the tar index.
    ///
    /// Uses an in-flight set to prevent the same tgz from being read and
    /// decompressed concurrently by multiple callers.
    async fn ensure_tgz_cached(&self, tgz_path: &Path) -> Result<()> {
        // Fast path — already cached
        {
            let idx = self.tar_index.read().map_err(|e| {
                warn!("tar index read lock poisoned: {e}");
                Error::other("tar index lock poisoned")
            })?;
            if idx.has_tgz(tgz_path) {
                return Ok(());
            }
        }

        // Dedup: skip if another task is already loading this tgz
        {
            let loading = self.loading.read().map_err(|_| Error::other("lock"))?;
            if loading.contains(tgz_path) {
                // Another task is loading — return Ok and let caller retry
                // via extract_file's cache-miss → ensure_tgz_cached loop.
                return Ok(());
            }
        }
        // Mark as loading
        if let Ok(mut loading) = self.loading.write() {
            loading.insert(tgz_path.to_path_buf());
        }

        // Parse outside all locks
        let result = async {
            let raw = tokio_fs_ext::read(tgz_path).await?;
            tar_index::parse_tgz(&raw)
        }
        .await;

        // Unmark loading regardless of success/failure
        if let Ok(mut loading) = self.loading.write() {
            loading.remove(tgz_path);
        }

        let parsed = result?;

        // Insert — write lock (insert_tgz already skips if present)
        let mut idx = self.tar_index.write().map_err(|e| {
            warn!("tar index write lock poisoned: {e}");
            Error::other("tar index lock poisoned")
        })?;
        idx.insert_tgz(tgz_path.to_path_buf(), parsed);
        Ok(())
    }

    /// Extract a single file from a tgz.
    ///
    /// Lookup order:
    /// 1. In-memory tar index — O(1), zero IO (hot path)
    /// 2. Decompress tgz → populate tar index → retry
    async fn extract_file(&self, tgz_path: &Path, file_path: &Path) -> Result<Bytes> {
        let lossy = file_path.to_string_lossy();
        let normalized = tar_index::strip_first(&lossy);

        // 1. Try in-memory tar index (fastest, no IO)
        let from_index = {
            let idx = self.tar_index.read().map_err(|e| {
                warn!("tar index read lock poisoned: {e}");
                Error::other("tar index lock poisoned")
            })?;
            idx.get_file(tgz_path, normalized)
        };
        if let Some(content) = from_index {
            return Ok(content);
        }

        // 2. Cache miss — load tgz, then retry
        self.ensure_tgz_cached(tgz_path).await?;

        let (content, is_dir) = {
            let idx = self.tar_index.read().map_err(|e| {
                warn!("tar index read lock poisoned: {e}");
                Error::other("tar index lock poisoned")
            })?;
            (
                idx.get_file(tgz_path, normalized),
                idx.is_dir_in_tgz(tgz_path, normalized),
            )
        };

        if let Some(content) = content {
            return Ok(content);
        }

        if is_dir {
            return Err(Error::new(
                ErrorKind::IsADirectory,
                format!("{} is a directory", file_path.display()),
            ));
        }

        Err(Error::new(
            ErrorKind::NotFound,
            format!(
                "file {} not found in {}",
                file_path.display(),
                tgz_path.display()
            ),
        ))
    }

    /// List directory contents from a cached tgz.
    async fn list_dir_in_tgz(&self, tgz_path: &Path, dir_path: &Path) -> Result<Vec<DirEntry>> {
        let lossy = dir_path.to_string_lossy();
        let normalized = tar_index::strip_first(&lossy);
        self.ensure_tgz_cached(tgz_path).await?;

        let idx = self.tar_index.read().map_err(|e| {
            warn!("tar index read lock poisoned: {e}");
            Error::other("tar index lock poisoned")
        })?;

        match idx.list_dir(tgz_path, normalized) {
            Some(items) => Ok(tar_index::to_dir_entries(items, dir_path)),
            None => Ok(vec![]),
        }
    }
}

// ── Resolved ─────────────────────────────────────────────────────────────

/// A successfully resolved fuse-link lookup.
struct Resolved {
    link: Arc<FuseLink>,
    /// Path relative to the fuse-link root directory.
    relative: PathBuf,
}

// ── path helpers ─────────────────────────────────────────────────────────

/// Walk up from `path` to find the `node_modules/<pkg>/fuse.link` path.
///
/// Low-allocation: uses a small `Vec<&OsStr>` for component tracking,
/// only allocates the final `PathBuf` result.
fn locate_fuse_link_file(path: &Path) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::path::Component;

    // Fast path: if the path doesn't contain "node_modules", there's no fuse link.
    if !path.to_string_lossy().contains("node_modules") {
        return None;
    }

    let node_modules = OsStr::new("node_modules");

    // We only care about the last `node_modules` in the path.
    // e.g. /a/node_modules/b/node_modules/pkg/src/index.js -> we want `pkg`
    let mut comps = path.components();
    let mut pkg_components: Vec<&OsStr> = Vec::new();

    // Iterate backwards. As soon as we hit `node_modules`, the components we
    // just traversed *must* contain the package name at the front.
    while let Some(comp) = comps.next_back() {
        if let Component::Normal(name) = comp {
            if name == node_modules {
                // We found `node_modules`. Now look at the components that came after it.
                // Because we traversed backwards, pkg_components is reversed.
                // The actual package name is at the *end* of pkg_components.
                if let Some(pkg) = pkg_components.last().copied() {
                    let pkg_str = pkg.to_string_lossy();
                    if pkg_str.starts_with('@') {
                        // Scoped: it needs 2 components (@scope and pkg name)
                        if pkg_components.len() >= 2 {
                            let scope = pkg;
                            let name = pkg_components[pkg_components.len() - 2];
                            let mut base = comps.as_path().to_path_buf();
                            base.push("node_modules");
                            base.push(scope);
                            base.push(name);
                            base.push("fuse.link");
                            return Some(base);
                        }
                    } else {
                        // Unscoped: 1 component
                        let mut base = comps.as_path().to_path_buf();
                        base.push("node_modules");
                        base.push(pkg);
                        base.push("fuse.link");
                        return Some(base);
                    }
                }
            } else {
                pkg_components.push(name);
            }
        }
    }
    None
}

/// Read directory directly (no fuse-link logic).
async fn read_dir_direct(path: &Path) -> Result<Vec<DirEntry>> {
    tokio_fs_ext::read_dir(path).await?.collect()
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_fuse_link_parse_with_prefix() {
        let link = FuseLink::parse("/stores/lodash/-/lodash-4.17.21.tgz|package").unwrap();
        assert_eq!(
            link.tgz_path,
            PathBuf::from("/stores/lodash/-/lodash-4.17.21.tgz")
        );
        assert_eq!(link.prefix.as_deref(), Some("package"));
        assert!(link.is_tgz_mode());
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_parse_without_prefix() {
        let link = FuseLink::parse("/stores/lodash/-/lodash-4.17.21.tgz\n").unwrap();
        assert_eq!(
            link.tgz_path,
            PathBuf::from("/stores/lodash/-/lodash-4.17.21.tgz")
        );
        assert!(link.prefix.is_none());
        assert!(!link.is_tgz_mode());
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_parse_empty() {
        assert!(FuseLink::parse("").is_none());
        assert!(FuseLink::parse("  \n").is_none());
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_roundtrip() {
        let link = FuseLink {
            tgz_path: PathBuf::from("/stores/foo/-/foo-1.0.0.tgz"),
            prefix: Some("package".to_string()),
        };
        let content = link.to_content();
        let parsed = FuseLink::parse(&content).unwrap();
        assert_eq!(parsed.tgz_path, link.tgz_path);
        assert_eq!(parsed.prefix, link.prefix);
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_file_in_tgz() {
        let link = FuseLink {
            tgz_path: PathBuf::from("/stores/foo.tgz"),
            prefix: Some("package".into()),
        };
        assert_eq!(
            link.file_in_tgz(Path::new("index.js")),
            Some(PathBuf::from("package/index.js"))
        );
        assert_eq!(link.file_in_tgz(Path::new("")), None);
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_dir_in_tgz() {
        let link = FuseLink {
            tgz_path: PathBuf::from("/stores/foo.tgz"),
            prefix: Some("package".into()),
        };
        assert_eq!(link.dir_in_tgz(Path::new("")), Some(PathBuf::new()));
        assert_eq!(
            link.dir_in_tgz(Path::new("lib")),
            Some(PathBuf::from("package/lib"))
        );
    }

    #[wasm_bindgen_test]
    fn test_locate_fuse_link_basic() {
        assert_eq!(
            locate_fuse_link_file(Path::new("./node_modules/c/index.js")),
            Some(PathBuf::from("./node_modules/c/fuse.link"))
        );
    }

    #[wasm_bindgen_test]
    fn test_locate_fuse_link_scoped() {
        assert_eq!(
            locate_fuse_link_file(Path::new("./node_modules/@a/b/package.json")),
            Some(PathBuf::from("./node_modules/@a/b/fuse.link"))
        );
    }

    #[wasm_bindgen_test]
    fn test_locate_fuse_link_nested_node_modules() {
        // Hoisted deps: node_modules/a/node_modules/b/lib/index.js
        assert_eq!(
            locate_fuse_link_file(Path::new("./node_modules/a/node_modules/b/lib/index.js")),
            Some(PathBuf::from("./node_modules/a/node_modules/b/fuse.link"))
        );
    }

    #[wasm_bindgen_test]
    fn test_locate_fuse_link_none() {
        assert_eq!(locate_fuse_link_file(Path::new("./some/other/path")), None);
        assert_eq!(locate_fuse_link_file(Path::new("./src/index.js")), None);
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_no_prefix_returns_none() {
        let link = FuseLink {
            tgz_path: PathBuf::from("/stores/foo.tgz"),
            prefix: None,
        };
        assert_eq!(link.file_in_tgz(Path::new("index.js")), None);
        assert_eq!(link.dir_in_tgz(Path::new("")), None);
        assert!(!link.is_tgz_mode());
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_deep_relative_path() {
        let link = FuseLink {
            tgz_path: PathBuf::from("/stores/react.tgz"),
            prefix: Some("package".into()),
        };
        assert_eq!(
            link.file_in_tgz(Path::new("cjs/react.production.min.js")),
            Some(PathBuf::from("package/cjs/react.production.min.js"))
        );
        assert_eq!(
            link.dir_in_tgz(Path::new("cjs")),
            Some(PathBuf::from("package/cjs"))
        );
    }

    #[wasm_bindgen_test]
    fn test_locate_fuse_link_fast_path_skips_non_node_modules() {
        // Paths without node_modules should return None immediately
        assert_eq!(locate_fuse_link_file(Path::new("/src/App.tsx")), None);
        assert_eq!(
            locate_fuse_link_file(Path::new("/project/lib/utils.js")),
            None
        );
        assert_eq!(locate_fuse_link_file(Path::new("package.json")), None);
    }
}
