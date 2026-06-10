//! Package install orchestration.
//!
//! This module handles only the install workflow — grouping packages by
//! tgz URL, downloading via [`Store`], and creating fuse links via
//! [`FuseFs`]. All I/O is delegated to those subsystems.

use std::collections::HashMap;
use std::path::Path;

use futures::stream::{self, StreamExt};

use crate::error::OpfsError;
use crate::package_lock::{LockPackage, PackageLock};
use crate::project::OpfsProject;

/// Types of dependencies that can be omitted during install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OmitType {
    /// Skip dev dependencies (packages with `dev: true`)
    Dev,
    /// Skip optional dependencies (packages with `optional: true`)
    Optional,
}

/// Options for [`OpfsProject::install`].
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Maximum concurrent downloads (overrides [`Config::max_concurrent_downloads`])
    pub max_concurrent_downloads: Option<usize>,
    /// Types of dependencies to skip
    pub omit: Vec<OmitType>,
}

// ── internal grouping ────────────────────────────────────────────────────

struct PackageGroup {
    name: String,
    version: String,
    tgz_url: String,
    integrity: Option<String>,
    shasum: Option<String>,
    target_paths: Vec<String>,
}

struct FetchedPackage {
    name: String,
    version: String,
    tgz_url: String,
    target_paths: Vec<String>,
    was_fresh: bool,
}

fn should_omit(pkg: &LockPackage, omit: &[OmitType]) -> bool {
    omit.iter().any(|o| match o {
        OmitType::Dev => pkg.dev == Some(true),
        OmitType::Optional => pkg.optional == Some(true),
    })
}

// ── public entry point ───────────────────────────────────────────────────

/// Install all packages from a lock file.
///
/// Called by [`OpfsProject::install`] — not intended for direct use.
pub(crate) async fn install(
    project: &OpfsProject,
    lock: &PackageLock,
    opts: &InstallOptions,
) -> Result<(), OpfsError> {
    let omit = &opts.omit;

    // 1. Group packages by tgz URL (deduplication)
    let mut groups: HashMap<String, PackageGroup> = HashMap::new();

    for (path, pkg) in lock.packages.iter().filter(|(p, _)| !p.is_empty()) {
        if should_omit(pkg, omit) {
            continue;
        }

        // Skip optional packages with platform constraints (binary, won't work in WASM)
        if pkg.optional == Some(true) && (pkg.os.is_some() || pkg.cpu.is_some()) {
            continue;
        }

        let name = pkg.get_name(path).into_owned();
        let version = pkg.get_version().into_owned();
        let tgz_url = match &pkg.resolved {
            Some(u) => u.clone(),
            None => {
                tracing::warn!("{name}@{version}: no resolved URL, skipping");
                continue;
            }
        };

        groups
            .entry(tgz_url.clone())
            .or_insert_with(|| PackageGroup {
                name,
                version,
                tgz_url,
                integrity: pkg.integrity.clone(),
                shasum: pkg.shasum.clone(),
                target_paths: Vec::new(),
            })
            .target_paths
            .push(path.clone());
    }

    // 2. Fetch all packages (cached or download) with integrity verification
    let store = project.store();
    let fuse = project.fuse_fs();
    let max_concurrent = opts
        .max_concurrent_downloads
        .unwrap_or(project.config().max_concurrent_downloads);

    // Issue #3: Reuse outer `store` reference — &Store is Copy, no need to
    // re-borrow from project inside each closure.
    let results: Vec<_> = stream::iter(groups.into_values().map(|g| async move {
        let was_fresh = store
            .ensure_tgz(
                &g.name,
                &g.version,
                &g.tgz_url,
                g.integrity.as_deref(),
                g.shasum.as_deref(),
            )
            .await?;
        Ok::<_, OpfsError>(FetchedPackage {
            name: g.name,
            version: g.version,
            tgz_url: g.tgz_url,
            target_paths: g.target_paths,
            was_fresh,
        })
    }))
    .buffer_unordered(max_concurrent)
    .collect()
    .await;

    // 3. Create fuse links **concurrently** for all successful fetches, collect errors.
    //    (Issue #1: previously this was a serial loop — now uses buffer_unordered.)
    let mut first_error: Option<OpfsError> = None;

    let successful: Vec<_> = results
        .into_iter()
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
                None
            }
        })
        .collect();

    let link_results: Vec<_> = stream::iter(successful.into_iter().map(|package| {
        let tgz_path = store.tgz_path(&package.name, &package.tgz_url);
        async move {
            // If the tgz was re-downloaded (e.g. cached copy failed
            // integrity), delete the stale sentinel so that
            // extract_tgz_to_dir is forced to re-extract.
            if package.was_fresh {
                let sentinel = std::path::PathBuf::from(format!(
                    "{}._resolved",
                    tgz_path.with_extension("").display()
                ));
                let _ = tokio_fs_ext::remove_file(&sentinel).await;
            }
            link_and_warm_cache(fuse, &package, &tgz_path).await
        }
    }))
    .buffer_unordered(max_concurrent)
    .collect()
    .await;

    for result in link_results {
        if let Err(e) = result {
            if first_error.is_none() {
                first_error = Some(e);
            }
        }
    }

    if let Some(e) = first_error {
        return Err(e);
    }

    Ok(())
}

/// Create fuse links and warm the cache for a set of target paths.
async fn link_and_warm_cache(
    fuse: &crate::fuse_fs::FuseFs,
    package: &FetchedPackage,
    tgz_path: &Path,
) -> std::result::Result<(), OpfsError> {
    let extracted_dir = match fuse.extract_tgz_to_dir(tgz_path).await {
        Ok(dir) => dir,
        Err(e) => return Err(extract_tgz_error(package, tgz_path, e).await),
    };

    // Create all fuse links concurrently within this group.
    futures::future::try_join_all(package.target_paths.iter().map(|target| {
        let extracted_dir = &extracted_dir;
        async move {
            let dst = std::path::PathBuf::from(target);
            fuse.create_fuse_link(extracted_dir, &dst)
                .await
                .map_err(|e| OpfsError::Other(format!("fuse link for {target}: {e}")))?;
            fuse.warm_link_cache(&dst, extracted_dir);
            Ok::<_, OpfsError>(())
        }
    }))
    .await?;

    Ok(())
}

async fn extract_tgz_error(
    package: &FetchedPackage,
    tgz_path: &Path,
    source: std::io::Error,
) -> OpfsError {
    // Best-effort current size. Another tab may change the file after the
    // extraction failure, so this is diagnostic context rather than a strict
    // read-time measurement.
    let current_bytes = tokio_fs_ext::metadata(tgz_path)
        .await
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len());

    OpfsError::Other(format!(
        "extract tgz failed for {}@{} (url={}, path={}, current_bytes={:?}, targets={}, was_fresh={}): {source}",
        package.name,
        package.version,
        package.tgz_url,
        tgz_path.display(),
        current_bytes,
        package.target_paths.len(),
        package.was_fresh
    ))
}
