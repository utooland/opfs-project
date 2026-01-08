use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::{RwLock, LazyLock};

use crate::util::read_dir_direct;
use crate::tar_cache;
use tracing::error;

static FUSE_LINK_CACHE: LazyLock<RwLock<HashMap<PathBuf, String>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Create fuse link with tgz path and prefix
pub async fn fuse_link_with_prefix<S: AsRef<Path>, D: AsRef<Path>>(
    tgz_path: S,
    dst: D,
    prefix: Option<&str>,
) -> Result<()> {
    let tgz_ref = tgz_path.as_ref();
    let dst_ref = dst.as_ref();

    tokio_fs_ext::create_dir_all(dst_ref).await?;

    let fuse_link_path = get_fuse_link_path(dst_ref)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Could not determine fuse.link path"))?;

    let link_content = match prefix {
        Some(p) => format!("{}|{}\n", tgz_ref.display(), p),
        None => format!("{}\n", tgz_ref.display()),
    };

    tokio_fs_ext::write(&fuse_link_path, link_content.as_bytes()).await?;

    if let Ok(mut cache) = FUSE_LINK_CACHE.write() {
        cache.insert(fuse_link_path, link_content.trim().to_string());
    }

    Ok(())
}

/// Get fuse.link path for a node_modules path
fn get_fuse_link_path<P: AsRef<Path>>(path: P) -> Option<PathBuf> {
    let mut current = path.as_ref();
    let mut components: (String, String) = (String::new(), String::new());

    while let Some(parent) = current.parent() {
        if let Some(name) = current.file_name() {
            let name_str = name.to_string_lossy().to_string();
            components = (name_str, components.0);

            if parent.file_name().map(|n| n == "node_modules").unwrap_or(false) {
                if components.0.is_empty() {
                    // continue
                } else if components.0.starts_with('@') {
                    if !components.1.is_empty() {
                        return Some(parent.join(&components.0).join(&components.1).join("fuse.link"));
                    }
                } else {
                    return Some(parent.join(&components.0).join("fuse.link"));
                }
            }
        }
        current = parent;
    }
    None
}

struct FuseLinkTarget {
    tgz_path: PathBuf,
    relative_path: PathBuf,
    prefix: Option<String>,
}

impl FuseLinkTarget {
    fn file_path_in_tgz(&self) -> Option<String> {
        let prefix = self.prefix.as_ref()?;
        if self.relative_path.as_os_str().is_empty() {
            None
        } else {
            Some(format!("{}/{}", prefix, self.relative_path.display()))
        }
    }

    fn dir_path_in_tgz(&self) -> Option<String> {
        let prefix = self.prefix.as_ref()?;
        if self.relative_path.as_os_str().is_empty() {
            Some(String::new())
        } else {
            Some(format!("{}/{}", prefix, self.relative_path.display()))
        }
    }

    fn is_tgz_mode(&self) -> bool {
        self.prefix.is_some()
    }
}

async fn resolve_fuse_link<P: AsRef<Path>>(path: P) -> Result<Option<FuseLinkTarget>> {
    let path_ref = path.as_ref();

    let fuse_link_path = match get_fuse_link_path(path_ref) {
        Some(p) => p,
        None => return Ok(None),
    };

    // Read from cache or file
    let content = read_fuse_link_content(&fuse_link_path).await?;
    let content = match content {
        Some(c) => c,
        None => return Ok(None),
    };

    // Parse content: "path" or "path|prefix"
    let (tgz_path, prefix) = match content.find('|') {
        Some(idx) => (content[..idx].to_string(), Some(content[idx + 1..].to_string())),
        None => (content, None),
    };

    let fuse_link_dir = fuse_link_path.parent()
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Invalid fuse.link path"))?;

    let relative_path = path_ref.strip_prefix(fuse_link_dir)
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "Path not under fuse.link directory"))?
        .to_path_buf();

    Ok(Some(FuseLinkTarget {
        tgz_path: PathBuf::from(tgz_path),
        relative_path,
        prefix,
    }))
}

