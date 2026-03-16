# OPFS Project

A Rust library for managing npm-style projects on the Origin Private File System (OPFS) in WebAssembly. Provides fuse-link indirection, a content-addressable store, and streaming tgz extraction.

## Features

- **Struct-based API** — all state owned by `OpfsProject` (config, caches, store)
- **Fuse links** — `fuse.link` files map `node_modules/<pkg>/` to extracted directories in the store
- **Content-addressable store** — tgz files stored by integrity hash (SHA-512 / SHA-1)
- **Streaming extraction** — tgz packages extracted to disk during install via `GzDecoder` + `tar::Archive` (no full decompressed buffer in memory)
- **Skip-if-extracted** — re-installs skip already-extracted packages
- **Async I/O** — all file operations are async via `tokio-fs-ext`

## Architecture

```
OpfsProject
├── Config         — store root, cache budgets
├── FuseFs         — fuse-link resolution + tgz extraction
│   └── link_cache — PathBuf → Arc<FuseLink> (LRU)
└── Store          — download, verify (sha512/sha1), save tgz
```

### Read path

```
read("node_modules/pkg/index.js")
  → locate_fuse_link_file()       zero-alloc path walk
  → read_fuse_link()              cache hit: Arc::clone (no IO)
  → tokio_fs_ext::read(dir/relative)  plain filesystem read
```

## Usage

```rust
use opfs_project::{OpfsProject, Config};

let project = OpfsProject::default();

// Fuse-aware reads (transparent redirection under node_modules)
let content = project.read("node_modules/react/index.js").await?;
let entries = project.read_dir("node_modules/react/lib").await?;

// Install from package-lock.json
use opfs_project::package_lock::PackageLock;
let lock = PackageLock::from_json(json_str)?;
project.install(&lock, &Default::default()).await?;
```

## Configuration

| Field | Default | Description |
|-------|---------|-------------|
| `store_root` | `/stores` | Root directory for tgz store |
| `fuse_cache_max_entries` | 10,000 | Fuse-link path cache capacity |
| `max_concurrent_downloads` | 20 | Parallel HTTP downloads |
| `download_retries` | 3 | Retry count for failed downloads |
| `retry_base_delay_ms` | 500 | Exponential backoff base delay |

## Testing

```bash
# native tests
cargo test

# wasm tests (requires chromedriver)
brew install --cask chromedriver  # macOS
CHROMEDRIVER=$(which chromedriver) cargo test --target wasm32-unknown-unknown

# interactive wasm tests
brew install wasm-pack
wasm-pack test --chrome
```

## License

MIT
