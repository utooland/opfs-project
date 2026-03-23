//! Fuse-link filesystem layer.
//!
//! A "fuse link" is a small file (`fuse.link`) placed inside a
//! `node_modules/<pkg>/` directory. Its content points to an extracted
//! directory in the store, enabling transparent reads.

use std::collections::HashMap;
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
const EXTRACTION_CONCURRENCY: usize = 32;

// ── FuseFs ───────────────────────────────────────────────────────────────

/// Fuse-link aware filesystem overlay.
pub struct FuseFs {
    /// Cache: fuse.link file path → parsed FuseLink (Arc to avoid cloning)
    link_cache: RwLock<HashMap<PathBuf, Arc<FuseLink>>>,
    max_link_cache: usize,
}

impl FuseFs {
    pub fn new(fuse_cache_max_entries: usize) -> Self {
        Self {
            link_cache: RwLock::new(HashMap::new()),
            max_link_cache: fuse_cache_max_entries,
        }
    }

    /// Create a fuse link: write `fuse.link` under `dst` pointing to `target_dir`.
    pub async fn create_fuse_link(&self, target_dir: &Path, dst: &Path) -> Result<()> {
        let fuse_link_path = dst.join("fuse.link");

        // Ensure the destination directory exists
        let parent = fuse_link_path.parent().ok_or_else(|| {
            Error::new(ErrorKind::InvalidInput, "cannot determine fuse.link path")
        })?;
        tokio_fs_ext::create_dir_all(parent).await?;

        let link = Arc::new(FuseLink {
            target_dir: target_dir.to_path_buf(),
        });

        tokio_fs_ext::write(&fuse_link_path, link.to_content().as_bytes()).await?;

        if let Ok(mut cache) = self.link_cache.write() {
            if cache.len() >= self.max_link_cache && !cache.contains_key(&fuse_link_path) {
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

        // Merge with any real files in the directory, deduplicating by name.
        // Target entries (from extracted package) take priority.
        match read_dir_direct(path).await {
            Ok(original) => {
                let target_names: std::collections::HashSet<_> = target_entries
                    .iter()
                    .map(|e| e.file_name())
                    .collect();
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
            if cache.len() >= self.max_link_cache && !cache.contains_key(&fuse_link_path) {
                let to_remove: Vec<_> = cache.keys().take(cache.len() / 2).cloned().collect();
                for k in to_remove {
                    cache.remove(&k);
                }
            }
            cache.insert(fuse_link_path, link);
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

        let entries = {
            let raw = tokio_fs_ext::read(tgz_path).await?;
            extract_entries_from_tgz(&raw)?
        };

        use futures::stream::{self, StreamExt};
        let out = out_dir.clone();

        let results: Vec<Result<()>> = stream::iter(entries)
            .map(|(name, content)| {
                let dir = out.clone();
                async move {
                    let path = dir.join(&name);
                    if let Some(parent) = path.parent() {
                        tokio_fs_ext::create_dir_all(parent).await?;
                    }
                    tokio_fs_ext::write(&path, &content).await
                }
            })
            .buffer_unordered(EXTRACTION_CONCURRENCY)
            .collect()
            .await;

        // Propagate any write errors — do NOT write sentinel if extraction is incomplete
        for result in results {
            result?;
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

}

/// Parse tgz bytes by streaming through tar entries.
/// GzDecoder decompresses on-the-fly — no full decompressed buffer.
fn extract_entries_from_tgz(tgz_bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let gz = GzDecoder::new(tgz_bytes);
    let mut archive = Archive::new(gz);
    let mut entries = Vec::new();

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().to_string();
        // Strip the first component (e.g. "package/") from tar paths
        let normalized = path
            .find('/')
            .map(|idx| &path[idx + 1..])
            .unwrap_or(&path);
        if normalized.is_empty() {
            continue;
        }
        let mut content = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut content)?;
        entries.push((normalized.to_string(), content));
    }

    Ok(entries)
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
        use crate::archive::{gzip, PackFile};
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
        assert!(tokio_fs_ext::metadata(&out.join("package.json")).await.is_ok());
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
        assert!(tokio_fs_ext::metadata(&out.join("package.json")).await.is_ok());

        // Cleanup
        let _ = tokio_fs_ext::remove_dir_all(base).await;
    }
}
