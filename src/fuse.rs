
use std::io::{Error, ErrorKind, Result};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::sync::{Mutex, LazyLock};

use crate::util::read_dir_direct;
use tracing::{info, instrument};

// Global cache for fuse.link mappings to avoid repeated file reads
// Key: fuse.link file path, Value: target directory path
static FUSE_LINK_CACHE: LazyLock<Mutex<HashMap<PathBuf, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

// Cache for resolved paths to avoid repeated path calculations
// Key: (fuse_link_dir, prepared_path), Value: (target_path, relative_path)
static PATH_RESOLVE_CACHE: LazyLock<Mutex<HashMap<(PathBuf, PathBuf), (PathBuf, PathBuf)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

// Create fuse link between source and destination directories
// node_modules/@a/b/fuse.link -> /stores/@a/b/unpack
// node_modules/@a/b/node_modules/c/fuse.link -> /stores/c/unpack
pub async fn fuse_link<S: AsRef<Path>, D: AsRef<Path>>(src: S, dst: D) -> Result<()> {
    let src_ref = src.as_ref();
    let dst_ref = dst.as_ref();

    info!("Creating fuse link from {} to {}", src_ref.display(), dst_ref.display());

    // Create the destination directory if it doesn't exist
    tokio_fs_ext::create_dir_all(dst_ref.to_string_lossy().as_ref()).await?;

    // Get the fuse.link path for the destination directory
    let fuse_link_path = get_fuse_link_path(dst_ref)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "Could not determine fuse.link path"))?;

    // Write the source path to the fuse.link file
    let link_content = format!("{}\n", src_ref.to_string_lossy());
    info!("Writing fuse.link file at {}", fuse_link_path.display());
    tokio_fs_ext::write(&fuse_link_path, link_content.as_bytes()).await?;

    // Cache the mapping for future reads
    if let Ok(mut cache) = FUSE_LINK_CACHE.lock() {
        cache.insert(fuse_link_path.clone(), src_ref.to_string_lossy().to_string());
        info!("Cached fuse link mapping: {} -> {}", fuse_link_path.display(), src_ref.display());
    }

    info!("Successfully created fuse link");
    Ok(())
}

/// Clear the fuse.link cache (useful for testing or memory management)
pub fn clear_fuse_link_cache() {
    if let Ok(mut cache) = FUSE_LINK_CACHE.lock() {
        let count = cache.len();
        cache.clear();
        info!("Cleared {} cached fuse.link mappings", count);
    }
    if let Ok(mut cache) = PATH_RESOLVE_CACHE.lock() {
        let count = cache.len();
        cache.clear();
        info!("Cleared {} cached path resolutions", count);
    }
}

/// Get cache statistics (for debugging/monitoring)
pub fn get_cache_stats() -> (usize, Vec<String>) {
    if let Ok(cache) = FUSE_LINK_CACHE.lock() {
        let count = cache.len();
        let paths: Vec<String> = cache.keys().map(|p| p.to_string_lossy().to_string()).collect();
        (count, paths)
    } else {
        (0, vec![])
    }
}

