use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::{RwLock, LazyLock};
use std::borrow::Cow;

use crate::util::read_dir_direct;
use crate::tar_cache;
use tracing::error;

// Global cache for fuse.link mappings to avoid repeated file reads
static FUSE_LINK_CACHE: LazyLock<RwLock<HashMap<PathBuf, String>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

/// Create fuse link with tgz path and prefix (for lazy extraction)
/// Format: "{tgz_path}|{prefix}"
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

    let link_content = if let Some(p) = prefix {
        format!("{}|{}\n", tgz_ref.to_string_lossy(), p)
    } else {
        format!("{}\n", tgz_ref.to_string_lossy())
    };

    tokio_fs_ext::write(&fuse_link_path, link_content.as_bytes()).await?;

    if let Ok(mut cache) = FUSE_LINK_CACHE.write() {
        cache.insert(fuse_link_path.clone(), link_content.trim().to_string());
    }

    Ok(())
}

/// Parse fuse.link content, returns (path, optional_prefix)
fn parse_fuse_link_content(content: &str) -> (String, Option<String>) {
    let trimmed = content.trim();
    if let Some(idx) = trimmed.find('|') {
        let path = trimmed[..idx].to_string();
        let prefix = trimmed[idx + 1..].to_string();
        (path, Some(prefix))
    } else {
        (trimmed.to_string(), None)
    }
}

/// Get fuse.link path for a given path that contains node_modules
fn get_fuse_link_path<P: AsRef<Path>>(path: P) -> Option<PathBuf> {
    let mut current = path.as_ref();
    let mut temp: (Cow<str>, Cow<str>) = (Cow::Borrowed(""), Cow::Borrowed(""));

    while let Some(parent) = current.parent() {
        if let Some(file_name) = current.file_name() {
            let name = file_name.to_string_lossy();
            temp = (name, temp.0);

            if let Some(parent_name) = parent.file_name() {
                if parent_name == "node_modules" {
                    if !temp.0.is_empty() {
                        if temp.1.is_empty() {
                            if !temp.0.starts_with('@') {
                                return Some(parent.join(temp.0.as_ref()).join("fuse.link"));
                            }
                        } else {
                            if temp.0.starts_with('@') {
                                return Some(parent.join(temp.0.as_ref()).join(temp.1.as_ref()).join("fuse.link"));
                            } else {
                                return Some(parent.join(temp.0.as_ref()).join("fuse.link"));
                            }
                        }
                    }
                }
            }
        }
        current = parent;
    }
    None
}

/// Fuse link target info
struct FuseLinkTarget {
    target_path: PathBuf,
    relative_path: PathBuf,
    tgz_prefix: Option<String>,
}

/// Get the target path for a node_modules path through fuse.link
async fn get_fuse_link_target_path<P: AsRef<Path> + std::fmt::Debug>(prepared_path: P) -> Result<Option<FuseLinkTarget>> {
    let path_ref = prepared_path.as_ref();

    let fuse_link_path = match get_fuse_link_path(path_ref) {
        Some(path) => path,
        None => return Ok(None),
    };

    let raw_content = if let Ok(cache) = FUSE_LINK_CACHE.read() {
        if let Some(cached_target) = cache.get(&fuse_link_path) {
            cached_target.clone()
        } else {
            drop(cache);

            let link_content = match tokio_fs_ext::read_to_string(&fuse_link_path).await {
                Ok(content) => content,
                Err(_) => return Ok(None),
            };

            let raw = link_content.lines().next().unwrap_or("").trim().to_string();
            if raw.is_empty() {
                return Ok(None);
            }

            if let Ok(mut cache) = FUSE_LINK_CACHE.write() {
                cache.insert(fuse_link_path.clone(), raw.clone());
            }

            raw
        }
    } else {
        let link_content = match tokio_fs_ext::read_to_string(&fuse_link_path).await {
            Ok(content) => content,
            Err(_) => return Ok(None),
        };

        let raw = link_content.lines().next().unwrap_or("").trim();
        if raw.is_empty() {
            return Ok(None);
        }
        raw.to_string()
    };

    let (target_dir, tgz_prefix) = parse_fuse_link_content(&raw_content);

    let fuse_link_dir = fuse_link_path.parent().ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, "Invalid fuse.link path")
    })?;

    let relative_path = match path_ref.strip_prefix(fuse_link_dir) {
        Ok(rel) => rel,
        Err(_) => {
            return Err(Error::new(ErrorKind::InvalidInput, "Path is not under fuse.link directory"));
        }
    };

    let target_path = if relative_path.as_os_str().is_empty() {
        PathBuf::from(&target_dir)
    } else if tgz_prefix.is_some() {
        PathBuf::from(&target_dir)
    } else {
        PathBuf::from(&target_dir).join(relative_path)
    };

    Ok(Some(FuseLinkTarget {
        target_path,
        relative_path: relative_path.to_path_buf(),
        tgz_prefix,
    }))
}

