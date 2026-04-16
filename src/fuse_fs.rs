//! Fuse-link filesystem layer.
//!
//! A "fuse link" is a small file (`fuse.link`) placed inside a
//! `node_modules/<pkg>/` directory. Its content points to an extracted
//! directory in the store, enabling transparent reads.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Error, ErrorKind, Read, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use flate2::read::GzDecoder;
use tar::Archive;
use tokio_fs_ext::DirEntry;
use tracing::warn;

// ── FuseLink (typed representation) ──────────────────────────────────────

/// Parsed representation of a `fuse.link` file.
///
/// Format on disk: a single line containing the absolute OPFS path to
/// the extracted package directory.
///
/// Wrapped in `Arc` when cached to avoid cloning `PathBuf` on every
/// cache hit.
#[derive(Debug, Clone)]
pub struct FuseLink {
    /// Absolute OPFS path to the extracted package directory.
    pub target_dir: PathBuf,
}

impl FuseLink {
    /// Parse the first line of a fuse.link file.
    pub fn parse(content: &str) -> Option<Self> {
        let line = content.lines().next()?.trim();
        if line.is_empty() {
            return None;
        }
        // Strip legacy `|prefix` suffix if present (backwards compat)
        let path = line.split_once('|').map_or(line, |(p, _)| p);
        Some(Self {
            target_dir: PathBuf::from(path),
        })
    }

    /// Serialise back to the on-disk format.
    pub fn to_content(&self) -> String {
        format!("{}\n", self.target_dir.display())
    }
}

/// Max concurrent OPFS writes during tgz extraction.
const EXTRACTION_CONCURRENCY: usize = 64;

// ── BoundedCache ─────────────────────────────────────────────────────────

/// A simple bounded cache with FIFO eviction.
///
/// Uses `HashMap` for O(1) lookups and `VecDeque` for insertion-order
/// tracking. When the cache is full, the oldest entry is evicted.
/// This replaces the previous random-eviction strategy without requiring
/// an external crate.
struct BoundedCache {
    map: HashMap<PathBuf, Arc<FuseLink>>,
    order: VecDeque<PathBuf>,
    capacity: usize,
}

