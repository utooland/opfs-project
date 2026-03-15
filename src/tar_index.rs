//! In-memory LRU index of tgz file contents.
//!
//! ## Path normalisation contract
//!
//! Tar entries inside npm tgz files have a first-level directory prefix
//! (e.g. `package/index.js`). This module strips that prefix during parsing
//! via [`strip_first_component`], so all cache keys are stored without the
//! prefix (e.g. `index.js`).

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{Read, Result};
use std::path::{Path, PathBuf};

use bytes::Bytes;
use flate2::read::GzDecoder;
use tar::Archive;
use tokio_fs_ext::{DirEntry, FileType};
use wasmtimer::std::Instant;

// ── helpers ──────────────────────────────────────────────────────────────

fn strip_first_component(path: &str) -> &str {
    path.find('/').map(|idx| &path[idx + 1..]).unwrap_or(path)
}

/// Strip first path component (public, for use in fuse_fs).
pub fn strip_first(path: &str) -> &str {
    strip_first_component(path)
}

/// `dir_path → Vec<(child_name, is_dir)>`.
/// Root is keyed by empty string `""`.
type DirChildren = HashMap<String, Vec<(String, bool)>>;

// ── cache entry ──────────────────────────────────────────────────────────

struct TgzEntry {
    /// The full decompressed tar data. Individual files are zero-copy slices.
    tar_data: Bytes,
    /// file path → (offset, length) into `tar_data`
    file_index: HashMap<String, (usize, usize)>,
    /// All directory paths inside this tgz (O(1) lookup).
    dirs: HashSet<String>,
    /// Pre-built directory listing: dir → children (O(1) lookup).
    children: DirChildren,
    /// Total bytes of actual file content (for LRU budget accounting).
    total_size: usize,
    last_accessed: Instant,
}

// ── TarIndex ─────────────────────────────────────────────────────────────

/// In-memory LRU cache for extracted tgz contents.
///
/// File contents are served via zero-copy `Bytes::slice()` from a single
/// decompressed tar buffer per tgz — no per-file heap allocations.
///
/// The owning [`FuseFs`](crate::fuse_fs::FuseFs) wraps this in a `RwLock`.
///
/// All read methods take `&self` so they can be called under a read lock.
pub struct TarIndex {
    cache: HashMap<PathBuf, TgzEntry>,
    current_size: usize,
    max_size: usize,
}

impl TarIndex {
    pub fn new(max_size: usize) -> Self {
        Self {
            cache: HashMap::new(),
            current_size: 0,
            max_size,
        }
    }

    /// Check whether a tgz is already loaded.
    pub fn has_tgz(&self, tgz_path: &Path) -> bool {
        self.cache.contains_key(tgz_path)
    }

    /// Get a single file from the cache — O(1), zero-copy.
    ///
    /// Returns a `Bytes::slice()` into the decompressed tar buffer.
    pub fn get_file(&self, tgz_path: &Path, normalized_path: &str) -> Option<Bytes> {
        let entry = self.cache.get(tgz_path)?;
        let &(offset, len) = entry.file_index.get(normalized_path)?;
        Some(entry.tar_data.slice(offset..offset + len))
    }

    /// Insert a fully-parsed tgz. Evicts LRU entries if over budget.
    pub fn insert_tgz(&mut self, tgz_path: PathBuf, parsed: ParsedTgz) {
        if self.cache.contains_key(&tgz_path) || parsed.total_size > self.max_size {
            return;
        }
        while self.current_size + parsed.total_size > self.max_size && !self.cache.is_empty() {
            self.evict_oldest();
        }
        self.current_size += parsed.total_size;
        self.cache.insert(
            tgz_path,
            TgzEntry {
                tar_data: parsed.tar_data,
                file_index: parsed.file_index,
                dirs: parsed.dirs,
                children: parsed.children,
                total_size: parsed.total_size,
                last_accessed: Instant::now(),
            },
        );
    }