// Get fuse.link path for a given path that contains node_modules
// ./node_modules/@a/b/package.json -> ./node_modules/@a/b/fuse.link
// ./node_modules/c/index.js -> ./node_modules/c/fuse.link
// ./node_modules/c/node_modules/d/types.js -> ./node_modules/c/node_modules/d/fuse.link
fn get_fuse_link_path<P: AsRef<Path>>(path: P) -> Option<std::path::PathBuf> {
    let mut current = path.as_ref();
    let mut temp = ("", "");

    let start_time = web_time::Instant::now();
    // find in cache first
    if let Ok(cache) = FUSE_LINK_CACHE.lock() {
        if let Some(fuse_link_path) = cache.keys().find(|key| current.starts_with(key.parent().unwrap())) {
            info!("Found fuse.link path in cache: {}, took {:?}", fuse_link_path.display(), start_time.elapsed());
            return Some(fuse_link_path.clone());
        }
    }

    // Walk up the path tree to find node_modules
    while let Some(parent) = current.parent() {
        if let Some(file_name) = current.file_name() {
            let name = file_name.to_str()?;

            // Update temp tuple: shift components and add new one
            temp = (name, temp.0);

            // Check if parent is node_modules
            if let Some(parent_name) = parent.file_name() {
                if parent_name == "node_modules" {
                    // Found node_modules, construct the fuse.link path
                    if !temp.0.is_empty() {
                        if temp.1.is_empty() {
                            // Single component package
                            let fuse_path = parent.join(temp.0).join("fuse.link");
                            info!("Found fuse.link path for single component: {}", fuse_path.display());
                            return Some(fuse_path);
                        } else {
                            // Two component package (could be scope/package or package/subpath)
                            if temp.0.starts_with('@') {
                                let fuse_path = parent.join(temp.0).join(temp.1).join("fuse.link");
                                info!("Found fuse.link path for scoped package: {}", fuse_path.display());
                                return Some(fuse_path);
                            } else {
                                let fuse_path = parent.join(temp.0).join("fuse.link");
                                info!("Found fuse.link path for package with subpath: {}", fuse_path.display());
                                return Some(fuse_path);
                            }
                        }
                    }
                }
            }
        }

        current = parent;
    }

    info!("No fuse.link path found for given path");
    None
}

/// Get the target path for a node_modules path through fuse.link
async fn get_fuse_link_target_path<P: AsRef<Path> + std::fmt::Debug>(prepared_path: P) -> Result<Option<(std::path::PathBuf, std::path::PathBuf)>> {
    #[cfg(target_arch = "wasm32")]
    let start_time = web_time::Instant::now();
    #[cfg(not(target_arch = "wasm32"))]
    let start_time = std::time::Instant::now();

    let path_ref = prepared_path.as_ref();

    // Step 1: Find fuse.link path - streamlined  
    let fuse_link_path = match get_fuse_link_path(path_ref) {
        Some(path) => path,
        None => return Ok(None),
    };

    // Step 2: Check cache first, then read fuse.link file  
    let target_dir = if let Ok(cache) = FUSE_LINK_CACHE.lock() {
        if let Some(cached_target) = cache.get(&fuse_link_path) {
            cached_target.clone()
        } else {
            drop(cache); // Release lock before async operation

            let read_link_start = start_time.elapsed();
            let link_content = match tokio_fs_ext::read_to_string(&fuse_link_path).await {
                Ok(content) => {
                    let read_link_duration = start_time.elapsed() - read_link_start;
                    info!("Cache miss - read fuse.link content ({:.2}ms)", read_link_duration.as_secs_f64() * 1000.0);
                    content
                },
                Err(e) => {
                    let read_link_duration = start_time.elapsed() - read_link_start;
                    info!("Failed to read fuse.link file: {:?} ({:.2}ms)", e, read_link_duration.as_secs_f64() * 1000.0);
                    return Ok(None);
                },
            };

            let target_dir = link_content.lines().next().unwrap_or("").trim().to_string();
            if target_dir.is_empty() {
                info!("Empty target directory in fuse.link");
                return Ok(None);
            }

            // Update cache
            if let Ok(mut cache) = FUSE_LINK_CACHE.lock() {
                cache.insert(fuse_link_path.clone(), target_dir.clone());
                info!("Cached new fuse link mapping after read");
            }

            target_dir
        }
    } else {
        // Fallback to direct file read if cache lock fails
        let read_link_start = start_time.elapsed();
        let link_content = match tokio_fs_ext::read_to_string(&fuse_link_path).await {
            Ok(content) => {
                let read_link_duration = start_time.elapsed() - read_link_start;
                info!("Cache unavailable - read fuse.link content ({:.2}ms)", read_link_duration.as_secs_f64() * 1000.0);
                content
            },
            Err(e) => {
                let read_link_duration = start_time.elapsed() - read_link_start;
                info!("Failed to read fuse.link file: {:?} ({:.2}ms)", e, read_link_duration.as_secs_f64() * 1000.0);
                return Ok(None);
            },
        };

        let target_dir = link_content.lines().next().unwrap_or("").trim();
        if target_dir.is_empty() {
            info!("Empty target directory in fuse.link");
            return Ok(None);
        }
        target_dir.to_string()
    };

    let fuse_link_dir = fuse_link_path.parent().ok_or_else(|| {
        Error::new(ErrorKind::InvalidInput, "Invalid fuse.link path")
    })?;

    // Fast path: direct string manipulation to avoid PathBuf allocations
    let fuse_link_dir_str = fuse_link_dir.to_string_lossy();
    let path_str = path_ref.to_string_lossy();
    
    // Quick prefix check and slice
    if !path_str.starts_with(&*fuse_link_dir_str) {
        return Err(Error::new(ErrorKind::InvalidInput, "Path is not under fuse.link directory"));
    }
    
    let mut rel_start = fuse_link_dir_str.len();
    if path_str.len() > rel_start && (path_str.chars().nth(rel_start) == Some('/') || path_str.chars().nth(rel_start) == Some('\\')) {
        rel_start += 1;
    }
    let relative_str = &path_str[rel_start..];
    
    // Direct concatenation
    let target_path_str = if relative_str.is_empty() {
        target_dir.clone()
    } else {
        format!("{}/{}", target_dir, relative_str)
    };
    
    let target_path = PathBuf::from(target_path_str);
    let relative_path = PathBuf::from(relative_str);
    Ok(Some((target_path, relative_path.to_path_buf())))
}