impl BoundedCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity.min(256)),
            order: VecDeque::with_capacity(capacity.min(256)),
            capacity: capacity.max(1),
        }
    }

    fn get(&self, key: &Path) -> Option<&Arc<FuseLink>> {
        self.map.get(key)
    }

    fn put(&mut self, key: PathBuf, value: Arc<FuseLink>) {
        if self.map.contains_key(&key) {
            self.map.insert(key, value);
            return;
        }
        // Evict oldest if at capacity
        if self.map.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

// ── FuseFs ───────────────────────────────────────────────────────────────

/// Fuse-link aware filesystem overlay.
///
/// Uses a bounded FIFO cache to avoid repeated disk reads for fuse.link
/// files. When the cache is full, the oldest entry is evicted.
pub struct FuseFs {
    link_cache: RwLock<BoundedCache>,
}

impl FuseFs {
    pub fn new(fuse_cache_max_entries: usize) -> Self {
        Self {
            link_cache: RwLock::new(BoundedCache::new(fuse_cache_max_entries)),
        }
    }

    /// Create a fuse link: write `fuse.link` under `dst` pointing to `target_dir`.
    ///
    /// Skips the write when existing content already matches (e.g. re-install
    /// of same version). The cache is always refreshed so callers never
    /// observe a stale link after this returns, even on eviction.
    pub async fn create_fuse_link(&self, target_dir: &Path, dst: &Path) -> Result<()> {
        let fuse_link_path = dst.join("fuse.link");
        let link = Arc::new(FuseLink {
            target_dir: target_dir.to_path_buf(),
        });
        let new_bytes = link.to_content().into_bytes();

        let existing = tokio_fs_ext::read(&fuse_link_path).await.ok();
        if existing.as_deref() != Some(new_bytes.as_slice()) {
            tokio_fs_ext::create_dir_all(dst).await?;
            tokio_fs_ext::write(&fuse_link_path, &new_bytes).await?;
        }

        if let Ok(mut cache) = self.link_cache.write() {
            cache.put(fuse_link_path, link);
        } else {
            warn!("fuse link cache lock poisoned");
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

        let real_path = resolved.link.target_dir.join(&resolved.relative);
        match tokio_fs_ext::read(&real_path).await {
            Ok(v) => Ok(Some(Bytes::from(v))),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Try to read a directory through fuse-link indirection.
    ///
    /// Returns `Ok(None)` if the path has no fuse link.
    pub async fn try_read_dir(&self, path: &Path) -> Result<Option<Vec<DirEntry>>> {
        let resolved = match self.resolve(path).await? {
            Some(r) => r,
            None => return Ok(None),
        };

        let real_dir = resolved.link.target_dir.join(&resolved.relative);
        let target_entries = match read_dir_direct(&real_dir).await {
            Ok(entries) => entries,
            Err(_) => return Ok(None),
        };

        // Issue #7: Merge with real files only when necessary. Most fuse-linked
        // directories only contain `fuse.link`, making the full merge redundant.
        match read_dir_direct(path).await {
            Ok(original) => {
                // Fast path: check if any non-fuse.link entries exist
                let has_extra_files = original
                    .iter()
                    .any(|e| e.file_name().to_string_lossy() != "fuse.link");

                if !has_extra_files {
                    return Ok(Some(target_entries));
                }

                // Slow path: merge original entries with target entries
                let target_names: HashSet<_> =
                    target_entries.iter().map(|e| e.file_name()).collect();
                let mut combined: Vec<_> = original
                    .into_iter()
                    .filter(|e| {
                        e.file_name().to_string_lossy() != "fuse.link"
                            && !target_names.contains(&e.file_name())
                    })
                    .collect();
                combined.extend(target_entries);
                Ok(Some(combined))
            }
            Err(_) => Ok(Some(target_entries)),
        }
    }

    /// Try to get metadata through fuse-link indirection.
    ///
    /// Returns `Ok(None)` if the path has no fuse link.
    pub async fn try_metadata(&self, path: &Path) -> Result<Option<tokio_fs_ext::Metadata>> {
        let resolved = match self.resolve(path).await? {
            Some(r) => r,
            None => return Ok(None),
        };

        let real_path = resolved.link.target_dir.join(&resolved.relative);
        match tokio_fs_ext::metadata(&real_path).await {
            Ok(m) => Ok(Some(m)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Pre-populate the link cache for a known fuse link (avoids disk IO on cold read).
    ///
    /// Called during install after `create_fuse_link` to ensure the cache is warm.
    pub fn warm_link_cache(&self, dst: &Path, target_dir: &Path) {
        let Some(fuse_link_path) = locate_fuse_link_file(dst) else {
            return;
        };
        let link = Arc::new(FuseLink {
            target_dir: target_dir.to_path_buf(),
        });
        if let Ok(mut cache) = self.link_cache.write() {
            cache.put(fuse_link_path, link);
        }
    }

    /// Extract all files from a tgz into a real directory on disk.
    ///
    /// Uses streaming decompression — no full decompressed buffer in memory.
    /// Returns the extraction root directory (tgz path with `.tgz` stripped).
    pub async fn extract_tgz_to_dir(&self, tgz_path: &Path) -> Result<PathBuf> {
        let out_dir = tgz_path.with_extension(""); // strip .tgz
        let sentinel = PathBuf::from(format!("{}._resolved", out_dir.display()));

        // Skip if already extracted successfully
        if tokio_fs_ext::metadata(&sentinel).await.is_ok() {
            return Ok(out_dir);
        }

        // Issue #2 & #8: Phase 1 — Read all entries in a scoped block so that the
        // raw tgz bytes (`raw`), `GzDecoder`, and `Archive` are dropped immediately
        // after iteration, freeing compressed-data memory before the write phase.
        // Also collect unique parent directories for batch creation (Issue #8).
        struct PendingFile {
            path: PathBuf,
            content: Bytes,
        }
        let mut pending_files: Vec<PendingFile> = Vec::new();
        let mut unique_dirs: HashSet<PathBuf> = HashSet::new();

        {
            let raw = tokio_fs_ext::read(tgz_path).await?;
            let gz = GzDecoder::new(&raw[..]);
            let mut archive = Archive::new(gz);

            for entry_result in archive.entries()? {
                let mut entry = entry_result?;
                if !entry.header().entry_type().is_file() {
                    continue;
                }

                let path = entry.path()?.to_path_buf();

                // Security: reject absolute paths and path traversal attempts
                if path.is_absolute()
                    || path
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        format!("malicious path in tar entry: {}", path.display()),
                    ));
                }

                // Strip the first component (e.g. "package/") from tar paths
                let normalized = if let Some(first) = path.components().next() {
                    let stripped = path.strip_prefix(first).unwrap_or(&path);
                    if stripped.as_os_str().is_empty() {
                        path
                    } else {
                        stripped.to_path_buf()
                    }
                } else {
                    path
                };

                if normalized.as_os_str().is_empty() {
                    continue;
                }

                let mut content = Vec::new();
                entry.read_to_end(&mut content)?;

                let full_path = out_dir.join(normalized);
                if let Some(parent) = full_path.parent() {
                    unique_dirs.insert(parent.to_path_buf());
                }
                pending_files.push(PendingFile {
                    path: full_path,
                    content: Bytes::from(content),
                });
            }
        } // raw, gz, archive dropped here — frees tgz memory

        // Phase 2 — Create all unique parent directories (Issue #8: deduplication
        // avoids redundant create_dir_all calls for files in the same directory).
        for dir in &unique_dirs {
            tokio_fs_ext::create_dir_all(dir).await?;
        }

        // Phase 3 — Write files concurrently (no per-file create_dir_all needed).
        use futures::stream::{FuturesUnordered, StreamExt};
        let mut write_futures = FuturesUnordered::new();

        for pf in pending_files {
            write_futures.push(async move { tokio_fs_ext::write(&pf.path, &pf.content).await });

            // Keep concurrency in check
            if write_futures.len() >= EXTRACTION_CONCURRENCY {
                if let Some(res) = write_futures.next().await {
                    res?;
                }
            }
        }

        // Wait for remaining writes
        while let Some(res) = write_futures.next().await {
            res?;
        }

        // Mark extraction as complete
        tokio_fs_ext::write(&sentinel, b"").await?;

        Ok(out_dir)
    }

    /// Clear the fuse-link cache.
    pub fn clear(&self) {
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

    /// Read and parse a fuse.link file, using the bounded cache when available.
    ///
    /// Returns `Arc<FuseLink>` — cheap to clone (refcount bump only).
    async fn read_fuse_link(&self, fuse_link_path: &Path) -> Result<Option<Arc<FuseLink>>> {
        // Cache hit — lock briefly, then release
        if let Ok(cache) = self.link_cache.read() {
            if let Some(link) = cache.get(fuse_link_path) {
                return Ok(Some(Arc::clone(link)));
            }
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

        // Populate cache — Issue #6: double-check to avoid redundant insert
        // if another task populated the cache concurrently.
        if let Ok(mut cache) = self.link_cache.write() {
            if let Some(existing) = cache.get(fuse_link_path) {
                return Ok(Some(Arc::clone(existing)));
            }
            cache.put(fuse_link_path.to_path_buf(), Arc::clone(&link));
        }

        Ok(Some(link))
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

    let node_modules = OsStr::new("node_modules");

    // Issue #5: Fast path — use component iteration instead of
    // `to_string_lossy().contains()` to avoid UTF-8 conversion + allocation.
    if !path
        .components()
        .any(|c| matches!(c, Component::Normal(name) if name == node_modules))
    {
        return None;
    }

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
                    } else if pkg_str != "fuse.link" {
                        // Unscoped: 1 component (skip fuse.link itself)
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
    fn test_fuse_link_parse_target_dir() {
        let link = FuseLink::parse("/stores/lodash/-/lodash-4.17.21").unwrap();
        assert_eq!(
            link.target_dir,
            PathBuf::from("/stores/lodash/-/lodash-4.17.21")
        );
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_parse_strips_legacy_prefix() {
        // Legacy format: path|prefix — should strip the |prefix part
        let link = FuseLink::parse("/stores/lodash/-/lodash-4.17.21.tgz|package").unwrap();
        assert_eq!(
            link.target_dir,
            PathBuf::from("/stores/lodash/-/lodash-4.17.21.tgz")
        );
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_parse_empty() {
        assert!(FuseLink::parse("").is_none());
        assert!(FuseLink::parse("  \n").is_none());
    }

    #[wasm_bindgen_test]
    fn test_fuse_link_roundtrip() {
        let link = FuseLink {
            target_dir: PathBuf::from("/stores/foo/-/foo-1.0.0"),
        };
        let content = link.to_content();
        let parsed = FuseLink::parse(&content).unwrap();
        assert_eq!(parsed.target_dir, link.target_dir);
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
    fn test_locate_fuse_link_fast_path_skips_non_node_modules() {
        assert_eq!(locate_fuse_link_file(Path::new("/src/App.tsx")), None);
        assert_eq!(
            locate_fuse_link_file(Path::new("/project/lib/utils.js")),
            None
        );
        assert_eq!(locate_fuse_link_file(Path::new("package.json")), None);
    }

    // ── extract_tgz_to_dir tests ────────────────────────────────────

    /// Helper: create a test tgz at the given path.
    async fn write_test_tgz(tgz_path: &Path) {
        use crate::archive::{PackFile, gzip};
        let files = vec![
            PackFile::new("package/package.json", br#"{"name":"test"}"#.to_vec()),
            PackFile::new("package/index.js", b"module.exports = {}".to_vec()),
        ];
        let tgz = gzip(&files).unwrap();
        if let Some(parent) = tgz_path.parent() {
            let _ = tokio_fs_ext::create_dir_all(parent).await;
        }
        tokio_fs_ext::write(tgz_path, &tgz).await.unwrap();
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_creates_sentinel() {
        let base = Path::new("/test_extract_sentinel");
        let tgz_path = base.join("pkg-1.0.0.tgz");
        write_test_tgz(&tgz_path).await;

        let fs = FuseFs::new(100);
        let out = fs.extract_tgz_to_dir(&tgz_path).await.unwrap();

        // Extraction dir is tgz path with .tgz stripped
        assert_eq!(out, base.join("pkg-1.0.0"));

        // Sentinel exists
        let sentinel = PathBuf::from(format!("{}._resolved", out.display()));
        assert!(tokio_fs_ext::metadata(&sentinel).await.is_ok());

        // Extracted files exist
        assert!(
            tokio_fs_ext::metadata(&out.join("package.json"))
                .await
                .is_ok()
        );
        assert!(tokio_fs_ext::metadata(&out.join("index.js")).await.is_ok());

        // Cleanup
        let _ = tokio_fs_ext::remove_dir_all(base).await;
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_skips_when_sentinel_exists() {
        let base = Path::new("/test_extract_skip");
        let tgz_path = base.join("pkg-1.0.0.tgz");
        write_test_tgz(&tgz_path).await;

        let fs = FuseFs::new(100);

        // First extraction
        let out = fs.extract_tgz_to_dir(&tgz_path).await.unwrap();
        assert!(tokio_fs_ext::metadata(&out.join("index.js")).await.is_ok());

        // Delete a file to verify second call skips (doesn't re-extract)
        let _ = tokio_fs_ext::remove_file(&out.join("index.js")).await;

        // Second extraction — sentinel exists, should skip
        let out2 = fs.extract_tgz_to_dir(&tgz_path).await.unwrap();
        assert_eq!(out, out2);
        // File is still missing — confirms extraction was skipped
        assert!(tokio_fs_ext::metadata(&out.join("index.js")).await.is_err());

        // Cleanup
        let _ = tokio_fs_ext::remove_dir_all(base).await;
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_complex() {
        let base = Path::new("/test_extract_complex");
        let tgz_path = base.join("complex-1.0.0.tgz");

        use crate::archive::{PackFile, gzip};
        let mut files = Vec::new();
        for i in 0..100 {
            files.push(PackFile::new(
                format!("package/file_{}.txt", i),
                format!("content {}", i).into_bytes(),
            ));
        }
        files.push(PackFile::new(
            "package/nested/deep/file.js",
            b"console.log('deep')".to_vec(),
        ));

        let tgz = gzip(&files).unwrap();
        let _ = tokio_fs_ext::create_dir_all(base).await;
        tokio_fs_ext::write(&tgz_path, &tgz).await.unwrap();

        let fs = FuseFs::new(100);
        let out = fs.extract_tgz_to_dir(&tgz_path).await.unwrap();

        // Check a few files
        assert!(
            tokio_fs_ext::metadata(&out.join("file_0.txt"))
                .await
                .is_ok()
        );
        assert!(
            tokio_fs_ext::metadata(&out.join("file_49.txt"))
                .await
                .is_ok()
        );
        assert!(
            tokio_fs_ext::metadata(&out.join("file_99.txt"))
                .await
                .is_ok()
        );
        assert!(
            tokio_fs_ext::metadata(&out.join("nested/deep/file.js"))
                .await
                .is_ok()
        );

        let content = tokio_fs_ext::read_to_string(&out.join("nested/deep/file.js"))
            .await
            .unwrap();
        assert_eq!(content, "console.log('deep')");

        // Cleanup
        let _ = tokio_fs_ext::remove_dir_all(base).await;
    }

    #[wasm_bindgen_test]
    async fn test_extract_tgz_re_extracts_without_sentinel() {
        let base = Path::new("/test_extract_reextract");
        let tgz_path = base.join("pkg-1.0.0.tgz");
        write_test_tgz(&tgz_path).await;

        let fs = FuseFs::new(100);
        let out_dir = base.join("pkg-1.0.0");
        let sentinel = PathBuf::from(format!("{}._resolved", out_dir.display()));

        // Simulate incomplete extraction: create dir without sentinel
        tokio_fs_ext::create_dir_all(&out_dir).await.unwrap();
        assert!(tokio_fs_ext::metadata(&sentinel).await.is_err());

        // Should overwrite and re-extract (idempotent)
        let out = fs.extract_tgz_to_dir(&tgz_path).await.unwrap();
        assert_eq!(out, out_dir);

        // Sentinel now exists
        assert!(tokio_fs_ext::metadata(&sentinel).await.is_ok());
        // Files were extracted
        assert!(
            tokio_fs_ext::metadata(&out.join("package.json"))
                .await
                .is_ok()
        );

        // Cleanup
        let _ = tokio_fs_ext::remove_dir_all(base).await;
    }

    fn cached_target(fs: &FuseFs, path: &Path) -> Option<PathBuf> {
        fs.link_cache
            .read()
            .ok()?
            .get(path)
            .map(|arc| arc.target_dir.clone())
    }

    /// Scenario: package-lock.json upgrades lodash 4.0.0 -> 4.0.1.
    ///
    /// `dst` (node_modules/lodash) stays the same across versions, only
    /// `target_dir` changes. `create_fuse_link` must rewrite the stale
    /// link and refresh the cache so future reads resolve to 4.0.1.
    #[wasm_bindgen_test]
    async fn test_create_fuse_link_upgrade_rewrites_on_content_change() {
        let base = Path::new("/test_fuse_link_upgrade");
        let dst = base.join("node_modules/lodash");
        let fuse_link_path = dst.join("fuse.link");
        let target_v0 = PathBuf::from("/stores/lodash/-/lodash-4.0.0");
        let target_v1 = PathBuf::from("/stores/lodash/-/lodash-4.0.1");

        let fs = FuseFs::new(100);

        fs.create_fuse_link(&target_v0, &dst).await.unwrap();
        let content_v0 = tokio_fs_ext::read_to_string(&fuse_link_path).await.unwrap();
        assert_eq!(FuseLink::parse(&content_v0).unwrap().target_dir, target_v0);
        assert_eq!(cached_target(&fs, &fuse_link_path), Some(target_v0.clone()));

        fs.create_fuse_link(&target_v1, &dst).await.unwrap();
        let content_v1 = tokio_fs_ext::read_to_string(&fuse_link_path).await.unwrap();
        assert_eq!(
            FuseLink::parse(&content_v1).unwrap().target_dir,
            target_v1,
            "fuse.link on disk not updated on upgrade"
        );
        assert_eq!(
            cached_target(&fs, &fuse_link_path),
            Some(target_v1.clone()),
            "link cache still points at stale 4.0.0 after upgrade"
        );

        fs.create_fuse_link(&target_v1, &dst).await.unwrap();
        let content_again = tokio_fs_ext::read_to_string(&fuse_link_path).await.unwrap();
        assert_eq!(content_again, content_v1);

        // Fast path must re-warm the cache even when it skips the disk write.
        fs.link_cache.write().unwrap().clear();
        fs.create_fuse_link(&target_v1, &dst).await.unwrap();
        assert_eq!(cached_target(&fs, &fuse_link_path), Some(target_v1));

        let _ = tokio_fs_ext::remove_dir_all(base).await;
    }
}