async fn read_fuse_link_content(fuse_link_path: &Path) -> Result<Option<String>> {
    // Try cache first
    if let Ok(cache) = FUSE_LINK_CACHE.read() {
        if let Some(content) = cache.get(fuse_link_path) {
            return Ok(Some(content.clone()));
        }
    }

    // Read from file
    let content = match tokio_fs_ext::read_to_string(fuse_link_path).await {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let trimmed = content.lines().next().unwrap_or("").trim().to_string();
    if trimmed.is_empty() {
        return Ok(None);
    }

    // Update cache
    if let Ok(mut cache) = FUSE_LINK_CACHE.write() {
        cache.insert(fuse_link_path.to_path_buf(), trimmed.clone());
    }

    Ok(Some(trimmed))
}

/// Try to read file through fuse.link
pub(super) async fn try_read_through_fuse_link<P: AsRef<Path>>(path: P) -> Result<Option<Vec<u8>>> {
    let target = match resolve_fuse_link(&path).await? {
        Some(t) => t,
        None => return Ok(None),
    };

    if !target.is_tgz_mode() {
        return tokio_fs_ext::read(&target.tgz_path).await.map(Some).or(Ok(None));
    }

    let file_in_tgz = match target.file_path_in_tgz() {
        Some(p) => p,
        None => return Err(Error::new(ErrorKind::IsADirectory, "Cannot read directory as file")),
    };

    match tar_cache::extract_file_cached(&target.tgz_path, &file_in_tgz).await {
        Ok(content) => Ok(Some(content)),
        Err(e) => {
            error!("Extract failed: {:?}", e);
            Ok(None)
        }
    }
}

/// Try to read directory through fuse.link
pub(super) async fn try_read_dir_through_fuse_link<P: AsRef<Path>>(
    path: P,
) -> Result<Option<Vec<tokio_fs_ext::DirEntry>>> {
    let target = match resolve_fuse_link(&path).await? {
        Some(t) => t,
        None => return Ok(None),
    };

    // Get entries from target (tgz or directory)
    let target_entries = if let Some(dir_in_tgz) = target.dir_path_in_tgz() {
        tar_cache::list_dir_cached(&target.tgz_path, &dir_in_tgz).await.ok()
    } else {
        read_dir_direct(&target.tgz_path).await.ok()
    };

    let target_entries = match target_entries {
        Some(e) => e,
        None => return Ok(None),
    };

    // Get original directory entries (excluding fuse.link)
    let original_entries = read_dir_direct(path.as_ref()).await.ok();

    let Some(original) = original_entries else {
        return Ok(Some(target_entries));
    };

    // Combine: original (filtered) + target
    let mut combined: Vec<_> = original
        .into_iter()
        .filter(|e| e.file_name().to_string_lossy() != "fuse.link")
        .collect();
    combined.extend(target_entries);

    Ok(Some(combined))
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_basic() {
        assert_eq!(
            get_fuse_link_path("./node_modules/c/index.js"),
            Some(PathBuf::from("./node_modules/c/fuse.link"))
        );
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_scoped() {
        assert_eq!(
            get_fuse_link_path("./node_modules/@a/b/package.json"),
            Some(PathBuf::from("./node_modules/@a/b/fuse.link"))
        );
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_nested() {
        assert_eq!(
            get_fuse_link_path("./node_modules/c/node_modules/d/types.js"),
            Some(PathBuf::from("./node_modules/c/node_modules/d/fuse.link"))
        );
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_scoped_nested() {
        assert_eq!(
            get_fuse_link_path("./node_modules/@a/b/node_modules/@c/d/index.js"),
            Some(PathBuf::from("./node_modules/@a/b/node_modules/@c/d/fuse.link"))
        );
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_no_node_modules() {
        assert_eq!(get_fuse_link_path("./some/other/path/file.js"), None);
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_direct() {
        assert_eq!(
            get_fuse_link_path("./node_modules/a"),
            Some(PathBuf::from("./node_modules/a/fuse.link"))
        );
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_scope_only() {
        assert_eq!(get_fuse_link_path("./node_modules/@umi"), None);
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_empty() {
        assert_eq!(get_fuse_link_path(""), None);
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_just_node_modules() {
        assert_eq!(get_fuse_link_path("./node_modules"), None);
    }
}