/// Try to read file through fuse link logic for node_modules
pub(super) async fn try_read_through_fuse_link<P: AsRef<Path> + std::fmt::Debug>(
    prepared_path: P,
) -> Result<Option<Vec<u8>>> {
    #[cfg(target_arch = "wasm32")]
    let start_time = web_time::Instant::now();
    #[cfg(not(target_arch = "wasm32"))]
    let start_time = std::time::Instant::now();

    let resolve_start = start_time.elapsed();
    let (target_dir, _) = match get_fuse_link_target_path(&prepared_path).await? {
        Some((path, relative)) => {
            let resolve_duration = start_time.elapsed() - resolve_start;
            info!("Found target path for reading: {} (fuse_resolve: {:.2}ms)",
                  path.display(), resolve_duration.as_secs_f64() * 1000.0);
            (path, relative)
        },
        None => {
            let resolve_duration = start_time.elapsed() - resolve_start;
            info!("No fuse link target found for reading (fuse_resolve: {:.2}ms)",
                  resolve_duration.as_secs_f64() * 1000.0);
            return Ok(None);
        },
    };

    #[cfg(target_arch = "wasm32")]
    let read_start = web_time::Instant::now();
    #[cfg(not(target_arch = "wasm32"))]
    let read_start = std::time::Instant::now();

    match tokio_fs_ext::read(&target_dir).await {
        Ok(content) => {
            let read_duration = read_start.elapsed();
            let total_duration = start_time.elapsed();
            info!("Successfully read {} bytes through fuse link (read: {:.2}ms, total: {:.2}ms)",
                  content.len(), read_duration.as_secs_f64() * 1000.0, total_duration.as_secs_f64() * 1000.0);
            Ok(Some(content))
        },
        Err(e) => {
            let total_duration = start_time.elapsed();
            info!("Failed to read target file: {:?} (total: {:.2}ms)", e, total_duration.as_secs_f64() * 1000.0);
            Ok(None)
        },
    }
}