/// Try to read file through fuse link logic for node_modules
pub(super) async fn try_read_through_fuse_link<P: AsRef<Path> + std::fmt::Debug>(
    prepared_path: P,
) -> Result<Option<Vec<u8>>> {
    let target = match get_fuse_link_target_path(&prepared_path).await? {
        Some(t) => t,
        None => return Ok(None),
    };

    if let Some(prefix) = &target.tgz_prefix {
        let tgz_path = &target.target_path;
        let file_in_tgz = if target.relative_path.as_os_str().is_empty() {
            return Err(Error::new(ErrorKind::IsADirectory, "Cannot read directory as file"));
        } else {
            format!("{}/{}", prefix, target.relative_path.to_string_lossy())
        };

        match tar_cache::extract_file_cached(tgz_path, &file_in_tgz).await {
            Ok(content) => Ok(Some(content)),
            Err(e) => {
                error!("Extract failed: {:?}", e);
                Ok(None)
            }
        }
    } else {
        match tokio_fs_ext::read(&target.target_path).await {
            Ok(content) => Ok(Some(content)),
            Err(_) => Ok(None),
        }
    }
}

/// Try to read directory through fuse.link for node_modules
pub(super) async fn try_read_dir_through_fuse_link<P: AsRef<Path> + std::fmt::Debug>(
    prepared_path: P,
) -> Result<Option<Vec<tokio_fs_ext::DirEntry>>> {
    let target = match get_fuse_link_target_path(&prepared_path).await? {
        Some(t) => t,
        None => return Ok(None),
    };

    let target_entries = if let Some(prefix) = &target.tgz_prefix {
        let tgz_path = &target.target_path;
        let dir_in_tgz = if target.relative_path.as_os_str().is_empty() {
            String::new()
        } else {
            format!("{}/{}", prefix, target.relative_path.to_string_lossy())
        };

        match tar_cache::list_dir_cached(tgz_path, &dir_in_tgz).await {
            Ok(entries) => entries,
            Err(_) => return Ok(None),
        }
    } else {
        match read_dir_direct(&target.target_path).await {
            Ok(entries) => entries,
            Err(_) => return Ok(None),
        }
    };

    let path_ref = prepared_path.as_ref();
    let original_entries = match read_dir_direct(path_ref).await {
        Ok(entries) => entries,
        Err(_) => return Ok(Some(target_entries)),
    };

    let filtered_original: Vec<_> = original_entries
        .into_iter()
        .filter(|entry| entry.file_name().to_string_lossy() != "fuse.link")
        .collect();

    let mut combined_entries = filtered_original;
    combined_entries.extend(target_entries);

    Ok(Some(combined_entries))
}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use crate::test_utils;
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_basic() {
        let path = Path::new("./node_modules/c/index.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/c/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_scoped() {
        let path = Path::new("./node_modules/@a/b/package.json");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/@a/b/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_nested() {
        let path = Path::new("./node_modules/c/node_modules/d/types.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/c/node_modules/d/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_scoped_nested() {
        let path = Path::new("./node_modules/@a/b/node_modules/@c/d/index.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/@a/b/node_modules/@c/d/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_no_node_modules() {
        let path = Path::new("./some/other/path/file.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, None);
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_direct() {
        let path = Path::new("./node_modules/a");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/a/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_scoped_direct() {
        let path = Path::new("./node_modules/@a/b");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/@a/b/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_scope_directory_only() {
        test_utils::init_tracing();
        let path = Path::new("./node_modules/@umi");
        let result = get_fuse_link_path(path);
        assert_eq!(result, None);
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_empty() {
        let path = Path::new("");
        let result = get_fuse_link_path(path);
        assert_eq!(result, None);
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_just_node_modules() {
        let path = Path::new("./node_modules");
        let result = get_fuse_link_path(path);
        assert_eq!(result, None);
    }

    #[wasm_bindgen_test]
    fn test_get_fuse_link_path_deep_nested() {
        let path = Path::new("./node_modules/a/node_modules/b/node_modules/c/node_modules/d/file.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/a/node_modules/b/node_modules/c/node_modules/d/fuse.link").to_path_buf()));
    }
}
