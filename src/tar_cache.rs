//! LRU cache for tgz file extraction

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Error, ErrorKind, Read, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use flate2::read::GzDecoder;
use tar::Archive;
use tokio_fs_ext::{DirEntry, FileType};
use web_time::Instant;

const DEFAULT_MAX_SIZE: usize = 100 * 1024 * 1024; // 100MB

fn strip_first_component(path: &str) -> &str {
    path.find('/').map(|idx| &path[idx + 1..]).unwrap_or(path)
}

struct TgzCacheEntry {
    files: HashMap<String, Arc<Vec<u8>>>,
    total_size: usize,
    last_accessed: Instant,
}

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

    fn get_file(&mut self, tgz_path: &Path, normalized_path: &str) -> Option<Arc<Vec<u8>>> {
        let entry = self.cache.get_mut(tgz_path)?;
        entry.last_accessed = Instant::now();
        entry.files.get(normalized_path).cloned()
    }

    fn has_tgz(&self, tgz_path: &Path) -> bool {
        self.cache.contains_key(tgz_path)
    }

    fn insert_tgz(&mut self, tgz_path: PathBuf, files: HashMap<String, Arc<Vec<u8>>>, total_size: usize) {
        // Skip if already cached (double-check)
        if self.cache.contains_key(&tgz_path) {
            return;
        }

        // Skip if too large
        if total_size > self.max_size {
            return;
        }

        // Evict until we have space
        while self.current_size + total_size > self.max_size && !self.cache.is_empty() {
            self.evict_oldest();
        }

        self.current_size += total_size;
        self.cache.insert(tgz_path, TgzCacheEntry {
            files,
            total_size,
            last_accessed: Instant::now(),
        });
    }

    fn evict_oldest(&mut self) {
        let oldest = self.cache.iter()
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

/// Parse tgz bytes into files map (sync, no lock held)
fn parse_tgz(tgz_bytes: &[u8]) -> Result<(HashMap<String, Arc<Vec<u8>>>, usize)> {
    let gz = GzDecoder::new(tgz_bytes);
    let mut archive = Archive::new(gz);

    let mut files = HashMap::new();
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
        files.insert(normalized, Arc::new(content));
    }

    Ok((files, total_size))
}

/// Ensure tgz is loaded into cache (with double-checked locking)
async fn ensure_tgz_cached(tgz_path: &Path) -> Result<()> {
    // First check with read lock
    if TAR_CACHE.read().map(|c| c.has_tgz(tgz_path)).unwrap_or(false) {
        return Ok(());
    }

    // Read file without holding lock
    let tgz_bytes = tokio_fs_ext::read(tgz_path).await?;
    let (files, total_size) = parse_tgz(&tgz_bytes)?;

    // Insert with write lock (double-check inside insert_tgz)
    if let Ok(mut cache) = TAR_CACHE.write() {
        cache.insert_tgz(tgz_path.to_path_buf(), files, total_size);
    }

    Ok(())
}

/// Extract file with caching
pub async fn extract_file_cached(tgz_path: &Path, file_path: &str) -> Result<Arc<Vec<u8>>> {
    let normalized_path = strip_first_component(file_path).to_string();

    // Try cache first
    if let Ok(mut cache) = TAR_CACHE.write()
        && let Some(content) = cache.get_file(tgz_path, &normalized_path)
    {
        return Ok(content);
    }

    // Load if needed
    ensure_tgz_cached(tgz_path).await?;

    // Get from cache
    let Ok(mut cache) = TAR_CACHE.write() else {
        return Err(Error::new(ErrorKind::Other, "Failed to acquire cache lock"));
    };

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

    Err(Error::new(ErrorKind::NotFound, format!("File {} not found in {}", file_path, tgz_path.display())))
}

/// List directory with caching
pub async fn list_dir_cached(tgz_path: &Path, dir_path: &str) -> Result<Vec<DirEntry>> {
    let normalized_dir = strip_first_component(dir_path).to_string();

    ensure_tgz_cached(tgz_path).await?;

    let Ok(mut cache) = TAR_CACHE.write() else {
        return Ok(vec![]);
    };

    let Some(items) = cache.list_dir(tgz_path, &normalized_dir) else {
        return Ok(vec![]);
    };

    Ok(items
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
        .collect())
}

/// Clear cache
pub fn clear_cache() {
    if let Ok(mut cache) = TAR_CACHE.write() {
        cache.cache.clear();
        cache.current_size = 0;
    }
}

/// Set maximum cache size
pub fn set_max_size(max_size_bytes: usize) {
    if let Ok(mut cache) = TAR_CACHE.write() {
        cache.max_size = max_size_bytes;
        while cache.current_size > cache.max_size && !cache.cache.is_empty() {
            cache.evict_oldest();
        }
    }
}