/// Try to read directory through fuse.link for node_modules
pub(super) async fn try_read_dir_through_fuse_link<P: AsRef<Path> + std::fmt::Debug>(
    prepared_path: P,
) -> Result<Option<Vec<tokio_fs_ext::DirEntry>>> {
    #[cfg(target_arch = "wasm32")]
    let start_time = web_time::Instant::now();
    #[cfg(not(target_arch = "wasm32"))]
    let start_time = std::time::Instant::now();

    let resolve_start = start_time.elapsed();
    let (target_path, _relative_path) = match get_fuse_link_target_path(&prepared_path).await? {
        Some((path, relative)) => {
            let resolve_duration = start_time.elapsed() - resolve_start;
            info!("Found target path for directory reading: {} (resolve: {:.2}ms)",
                  path.display(), resolve_duration.as_secs_f64() * 1000.0);
            (path, relative)
        },
        None => {
            let resolve_duration = start_time.elapsed() - resolve_start;
            info!("No fuse link target found for directory reading (resolve: {:.2}ms)",
                  resolve_duration.as_secs_f64() * 1000.0);
            return Ok(None);
        },
    };

    let read_target_start = start_time.elapsed();
    let target_entries = read_dir_direct(&target_path).await?;
    let read_target_duration = start_time.elapsed() - read_target_start;
    info!("Read {} entries from target directory ({:.2}ms)",
          target_entries.len(), read_target_duration.as_secs_f64() * 1000.0);

    // Always read original directory and combine with target entries
    // This ensures we always see both the original content (like node_modules) and target content
    let path_ref = prepared_path.as_ref();
    let read_orig_start = start_time.elapsed();
    let original_entries = match read_dir_direct(path_ref).await {
        Ok(entries) => {
            let read_orig_duration = start_time.elapsed() - read_orig_start;
            info!("Read {} entries from original directory ({:.2}ms)",
                  entries.len(), read_orig_duration.as_secs_f64() * 1000.0);
            entries
        },
        Err(e) => {
            let read_orig_duration = start_time.elapsed() - read_orig_start;
            let total_duration = start_time.elapsed();
            info!("Failed to read original directory, returning {} target entries only: {:?} (read_orig: {:.2}ms, total: {:.2}ms)",
                  target_entries.len(), e, read_orig_duration.as_secs_f64() * 1000.0, total_duration.as_secs_f64() * 1000.0);
            return Ok(Some(target_entries));
        },
    };

    // Filter out fuse.link files from original entries
    let filtered_original: Vec<_> = original_entries
        .into_iter()
        .filter(|entry| {
            if let Some(file_name) = entry.file_name().to_str() {
                file_name != "fuse.link"
            } else {
                true
            }
        })
        .collect();

    // Combine original entries (including node_modules) + target entries (package content)
    let mut combined_entries = filtered_original;
    combined_entries.extend(target_entries);

    let total_duration = start_time.elapsed();
    info!("Combined {} total directory entries (total: {:.2}ms)",
          combined_entries.len(), total_duration.as_secs_f64() * 1000.0);
    Ok(Some(combined_entries))

}

