# OPFS Project

A Rust library for managing npm-style projects on the Origin Private File System (OPFS) in WebAssembly. Provides lazy tgz extraction via fuse links, a content-addressable store, and zero-copy file reads.

## Features

- **Struct-based API** — all state owned by `OpfsProject` (config, caches, store)
- **Fuse links** — `fuse.link` files map `node_modules/<pkg>/` to tgz entries in the store, enabling lazy on-demand extraction
- **Zero-copy reads** — decompressed tar kept as a single `Bytes` buffer; individual files served via `Bytes::slice()` (no per-file allocations)
- **O(1) lookups** — `is_dir`, `get_file`, `list_dir` all backed by `HashMap`/`HashSet`
- **LRU cache** — configurable budget for decompressed tar data with automatic eviction
- **In-flight dedup** — concurrent reads of the same tgz only decompress once
- **Content-addressable store** — tgz files stored by integrity hash (SHA-512 / SHA-1)
- **Async I/O** — all file operations are async via `tokio-fs-ext`
- **Lazy / non-lazy tgz mode** — configurable via `Config::lazy_tgz`

## Architecture

```
OpfsProject
├── Config         — store root, cache budgets, lazy_tgz toggle
├── FuseFs         — fuse-link resolution + tar index
│   ├── link_cache — PathBuf → Arc<FuseLink> (LRU)
│   └── tar_index  — PathBuf → TgzEntry (LRU, zero-copy Bytes)
└── Store          — download, verify (sha512/sha1), save tgz
```

### Read path (lazy_tgz = true, default)

```
read("node_modules/pkg/index.js")
  → locate_fuse_link_file()       zero-alloc path walk
  → read_fuse_link()              cache hit: Arc::clone (no IO)
  → extract_file()
      1. tar_index.get_file()     O(1), Bytes::slice(), zero IO
      2. ensure_tgz_cached()      decompress tgz on first access
```

### Read path (lazy_tgz = false)

```
read("node_modules/pkg/index.js")
  → locate_fuse_link_file()       zero-alloc path walk
  → read_fuse_link()              cache hit: Arc::clone (no IO)
  → tokio_fs_ext::read(dir/relative)  plain filesystem read
```

## Usage

```rust
use opfs_project::{OpfsProject, Config};

// Lazy mode (default) — reads from in-memory tar index
let project = OpfsProject::default();

// Non-lazy mode — extracts tgz to real files during install
let project = OpfsProject::new(Config {
    lazy_tgz: false,
    ..Config::default()
});

// Fuse-aware reads (transparent tgz extraction under node_modules)
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
| `tar_cache_max_bytes` | 100 MB | In-memory tar index budget |
| `fuse_cache_max_entries` | 10,000 | Fuse-link path cache capacity |
| `max_concurrent_downloads` | 20 | Parallel HTTP downloads |
| `download_retries` | 3 | Retry count for failed downloads |
| `retry_base_delay_ms` | 500 | Exponential backoff base delay |
| `lazy_tgz` | `true` | `true`: read from in-memory tar index; `false`: extract to disk during install |

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
