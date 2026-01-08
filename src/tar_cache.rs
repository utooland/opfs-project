//! LRU cache for tgz file extraction

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Error, ErrorKind, Read, Result};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use flate2::read::GzDecoder;
use tar::Archive;
use tokio_fs_ext::{DirEntry, FileType};
use web_time::Instant;

/// Strip the first path component (e.g., "package/" prefix in npm tarballs)
fn strip_first_component(path: &str) -> &str {
    path.find('/').map(|idx| &path[idx + 1..]).unwrap_or(path)
}

/// Default cache size: 100MB
const DEFAULT_MAX_SIZE: usize = 100 * 1024 * 1024;

/// Cached tgz content - all files from a single tgz
struct TgzCacheEntry {
    files: HashMap<String, Vec<u8>>,
    total_size: usize,
    last_accessed: Instant,
}

/// LRU cache for entire tgz packages
struct TarCache {
    cache: HashMap<PathBuf, TgzCacheEntry>,
    current_size: usize,
    max_size: usize,
}

impl TarCache {
    fn new(max_size: usize) -> Self {
        Self {
            cache: HashMap::new(),
            current_size: 0,
            max_size,
        }
    }

    fn get_file(&mut self, tgz_path: &Path, normalized_path: &str) -> Option<Vec<u8>> {
        if let Some(entry) = self.cache.get_mut(tgz_path) {
            entry.last_accessed = Instant::now();
            return entry.files.get(normalized_path).cloned();
        }
        None
    }

    fn has_tgz(&self, tgz_path: &Path) -> bool {
        self.cache.contains_key(tgz_path)
    }

    fn put_tgz(&mut self, tgz_path: PathBuf, files: HashMap<String, Vec<u8>>, total_size: usize) {
        if total_size > self.max_size {
            return;
        }

        while self.current_size + total_size > self.max_size && !self.cache.is_empty() {
            self.evict_oldest();
        }

        if let Some(old) = self.cache.remove(&tgz_path) {
            self.current_size -= old.total_size;
        }

        self.current_size += total_size;
        self.cache.insert(
            tgz_path,
            TgzCacheEntry {
                files,
                total_size,
                last_accessed: Instant::now(),
            },
        );
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .cache
            .iter()
            .min_by_key(|(_, e)| e.last_accessed)
            .map(|(k, _)| k.clone());

        if let Some(key) = oldest {
            if let Some(entry) = self.cache.remove(&key) {
                self.current_size -= entry.total_size;
            }
        }
    }

    fn list_dir(&mut self, tgz_path: &Path, normalized_dir: &str) -> Option<Vec<(String, bool)>> {
        let entry = self.cache.get_mut(tgz_path)?;
        entry.last_accessed = Instant::now();

        let prefix = if normalized_dir.is_empty() {
            String::new()
        } else {
            format!("{}/", normalized_dir)
        };

        let mut seen: HashMap<String, bool> = HashMap::new();
        for path in entry.files.keys() {
            let rel_path = if prefix.is_empty() {
                path.as_str()
            } else if let Some(rest) = path.strip_prefix(&prefix) {
                rest
            } else {
                continue;
            };

            if rel_path.is_empty() {
                continue;
            }

            if let Some(idx) = rel_path.find('/') {
                seen.entry(rel_path[..idx].to_string()).or_insert(true);
            } else {
                seen.entry(rel_path.to_string()).or_insert(false);
            }
        }

        Some(seen.into_iter().collect())
    }
}

static TAR_CACHE: LazyLock<RwLock<TarCache>> =
    LazyLock::new(|| RwLock::new(TarCache::new(DEFAULT_MAX_SIZE)));

async fn load_tgz_to_cache(tgz_path: &Path) -> Result<()> {
    let tgz_bytes = tokio_fs_ext::read(tgz_path).await?;
    let gz = GzDecoder::new(tgz_bytes.as_slice());
    let mut archive = Archive::new(gz);

    let mut files: HashMap<String, Vec<u8>> = HashMap::new();
    let mut total_size = 0usize;

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        if entry.header().entry_type().is_dir() {
            continue;
        }

        let path = entry.path()?;
        let normalized = strip_first_component(&path.to_string_lossy()).to_string();

        let mut content = Vec::new();
        entry.read_to_end(&mut content)?;
        total_size += content.len();
        files.insert(normalized, content);
    }

    if let Ok(mut cache) = TAR_CACHE.write() {
        cache.put_tgz(tgz_path.to_path_buf(), files, total_size);
    }

    Ok(())
}

/// Extract file with caching (caches entire tgz on first access)
pub async fn extract_file_cached(tgz_path: &Path, file_path: &str) -> Result<Vec<u8>> {
    let normalized_path = strip_first_component(file_path).to_string();

    // Check cache
    if let Ok(mut cache) = TAR_CACHE.write() {
        if let Some(content) = cache.get_file(tgz_path, &normalized_path) {
            return Ok(content);
        }
    }

    // Load tgz if not cached
    let needs_load = TAR_CACHE
        .read()
        .map(|c| !c.has_tgz(tgz_path))
        .unwrap_or(true);

    if needs_load {
        load_tgz_to_cache(tgz_path).await?;
    }

    // Try cache again
    if let Ok(mut cache) = TAR_CACHE.write() {
        if let Some(content) = cache.get_file(tgz_path, &normalized_path) {
            return Ok(content);
        }

        // Check if it's a directory
        if let Some(entry) = cache.cache.get(tgz_path) {
            let dir_prefix = format!("{}/", normalized_path);
            if entry.files.keys().any(|k| k.starts_with(&dir_prefix)) {
                return Err(Error::new(ErrorKind::IsADirectory, format!("{} is a directory", file_path)));
            }
        }
    }

    Err(Error::new(ErrorKind::NotFound, format!("File {} not found in {}", file_path, tgz_path.display())))
}

/// List directory with caching
pub async fn list_dir_cached(tgz_path: &Path, dir_path: &str) -> Result<Vec<DirEntry>> {
    let normalized_dir = strip_first_component(dir_path).to_string();

    let needs_load = TAR_CACHE
        .read()
        .map(|c| !c.has_tgz(tgz_path))
        .unwrap_or(true);

    if needs_load {
        load_tgz_to_cache(tgz_path).await?;
    }

    if let Ok(mut cache) = TAR_CACHE.write() {
        if let Some(items) = cache.list_dir(tgz_path, &normalized_dir) {
            return Ok(items
                .into_iter()
                .map(|(name, is_dir)| {
                    let path = if dir_path.is_empty() {
                        PathBuf::from(&name)
                    } else {
                        PathBuf::from(dir_path).join(&name)
                    };
                    DirEntry::new(
                        path,
                        OsString::from(&name),
                        if is_dir { FileType::Directory } else { FileType::File },
                    )
                })
                .collect());
        }
    }

    Ok(vec![])
}

/// Clear cache
pub fn clear_cache() {
    if let Ok(mut cache) = TAR_CACHE.write() {
        cache.cache.clear();
        cache.current_size = 0;
    }
}

/// Set maximum cache size (in bytes)
pub fn set_max_size(max_size_bytes: usize) {
    if let Ok(mut cache) = TAR_CACHE.write() {
        cache.max_size = max_size_bytes;
        while cache.current_size > cache.max_size && !cache.cache.is_empty() {
            cache.evict_oldest();
        }
    }
}
