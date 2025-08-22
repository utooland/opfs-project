#![cfg(all(target_family = "wasm", target_os = "unknown"))]

use std::{io::Result, path::{Path, PathBuf}};



use tokio_fs_ext::DirEntry;

mod fuse;
mod package_lock;
pub mod package_manager;
mod util;

pub use fuse::{read, read_dir};

/// Read file content as bytes (without fuse.link support)
pub(crate) async fn read_without_fuse_link(path: &str) -> Result<Vec<u8>> {
    let prepared_path = crate::util::prepare_path(path);
    let content = tokio_fs_ext::read(&prepared_path).await?;
    Ok(content)
}

/// Set current working directory
pub fn set_cwd(path: impl AsRef<Path>) {
    tokio_fs_ext::set_current_dir(path).unwrap();
}

/// Read current working directory
pub fn get_cwd() -> PathBuf {
    tokio_fs_ext::current_dir().unwrap()
}