    /// List immediate children of a directory — O(1) lookup.
    ///
    /// Returns `(name, is_dir)` pairs, or `None` if the tgz is not cached.
    pub fn list_dir(&self, tgz_path: &Path, normalized_dir: &str) -> Option<Vec<(String, bool)>> {
        let entry = self.cache.get(tgz_path)?;
        entry.children.get(normalized_dir).cloned()
    }

    /// O(1) check whether a normalised path is a directory.
    pub fn is_dir_in_tgz(&self, tgz_path: &Path, normalized_path: &str) -> bool {
        self.cache
            .get(tgz_path)
            .is_some_and(|entry| entry.dirs.contains(normalized_path))
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.current_size = 0;
    }

    /// Return all (file_name, content) pairs from a cached tgz.
    ///
    /// Used by eager extraction to write all files to disk at once.
    pub fn all_files(&self, tgz_path: &Path) -> Option<Vec<(String, Bytes)>> {
        let entry = self.cache.get(tgz_path)?;
        Some(
            entry
                .file_index
                .iter()
                .map(|(name, &(offset, len))| {
                    (name.clone(), entry.tar_data.slice(offset..offset + len))
                })
                .collect(),
        )
    }

    // ── private ──

    fn evict_oldest(&mut self) {
        let oldest = self
            .cache
            .iter()
            .min_by_key(|(_, e)| e.last_accessed)
            .map(|(k, _)| k.clone());

        if let Some(key) = oldest
            && let Some(entry) = self.cache.remove(&key)
        {
            self.current_size -= entry.total_size;
        }
    }
}

// ── ParsedTgz ────────────────────────────────────────────────────────────

/// Output of [`parse_tgz`] — owns all data needed by [`TarIndex::insert_tgz`].
pub struct ParsedTgz {
    /// The full decompressed tar data (single allocation).
    pub tar_data: Bytes,
    /// File name → (offset, length) into `tar_data`.
    pub file_index: HashMap<String, (usize, usize)>,
    pub dirs: HashSet<String>,
    pub children: DirChildren,
    pub total_size: usize,
}

// ── parsing (sync, no locks) ─────────────────────────────────────────────

/// Parse tgz bytes into all index structures at once.
///
/// Decompresses gzip into a single buffer, then builds a file-offset index.
/// All subsequent file reads are zero-copy via `Bytes::slice()`.
pub fn parse_tgz(tgz_bytes: &[u8]) -> Result<ParsedTgz> {
    // Step 1: decompress gzip into a single buffer
    let mut tar_data = Vec::new();
    GzDecoder::new(tgz_bytes).read_to_end(&mut tar_data)?;
    let tar_data = Bytes::from(tar_data);

    // Step 2: parse tar to build file index (offset + length)
    let mut file_index: HashMap<String, (usize, usize)> = HashMap::new();
    let mut dirs = HashSet::new();

    let mut archive = Archive::new(tar_data.as_ref());
    for entry_result in archive.entries()? {
        let entry = entry_result?;
        let path = entry.path()?;
        let lossy = path.to_string_lossy();
        let normalized = strip_first_component(&lossy);

        if entry.header().entry_type().is_dir() {
            let trimmed = normalized.trim_end_matches('/');
            if !trimmed.is_empty() {
                dirs.insert(trimmed.to_string());
            }
            continue;
        }

        let normalized = normalized.to_string();

        // Register all ancestor directories
        let mut ancestor = normalized.as_str();
        while let Some(idx) = ancestor.rfind('/') {
            ancestor = &ancestor[..idx];
            if !dirs.insert(ancestor.to_string()) {
                break;
            }
        }

        let offset = entry.raw_file_position() as usize;
        let size = entry.size() as usize;
        file_index.insert(normalized, (offset, size));
    }

    // Step 3: build children index
    let file_names: Vec<&str> = file_index.keys().map(|s| s.as_str()).collect();
    let children = build_children_index(&file_names, &dirs);

    // total_size = decompressed tar buffer size (actual memory consumed)
    let total_size = tar_data.len();

    Ok(ParsedTgz {
        tar_data,
        file_index,
        dirs,
        children,
        total_size,
    })
}