#[cfg(test)]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_dedicated_worker);
    use super::*;

    use wasm_bindgen_test::*;

    /// Test helper: create a temporary directory with test files
    async fn create_test_dir(name: &str) -> String {
        let temp_path = format!("/test-fuse-dir-{}", name);
        tokio_fs_ext::create_dir_all(&temp_path).await.unwrap();

        // Create test files
        tokio_fs_ext::write(&format!("{}/test.txt", temp_path), b"Hello, World!")
            .await
            .unwrap();

        tokio_fs_ext::write(
            &format!("{}/package.json", temp_path),
            b"{\"name\": \"test-package\"}",
        )
        .await
        .unwrap();

        temp_path
    }

    #[wasm_bindgen_test]
    async fn test_simplified_fuse_link_basic() {
        let base_path = create_test_dir("test-simplified-fuse-link-basic").await;
        let src_path = format!("{}/stores/a/unpack", base_path);
        let dst_path = format!("{}/node_modules/a", base_path);

        // Create source directory
        tokio_fs_ext::create_dir_all(&src_path).await.unwrap();
        tokio_fs_ext::write(format!("{}/package.json", src_path), b"{}").await.unwrap();

        // Create fuse link
        fuse_link(&src_path, &dst_path).await.unwrap();

        // Verify fuse.link file was created
        let fuse_link_path = format!("{}/fuse.link", dst_path);
        let link_content = tokio_fs_ext::read_to_string(&fuse_link_path).await.unwrap();
        assert_eq!(link_content.trim(), src_path);

        // Verify we can read through the fuse link
        let test_file_path = format!("{}/package.json", dst_path);
        let content = crate::read(&test_file_path).await.unwrap();
        assert_eq!(content, b"{}");
    }

    #[wasm_bindgen_test]
    async fn test_simplified_fuse_link_scoped_package() {
        let base_path = create_test_dir("test-simplified-fuse-link-scoped").await;
        let src_path = format!("{}/stores/@a/b/unpack", base_path);
        let dst_path = format!("{}/node_modules/@a/b", base_path);

        // Create source directory
        tokio_fs_ext::create_dir_all(&src_path).await.unwrap();
        tokio_fs_ext::write(format!("{}/package.json", src_path), b"{}").await.unwrap();

        // Create fuse link
        fuse_link(&src_path, &dst_path).await.unwrap();

        // Verify fuse.link file was created
        let fuse_link_path = format!("{}/fuse.link", dst_path);
        let link_content = tokio_fs_ext::read_to_string(&fuse_link_path).await.unwrap();
        assert_eq!(link_content.trim(), src_path);

        // Verify we can read through the fuse link
        let test_file_path = format!("{}/package.json", dst_path);
        let content = crate::read(&test_file_path).await.unwrap();
        assert_eq!(content, b"{}");
    }

    #[wasm_bindgen_test]
    async fn test_fuse_link_combine_logic() {
        let base_path = create_test_dir("test-fuse-link-combine").await;

        // Create the structure:
        // /project/node_modules/a/fuse.link -> /project/stores/a/unpack
        // /project/node_modules/a/node_modules/b/fuse.link -> /project/stores/b/unpack

        // Create source directories
        let src_a = format!("{}/stores/a/unpack", base_path);
        let src_b = format!("{}/stores/b/unpack", base_path);

        tokio_fs_ext::create_dir_all(&src_a).await.unwrap();
        tokio_fs_ext::create_dir_all(&src_b).await.unwrap();

        // Create package files
        tokio_fs_ext::write(format!("{}/package.json", src_a), b"{\"name\": \"package-a\"}").await.unwrap();
        tokio_fs_ext::write(format!("{}/index.js", src_a), b"console.log('package-a');").await.unwrap();

        tokio_fs_ext::write(format!("{}/package.json", src_b), b"{\"name\": \"package-b\"}").await.unwrap();
        tokio_fs_ext::write(format!("{}/index.js", src_b), b"console.log('package-b');").await.unwrap();

        // Create destination directories
        let dst_a = format!("{}/node_modules/a", base_path);
        let dst_b = format!("{}/node_modules/a/node_modules/b", base_path);

        // Create the full directory structure
        tokio_fs_ext::create_dir_all(&dst_a).await.unwrap();
        tokio_fs_ext::create_dir_all(&dst_b).await.unwrap();

        // Create fuse links
        fuse_link(&src_a, &dst_a).await.unwrap();
        fuse_link(&src_b, &dst_b).await.unwrap();

        // Debug: Check directory structure after creating fuse links
        println!("After creating fuse links:");
        println!("dst_a exists: {}", tokio_fs_ext::try_exists(&dst_a).await.unwrap_or(false));
        println!("dst_b exists: {}", tokio_fs_ext::try_exists(&dst_b).await.unwrap_or(false));

        // Check if node_modules subdirectory exists in dst_a
        let node_modules_in_a = format!("{}/node_modules", dst_a);
        println!("node_modules in dst_a exists: {}", tokio_fs_ext::try_exists(&node_modules_in_a).await.unwrap_or(false));

        // Verify fuse.link files were created
        let fuse_link_a = format!("{}/fuse.link", dst_a);
        let fuse_link_b = format!("{}/fuse.link", dst_b);

        let link_content_a = tokio_fs_ext::read_to_string(&fuse_link_a).await.unwrap();
        let link_content_b = tokio_fs_ext::read_to_string(&fuse_link_b).await.unwrap();

        assert_eq!(link_content_a.trim(), src_a);
        assert_eq!(link_content_b.trim(), src_b);

        // Test: Read directory /project/node_modules/a
        // This should combine:
        // 1. Original directory content (node_modules/b, fuse.link)
        // 2. Target directory content (package.json, index.js)
        let entries = crate::read_dir(&dst_a).await.unwrap();
        let entry_names: Vec<String> = entries
            .into_iter()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();

        println!("Directory entries for {}: {:?}", dst_a, entry_names);

        // Debug: Check what's actually in the original directory
        let original_entries = crate::util::read_dir_direct(&dst_a).await.unwrap();
        let original_names: Vec<String> = original_entries
            .into_iter()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        println!("Original directory entries: {:?}", original_names);

        // Debug: Check what's in the target directory
        let target_entries = crate::util::read_dir_direct(&src_a).await.unwrap();
        let target_names: Vec<String> = target_entries
            .into_iter()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        println!("Target directory entries: {:?}", target_names);

                // Should contain: package.json, index.js (fuse.link should be filtered out)
        // Note: node_modules might not be visible if it's empty or if fuse.link replaces the directory
        println!("Expected: package.json, index.js (and possibly node_modules)");
        println!("Actual: {:?}", entry_names);

        // At minimum, we should see the package content
        assert!(entry_names.contains(&"package.json".to_string()));
        assert!(entry_names.contains(&"index.js".to_string()));
        assert!(!entry_names.contains(&"fuse.link".to_string()));

        // Verify we can read files through the fuse link
        let package_json_content = crate::read(&format!("{}/package.json", dst_a)).await.unwrap();
        assert_eq!(package_json_content, b"{\"name\": \"package-a\"}");

        let index_js_content = crate::read(&format!("{}/index.js", dst_a)).await.unwrap();
        assert_eq!(index_js_content, b"console.log('package-a');");

        // Test: Read nested directory /project/node_modules/a/node_modules/b
        // This should only return target content (not combine)
        let entries_b = crate::read_dir(&dst_b).await.unwrap();
        let entry_names_b: Vec<String> = entries_b
            .into_iter()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();

        println!("Directory entries for {}: {:?}", dst_b, entry_names_b);

        // Should only contain: package.json, index.js (no node_modules, no fuse.link)
        assert!(!entry_names_b.contains(&"node_modules".to_string()));
        assert!(entry_names_b.contains(&"package.json".to_string()));
        assert!(entry_names_b.contains(&"index.js".to_string()));
        assert!(!entry_names_b.contains(&"fuse.link".to_string()));
    }

    #[wasm_bindgen_test]
    async fn test_simplified_fuse_link_nested_structure() {
        let base_path = create_test_dir("test-simplified-fuse-link-nested").await;

        // Test nested structure: a depends on b
        let src_a = format!("{}/stores/a/unpack", base_path);
        let src_b = format!("{}/stores/b/unpack", base_path);
        let dst_a = format!("{}/node_modules/a", base_path);
        let dst_b = format!("{}/node_modules/a/node_modules/b", base_path);



        // Create source directories
        tokio_fs_ext::create_dir_all(&src_a).await.unwrap();
        tokio_fs_ext::create_dir_all(&src_b).await.unwrap();
        tokio_fs_ext::write(format!("{}/package.json", src_a), b"{\"name\":\"a\"}").await.unwrap();
        tokio_fs_ext::write(format!("{}/package.json", src_b), b"{\"name\":\"b\"}").await.unwrap();

        // Create fuse links
        fuse_link(&src_a, &dst_a).await.unwrap();
        fuse_link(&src_b, &dst_b).await.unwrap();

        // Verify fuse.link files were created
        let fuse_link_a = format!("{}/fuse.link", dst_a);
        let fuse_link_b = format!("{}/fuse.link", dst_b);

        let link_content_a = tokio_fs_ext::read_to_string(&fuse_link_a).await.unwrap();
        let link_content_b = tokio_fs_ext::read_to_string(&fuse_link_b).await.unwrap();

        println!("fuse_link_a content: {}", link_content_a);
        println!("fuse_link_b content: {}", link_content_b);

        assert_eq!(link_content_a.trim(), src_a);
        assert_eq!(link_content_b.trim(), src_b);

        // Verify we can read through the fuse links
        let test_file_a = format!("{}/package.json", dst_a);
        let test_file_b = format!("{}/package.json", dst_b);

        println!("test_file_a: {}", test_file_a);
        println!("test_file_b: {}", test_file_b);

        let content_a = crate::read(&test_file_a).await.unwrap();
        println!("content_a: {:?}", content_a);

        println!("About to read test_file_b: {}", test_file_b);

        // Add debug info before reading
        let fuse_link_path = get_fuse_link_path(&test_file_b);
        println!("fuse_link_path for test_file_b: {:?}", fuse_link_path);

        if let Some(ref fuse_path) = fuse_link_path {
            match tokio_fs_ext::read_to_string(fuse_path).await {
                Ok(content) => println!("fuse.link content: {}", content),
                Err(e) => println!("Failed to read fuse.link: {:?}", e),
            }
        }

        let content_b = crate::read(&test_file_b).await.unwrap();
        println!("content_b: {:?}", content_b);

        assert_eq!(content_a, b"{\"name\":\"a\"}");
        assert_eq!(content_b, b"{\"name\":\"b\"}");
    }

    #[wasm_bindgen_test]
    async fn test_simplified_fuse_link_deep_nested() {
        let base_path = create_test_dir("test-simplified-fuse-link-deep-nested").await;

        // Test deep nested structure: a -> b -> c
        let src_a = format!("{}/stores/a/unpack", base_path);
        let src_b = format!("{}/stores/b/unpack", base_path);
        let src_c = format!("{}/stores/c/unpack", base_path);

        let dst_a = format!("{}/node_modules/a", base_path);
        let dst_b = format!("{}/node_modules/a/node_modules/b", base_path);
        let dst_c = format!("{}/node_modules/a/node_modules/b/node_modules/c", base_path);

        // Create source directories
        tokio_fs_ext::create_dir_all(&src_a).await.unwrap();
        tokio_fs_ext::create_dir_all(&src_b).await.unwrap();
        tokio_fs_ext::create_dir_all(&src_c).await.unwrap();

        tokio_fs_ext::write(format!("{}/package.json", src_a), b"{\"name\":\"a\"}").await.unwrap();
        tokio_fs_ext::write(format!("{}/package.json", src_b), b"{\"name\":\"b\"}").await.unwrap();
        tokio_fs_ext::write(format!("{}/package.json", src_c), b"{\"name\":\"c\"}").await.unwrap();

        // Create fuse links
        fuse_link(&src_a, &dst_a).await.unwrap();
        fuse_link(&src_b, &dst_b).await.unwrap();
        fuse_link(&src_c, &dst_c).await.unwrap();

        // Verify we can read through all fuse links
        let test_file_c = format!("{}/package.json", dst_c);
        let content_c = crate::read(&test_file_c).await.unwrap();
        assert_eq!(content_c, b"{\"name\":\"c\"}");
    }

    #[wasm_bindgen_test]
    async fn test_simplified_fuse_link_scoped_nested() {
        let base_path = create_test_dir("test-simplified-fuse-link-scoped-nested").await;

        // Test scoped package with nested dependency
        let src_a = format!("{}/stores/@a/b/unpack", base_path);
        let src_c = format!("{}/stores/@c/d/unpack", base_path);

        let dst_a = format!("{}/node_modules/@a/b", base_path);
        let dst_c = format!("{}/node_modules/@a/b/node_modules/@c/d", base_path);

        // Create source directories
        tokio_fs_ext::create_dir_all(&src_a).await.unwrap();
        tokio_fs_ext::create_dir_all(&src_c).await.unwrap();

        tokio_fs_ext::write(format!("{}/package.json", src_a), b"{\"name\":\"@a/b\"}").await.unwrap();
        tokio_fs_ext::write(format!("{}/package.json", src_c), b"{\"name\":\"@c/d\"}").await.unwrap();

        // Create fuse links
        fuse_link(&src_a, &dst_a).await.unwrap();
        fuse_link(&src_c, &dst_c).await.unwrap();

        // Verify we can read through all fuse links
        let test_file_c = format!("{}/package.json", dst_c);
        let content_c = crate::read(&test_file_c).await.unwrap();
        assert_eq!(content_c, b"{\"name\":\"@c/d\"}");
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_basic() {
        // Test basic package: ./node_modules/c/index.js -> ./node_modules/c/fuse.link
        let path = Path::new("./node_modules/c/index.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/c/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_scoped() {
        // Test scoped package: ./node_modules/@a/b/package.json -> ./node_modules/@a/b/fuse.link
        let path = Path::new("./node_modules/@a/b/package.json");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/@a/b/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_nested() {
        // Test nested node_modules: ./node_modules/c/node_modules/d/types.js -> ./node_modules/c/node_modules/d/fuse.link
        let path = Path::new("./node_modules/c/node_modules/d/types.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/c/node_modules/d/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_scoped_nested() {
        // Test nested scoped package: ./node_modules/@a/b/node_modules/@c/d/index.js -> ./node_modules/@a/b/node_modules/@c/d/fuse.link
        let path = Path::new("./node_modules/@a/b/node_modules/@c/d/index.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/@a/b/node_modules/@c/d/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_deep_nested() {
        // Test deep nested: ./node_modules/a/node_modules/b/node_modules/c/package.json -> ./node_modules/a/node_modules/b/node_modules/c/fuse.link
        let path = Path::new("./node_modules/a/node_modules/b/node_modules/c/package.json");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/a/node_modules/b/node_modules/c/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_no_node_modules() {
        // Test path without node_modules
        let path = Path::new("./some/other/path/file.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, None);
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_direct_node_modules() {
        // Test direct node_modules path: ./node_modules/a -> ./node_modules/a/fuse.link
        let path = Path::new("./node_modules/a");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/a/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_scoped_direct() {
        // Test direct scoped path: ./node_modules/@a/b -> ./node_modules/@a/b/fuse.link
        let path = Path::new("./node_modules/@a/b");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/@a/b/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_with_file() {
        // Test with file in package: ./node_modules/lodash/cloneDeep.js -> ./node_modules/lodash/fuse.link
        let path = Path::new("./node_modules/lodash/cloneDeep.js");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("./node_modules/lodash/fuse.link").to_path_buf()));
    }

    #[wasm_bindgen_test]
    async fn test_get_fuse_link_path_with_subdirectory() {
        // Test with subdirectory: ./node_modules/@types/node/fs/promises.d.ts -> ./node_modules/@types/node/fuse.link
        let path = Path::new("/node_modules/@types/node/fs/promises.d.ts");
        let result = get_fuse_link_path(path);
        assert_eq!(result, Some(Path::new("/node_modules/@types/node/fuse.link").to_path_buf()));
    }
}