/// Build `dir → Vec<(child_name, is_dir)>` from the file paths and dir set.
fn build_children_index(file_paths: &[&str], dirs: &HashSet<String>) -> DirChildren {
    let mut children: DirChildren = HashMap::new();

    // Ensure root entry exists
    children.entry(String::new()).or_default();

    // Register file children
    for path in file_paths {
        let (parent, name) = match path.rfind('/') {
            Some(idx) => (&path[..idx], &path[idx + 1..]),
            None => ("", *path),
        };
        children
            .entry(parent.to_string())
            .or_default()
            .push((name.to_string(), false));
    }

    // Register directory children (dirs that are direct children of another dir)
    for dir in dirs {
        let (parent, name) = match dir.rfind('/') {
            Some(idx) => (&dir[..idx], &dir[idx + 1..]),
            None => ("", dir.as_str()),
        };
        let entry = children.entry(parent.to_string()).or_default();
        // Avoid duplicates — a dir might already be listed as a child
        if !entry.iter().any(|(n, _)| n == name) {
            entry.push((name.to_string(), true));
        }
    }

    children
}

// ── public helpers for converting list_dir output ────────────────────────

/// Convert `(name, is_dir)` pairs into `DirEntry` values.
pub fn to_dir_entries(items: Vec<(String, bool)>, base_path: &Path) -> Vec<DirEntry> {
    items
        .into_iter()
        .map(|(name, is_dir)| {
            let path = if base_path.as_os_str().is_empty() {
                PathBuf::from(&name)
            } else {
                base_path.join(&name)
            };
            DirEntry::new(
                path,
                OsString::from(&name),
                if is_dir {
                    FileType::Directory
                } else {
                    FileType::File
                },
            )
        })
        .collect()
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;
    use wasm_bindgen_test::*;

    /// Helper: create a tgz with a `package/` prefix (like a real npm tgz).
    fn make_test_tgz() -> Vec<u8> {
        use crate::archive::{PackFile, gzip};
        gzip(&[
            PackFile::new("package/index.js", b"console.log('hello')".to_vec()),
            PackFile::new("package/lib/utils.js", b"export default 42".to_vec()),
            PackFile::new("package/package.json", b"{}".to_vec()),
        ])
        .unwrap()
    }

    fn make_test_index() -> (TarIndex, PathBuf) {
        let tgz = make_test_tgz();
        let parsed = parse_tgz(&tgz).unwrap();
        let mut index = TarIndex::new(100 * 1024 * 1024);
        let path = PathBuf::from("/stores/test.tgz");
        index.insert_tgz(path.clone(), parsed);
        (index, path)
    }

    #[wasm_bindgen_test]
    fn test_strip_first_component() {
        assert_eq!(strip_first_component("package/index.js"), "index.js");
        assert_eq!(strip_first_component("file.txt"), "file.txt");
    }

    #[wasm_bindgen_test]
    fn test_parse_tgz() {
        let tgz = make_test_tgz();
        let parsed = parse_tgz(&tgz).unwrap();

        assert_eq!(parsed.file_index.len(), 3);
        assert!(parsed.file_index.contains_key("index.js"));
        assert!(parsed.file_index.contains_key("lib/utils.js"));
        assert!(parsed.dirs.contains("lib"));
    }

    #[wasm_bindgen_test]
    fn test_index_get_file() {
        let (index, path) = make_test_index();

        let content = index.get_file(&path, "index.js").unwrap();
        assert_eq!(&content[..], b"console.log('hello')");
        assert!(index.get_file(&path, "nonexistent.js").is_none());
    }

    #[wasm_bindgen_test]
    fn test_index_is_dir() {
        let (index, path) = make_test_index();

        assert!(index.is_dir_in_tgz(&path, "lib"));
        assert!(!index.is_dir_in_tgz(&path, "index.js"));
    }

    #[wasm_bindgen_test]
    fn test_index_list_dir() {
        let (index, path) = make_test_index();

        // Root
        let root = index.list_dir(&path, "").expect("should list root");
        let names: Vec<&str> = root.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"index.js"));
        assert!(names.contains(&"lib"));
        let lib = root.iter().find(|(n, _)| n == "lib").unwrap();
        assert!(lib.1); // is_dir

        // Subdir
        let lib_items = index.list_dir(&path, "lib").unwrap();
        assert_eq!(lib_items.len(), 1);
        assert_eq!(lib_items[0].0, "utils.js");
    }

    #[wasm_bindgen_test]
    fn test_index_rejects_oversized() {
        let mut index = TarIndex::new(50);
        let path = PathBuf::from("/stores/test.tgz");
        let parsed = parse_tgz(&make_test_tgz()).unwrap();
        index.insert_tgz(path.clone(), parsed);
        assert!(!index.has_tgz(&path));
    }

    #[wasm_bindgen_test]
    fn test_lru_eviction() {
        use crate::archive::{PackFile, gzip};

        // Create two small tgz files
        let tgz_a = gzip(&[PackFile::new("package/a.js", b"aaa".to_vec())]).unwrap();
        let tgz_b = gzip(&[PackFile::new("package/b.js", b"bbb".to_vec())]).unwrap();

        let parsed_a = parse_tgz(&tgz_a).unwrap();
        let parsed_b = parse_tgz(&tgz_b).unwrap();
        let budget = parsed_a.total_size + parsed_b.total_size - 1; // just under both

        let mut index = TarIndex::new(budget);
        let path_a = PathBuf::from("/stores/a.tgz");
        let path_b = PathBuf::from("/stores/b.tgz");

        index.insert_tgz(path_a.clone(), parsed_a);
        assert!(index.has_tgz(&path_a));

        // Inserting b should evict a (oldest)
        index.insert_tgz(path_b.clone(), parsed_b);
        assert!(index.has_tgz(&path_b));
        assert!(!index.has_tgz(&path_a), "oldest entry should be evicted");
    }

    #[wasm_bindgen_test]
    fn test_deeply_nested_tgz() {
        use crate::archive::{PackFile, gzip};

        let tgz = gzip(&[
            PackFile::new(
                "package/src/components/Button.tsx",
                b"export Button".to_vec(),
            ),
            PackFile::new("package/src/components/Input.tsx", b"export Input".to_vec()),
            PackFile::new("package/src/index.ts", b"export *".to_vec()),
        ])
        .unwrap();

        let parsed = parse_tgz(&tgz).unwrap();

        // Directory structure: src, src/components
        assert!(parsed.dirs.contains("src"));
        assert!(parsed.dirs.contains("src/components"));

        // Children index
        let src = parsed.children.get("src").expect("src dir");
        let src_names: Vec<&str> = src.iter().map(|(n, _)| n.as_str()).collect();
        assert!(src_names.contains(&"index.ts"));
        assert!(src_names.contains(&"components"));

        let comps = parsed
            .children
            .get("src/components")
            .expect("components dir");
        assert_eq!(comps.len(), 2);

        // Through TarIndex
        let mut index = TarIndex::new(100 * 1024 * 1024);
        let path = PathBuf::from("/stores/deep.tgz");
        index.insert_tgz(path.clone(), parsed);

        assert!(index.is_dir_in_tgz(&path, "src/components"));
        assert!(!index.is_dir_in_tgz(&path, "src/components/Button.tsx"));

        let content = index.get_file(&path, "src/components/Button.tsx").unwrap();
        assert_eq!(&content[..], b"export Button");
    }
}
