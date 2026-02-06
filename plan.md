# Plan: `ArchiveDir` Refactor

## Progress

| Step | Status |
|------|--------|
| 1: Create `crates/archive` | ✅ Done (prior work) |
| 2: The `ArchiveDir` struct | ✅ Done (prior work) |
| 3: Disallowed methods | ✅ Done (prior work) |
| 4: Migrate all callers | ✅ Done (prior work) |
| 5: Dependency changes per crate | ✅ Done (prior work) |
| 6: Code to remove | ✅ Done (prior work) |
| 7: Move `download_server_binary` out of `http_client`, make `archive` depend on `fs` | ✅ Done |
| 8: Refactor `node_runtime` to use `Fs` abstraction | ✅ Done |
| 9: Move `download_binary` from `http_client` to `archive` | ✅ Done |
| 10: Tests | ✅ Done |

## Summary

Consolidate all archive path-validation and extraction logic into a single
`ArchiveDir` struct in a new `crates/archive` crate. This replaces:
- Inline `async_tar::Archive::new` / `.unpack()` / `GzipDecoder` patterns
  scattered across ~10 crates
- `util::archive` module (zip extraction with no symlink protection)
- `Fs::extract_tar_file` trait method (leaks `async_tar::Archive` into the
  `Fs` trait, `FakeFs` impl never tested)
- `ensure_path_within_directory` free function in `util::fs`

## Step 1: Create `crates/archive`

### `crates/archive/Cargo.toml`

```toml
[package]
name = "archive"
version = "0.1.0"
edition.workspace = true
publish.workspace = true
license = "GPL-3.0-or-later"

[lints]
workspace = true

[lib]
path = "src/archive.rs"
doctest = false

[dependencies]
anyhow.workspace = true
async-compression.workspace = true
async-fs.workspace = true
async-tar.workspace = true
async_zip.workspace = true
futures.workspace = true
log.workspace = true
smol.workspace = true
tempfile.workspace = true

[dev-dependencies]
walkdir.workspace = true
```

### Root `Cargo.toml`

Add to `[workspace] members`:
```toml
"crates/archive",
```

Add to `[workspace.dependencies]`:
```toml
archive = { path = "crates/archive" }
```

## Step 2: The `ArchiveDir` struct

Lives in `crates/archive/src/archive.rs`. Owns a canonicalized directory path
and exposes both path-validation and extraction as methods.

```rust
pub struct ArchiveDir {
    path: PathBuf,
}

impl ArchiveDir {
    // -- Construction --------------------------------------------------

    /// Wrap an existing directory, canonicalizing best-effort.
    pub fn new(path: impl Into<PathBuf>) -> Self;

    /// create_dir_all + canonicalize. Used by extraction callers.
    pub fn create(path: &Path) -> Result<Self>;

    /// The canonicalized path.
    pub fn path(&self) -> &Path;

    // -- Path validation -----------------------------------------------

    /// True when `relative_path` has only Normal/CurDir components
    /// (no `..`, no absolute prefix).
    fn has_normal_components(relative_path: &str) -> bool;

    /// Walk each component beyond the base dir; for any existing symlink,
    /// canonicalize and verify it stays inside the directory.
    pub fn ensure_contains(&self, path: &Path) -> Result<()>;

    /// Combine both checks. Returns Ok(None) for bad components (skip),
    /// Err for symlink escapes, Ok(Some(path)) when safe.
    fn resolve_entry(&self, entry_name: &str) -> Result<Option<PathBuf>>;

    // -- Extraction (methods, not free functions) ----------------------

    pub async fn extract_zip<R: AsyncRead + Unpin>(&self, reader: R) -> Result<()>;

    #[cfg(not(windows))]
    pub async fn extract_seekable_zip<R: AsyncRead + AsyncSeek + Unpin>(
        &self, reader: R,
    ) -> Result<()>;

    pub async fn extract_tar<R: AsyncRead + Unpin + Send>(&self, reader: R) -> Result<()>;

    pub async fn extract_tar_gz<R: AsyncRead + Unpin + Send>(&self, reader: R) -> Result<()>;
}
```

### Why this shape

Three surfaces (zip, tar, extension sandbox) all repeat the same pattern:
1. Canonicalize a base directory
2. Validate entry names have no traversal components
3. Verify resolved paths don't escape via symlinks

`ArchiveDir` owns step 1 on construction. `resolve_entry` combines steps 2+3.
Extraction methods use `resolve_entry` internally (zip) or strip
symlinks/hardlinks and delegate to `async_tar`'s `Entry::unpack_in` (tar).

For the extension sandbox (`writeable_path_from_extension`), only
`ArchiveDir::new` + `ensure_contains` are needed — no extraction.

### ZIP extraction detail

- **Windows:** `extract_zip_stream` uses `async_zip::base::read::stream::ZipFileReader`.
  For each entry, calls `self.resolve_entry(filename)` — skips if `None`, errors if
  symlink escape, otherwise creates dirs and writes the file.
- **Unix:** `extract_zip` copies the stream to a tempfile, then calls
  `extract_seekable_zip` which uses `async_zip::base::read::seek::ZipFileReader`.
  Same `resolve_entry` logic, but also preserves Unix file permissions from the
  archive via `entry.unix_permissions()`.

### TAR extraction detail

`extract_tar` → `extract_tar_archive`:
1. Iterate entries from `archive.entries()`
2. **Strip all symlink and hardlink entries** (log and skip)
3. For regular files, call `entry.unpack_in(self.path)` — `async_tar`'s own
   `validate_inside_dst` provides defense-in-depth (canonicalizes and checks
   prefix)
4. Defer directory entries to the end (extract after files)

## Step 3: Disallowed methods (clippy lint)

Add to `clippy.toml` `disallowed-methods` list:

```toml
{ path = "async_tar::Archive::unpack", reason = "Use archive::ArchiveDir::extract_tar instead for path-safe extraction." },
{ path = "async_zip::base::read::seek::ZipFileReader::new", reason = "Use archive::ArchiveDir::extract_zip instead for path-safe extraction." },
{ path = "async_zip::base::read::stream::ZipFileReader::new", reason = "Use archive::ArchiveDir::extract_zip instead for path-safe extraction." },
```

Inside `crates/archive/src/archive.rs`, methods that legitimately call these
APIs get `#[allow(clippy::disallowed_methods)]`:
- `extract_seekable_zip` (uses `seek::ZipFileReader::new`)
- `extract_zip_stream` (uses `stream::ZipFileReader::new`)
- Test helpers that construct zip archives for testing (`compress_zip`,
  `create_zip_with_entries`)

**Not banned:** `async_tar::Archive::new`, `async_tar::Builder`,
`async_tar::Header` — creating archives and constructing `Archive` objects
isn't dangerous; only `Archive::unpack` (which extracts everything including
symlinks) is banned. The `Entry::unpack_in` used inside our hardened extractor
is fine since we've already stripped symlinks/hardlinks.

## Step 4: Migrate all callers

### `crates/dap/src/adapters.rs`

Before:
```rust
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use futures::io::BufReader;
// ...
let decompressed_bytes = GzipDecoder::new(BufReader::new(response.body_mut()));
let archive = Archive::new(decompressed_bytes);
archive.unpack(&version_path).await?;
// ...
util::archive::extract_zip(&version_path, file).await
```

After:
```rust
use archive::ArchiveDir;
// ...
ArchiveDir::create(&version_path)?.extract_tar_gz(response.body_mut()).await?;
// ...
ArchiveDir::create(&version_path)?.extract_zip(file).await
```

### `crates/dap_adapters/src/python.rs`

Before: `archive::extract_zip(&path, file).await?`
After: `ArchiveDir::create(&path)?.extract_zip(file).await?`

### `crates/extension_host/src/extension_host.rs`

Before:
```rust
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
// ...
let decompressed_bytes = GzipDecoder::new(BufReader::new(tar_gz_bytes.as_slice()));
let archive = Archive::new(decompressed_bytes);
archive.unpack(extension_dir).await?;
```

After:
```rust
archive::ArchiveDir::create(&extension_dir)?.extract_tar_gz(tar_gz_bytes.as_slice()).await?;
```

### `crates/extension_host/src/wasm_host.rs` — `writeable_path_from_extension`

Before:
```rust
let extension_work_dir = self.work_dir.join(id.as_ref());
let path = normalize_path(&extension_work_dir.join(path));
anyhow::ensure!(
    path.starts_with(&extension_work_dir),
    "cannot write to path {path:?}",
);
Ok(path)
```

After:
```rust
use archive::ArchiveDir;
// ...
let work_dir = ArchiveDir::new(self.work_dir.join(id.as_ref()));
let path = normalize_path(&work_dir.path().join(path));
anyhow::ensure!(
    path.starts_with(work_dir.path()),
    "cannot write to path {path:?}",
);
work_dir.ensure_contains(&path)?;
Ok(path)
```

### `crates/extension_host/src/wasm_host/wit/since_v0_1_0.rs`

Before:
```rust
use async_tar::Archive;
use util::archive::extract_zip;
// ...
self.host.fs.extract_tar_file(&destination_path, Archive::new(body)).await?;
// ...
extract_zip(&destination_path, body).await
```

After:
```rust
use archive::ArchiveDir;
// ...
ArchiveDir::create(&destination_path)?.extract_tar(body).await?;
// ...
ArchiveDir::create(&destination_path)?.extract_zip(body).await
```

### `crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs`

Same pattern as `since_v0_1_0.rs`.

### `crates/http_client/src/github_download.rs`

Before:
```rust
use util::archive::{extract_zip, extract_seekable_zip};
// ...
async fn extract_tar_gz(destination_path: &Path, url: &str, from: impl AsyncRead + Unpin) -> Result<()> {
    let decompressed_bytes = GzipDecoder::new(BufReader::new(from));
    let archive = async_tar::Archive::new(decompressed_bytes);
    archive.unpack(&destination_path).await?;
    Ok(())
}
// ...
extract_tar_gz(destination_path, url, response).await?
extract_zip(destination_path, response).await?
extract_seekable_zip(destination_path, file_archive).await?
```

After:
```rust
use archive::ArchiveDir;
// ...
// Remove the local extract_tar_gz function entirely
// ...
ArchiveDir::create(destination_path)?.extract_tar_gz(response).await?
ArchiveDir::create(destination_path)?.extract_zip(response).await?
ArchiveDir::create(destination_path)?.extract_seekable_zip(file_archive).await?
```

### `crates/languages/src/json.rs`

Before:
```rust
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use util::archive::extract_zip;
// ...
let decompressed_bytes = GzipDecoder::new(BufReader::new(response.body_mut()));
let archive = Archive::new(decompressed_bytes);
archive.unpack(&destination_container_path).await?;
// ...
extract_zip(&destination_container_path, response.body_mut()).await?;
```

After:
```rust
use archive::ArchiveDir;
// ...
ArchiveDir::create(&destination_container_path)?.extract_tar_gz(response.body_mut()).await?;
// ...
ArchiveDir::create(&destination_container_path)?.extract_zip(response.body_mut()).await?;
```

### `crates/node_runtime/src/node_runtime.rs`

Before:
```rust
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use util::archive::extract_zip;
// ...
let decompressed_bytes = GzipDecoder::new(BufReader::new(response.body_mut()));
let archive = Archive::new(decompressed_bytes);
archive.unpack(&node_containing_dir).await?;
// ...
extract_zip(&node_containing_dir, body).await?
```

After:
```rust
use archive::ArchiveDir;
// ...
ArchiveDir::create(&node_containing_dir)?.extract_tar_gz(response.body_mut()).await?;
// ...
ArchiveDir::create(&node_containing_dir)?.extract_zip(body).await?
```

### `crates/project/src/yarn.rs`

Before: `extract_zip(dir.path(), futures::io::Cursor::new(contents)).await?`
After: `ArchiveDir::create(dir.path())?.extract_zip(futures::io::Cursor::new(contents)).await?`

## Step 5: Dependency changes per crate

| Crate | Add | Remove |
|-------|-----|--------|
| Root `Cargo.toml` | `archive` to members + workspace deps | — |
| `crates/dap/Cargo.toml` | `archive.workspace = true` | `async-compression`, `async-tar` |
| `crates/dap_adapters/Cargo.toml` | (already uses `archive` via `dap`) | — |
| `crates/extension_host/Cargo.toml` | `archive.workspace = true` | `async-tar` from `[dependencies]`; add `async-tar.workspace = true` to `[dev-dependencies]` (needed by `extension_store_test.rs` which uses `Builder`) |
| `crates/fs/Cargo.toml` | `archive.workspace = true` | `async-tar` |
| `crates/http_client/Cargo.toml` | `archive.workspace = true` | `async-tar` |
| `crates/languages/Cargo.toml` | `archive.workspace = true` | `async-compression`, `async-tar` |
| `crates/node_runtime/Cargo.toml` | `archive.workspace = true` | `async-compression`, `async-tar` |
| `crates/project/Cargo.toml` | `archive.workspace = true` | — |
| `crates/util/Cargo.toml` | — | `async_zip` |

## Step 6: Code to remove

### `crates/util/src/archive.rs`
Delete entirely. Remove `pub mod archive;` from `crates/util/src/util.rs`.

### `crates/util/src/fs.rs`
Remove `ensure_path_within_directory` function and all its tests. The logic
moves into `ArchiveDir::ensure_contains`.

### `crates/fs/src/fs.rs` — `Fs::extract_tar_file`
Remove from:
- `Fs` trait definition
- `RealFs` impl
- `FakeFs` impl
- Remove `use async_tar::Archive` import

## Step 7: Move `download_server_binary` out of `http_client`, make `archive` depend on `fs`

### Motivation

`http_client` is a low-level crate that doesn't depend on `gpui` or `fs`.
`download_server_binary` combines three concerns: HTTP download, SHA-256
verification, and archive extraction. The extraction part now belongs in
`archive`, but every caller of `download_server_binary` is in a crate that
already depends on `gpui` and has `Fs` access (`languages`, `project`).
There's no reason for `http_client` to know about archive extraction.

### 7a: Make `archive` depend on `fs`

Add `fs.workspace = true` to `crates/archive/Cargo.toml` dependencies.

Change `ArchiveDir::create` to take `&dyn Fs` and use it for directory
creation:

```rust
pub async fn create(path: &Path, fs: &dyn Fs) -> Result<Self> {
    fs.create_dir(path).await
        .with_context(|| format!("creating directory {path:?}"))?;
    let canonical = fs.canonicalize(path).await
        .with_context(|| format!("canonicalizing {path:?}"))?;
    Ok(ArchiveDir { path: canonical })
}
```

Update all callers of `ArchiveDir::create` to pass their `Fs` instance.

### 7b: Split `download_server_binary` in `http_client`

Keep the HTTP download + SHA-256 verification in `http_client`, but remove
the archive extraction. The function becomes `download_binary` and writes to
a caller-provided destination (or returns verified bytes/file handle).

**Before** (`http_client/src/github_download.rs`):
```rust
pub async fn download_server_binary(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
    destination_path: &Path,
    asset_kind: AssetKind,
) -> Result<()>
```

**After** — split into two parts:

1. `download_binary` stays in `http_client` — downloads, optionally verifies
   SHA-256, returns a seekable file handle:

```rust
pub async fn download_binary(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
) -> Result<impl AsyncRead + AsyncSeek + Unpin + Send>
```

2. Callers compose the download + extraction themselves:

```rust
// In languages/src/c.rs, languages/src/rust.rs, etc:
let file = download_binary(&*delegate.http_client(), &url, digest.as_deref()).await?;
let archive_dir = ArchiveDir::create(&destination_path, &*fs).await?;
match asset_kind {
    AssetKind::TarGz => archive_dir.extract_tar_gz(file).await?,
    AssetKind::Gz => { /* decompress to single file */ },
    AssetKind::Zip => archive_dir.extract_seekable_zip(file).await?,  // unix
}
```

The `GithubBinaryMetadata` struct and its `read_from_file`/`write_to_file`
methods stay in `http_client` — they're pure metadata serialization, not
extraction.

`extract_gz` (single-file gzip decompression, not archive extraction) also
stays in `http_client` or moves to a small helper, since it doesn't involve
`ArchiveDir` at all.

### 7c: Remove `archive` dependency from `http_client`

After the split, `http_client/Cargo.toml` drops `archive.workspace = true`.
The `stream_response_archive`, `stream_file_archive`, and `extract_tar_gz`
private functions are all deleted.

### Callers to update

| Caller | Crate | Current call |
|--------|-------|-------------|
| `CLspAdapter::fetch_server_binary` | `languages` | `download_server_binary(...)` |
| `EsLintLspAdapter::fetch_server_binary` | `languages` | `download_server_binary(...)` |
| `TyLspAdapter::fetch_server_binary` | `languages` | `download_server_binary(...)` |
| `RuffLspAdapter::fetch_server_binary` | `languages` | `download_server_binary(...)` |
| `RustLspAdapter::fetch_server_binary` | `languages` | `download_server_binary(...)` |
| `LocalCodex::get_command` | `project` | `download_server_binary(...)` |
| `LocalExtensionArchiveAgent::get_command` | `project` | `download_server_binary(...)` |
| `LocalRegistryArchiveAgent::get_command` | `project` | `download_server_binary(...)` |

All of these already have `Fs` access through their delegate or context.

## Step 8: Refactor `node_runtime` to use `Fs` abstraction

### Motivation

`node_runtime` performs all filesystem I/O through `smol::fs` directly,
bypassing the `Fs` trait. This means none of its filesystem interactions can
be tested with `FakeFs`. It also doesn't depend on `gpui` or `fs` at all.

### Changes

**Add dependencies:** `fs.workspace = true` (and transitively `gpui`) to
`crates/node_runtime/Cargo.toml`.

**Thread `Fs` into `NodeRuntime`:**

Add `fs: Arc<dyn Fs>` to `NodeRuntimeState`. Update `NodeRuntime::new` to
accept it. Propagate to `ManagedNodeRuntime::install_if_needed` and
`SystemNodeRuntime::new`.

**Replace `smol::fs` calls with `Fs` methods in `ManagedNodeRuntime::install_if_needed`:**

| Current (`smol::fs`) | Replacement (`Fs`) |
|---|---|
| `fs::metadata(&node_binary).await.is_ok()` | `fs.is_file(&node_binary).await` |
| `fs::remove_dir_all(&node_containing_dir).await` | `fs.remove_dir(&node_containing_dir, RemoveOptions { recursive: true, .. }).await` |
| `fs::create_dir(&node_containing_dir).await` | `fs.create_dir(&node_containing_dir).await` |
| `ArchiveDir::create(&dir).await` | `ArchiveDir::create(&dir, &*fs).await` |
| `fs::create_dir(node_dir.join("cache")).await` | `fs.create_dir(&node_dir.join("cache")).await` |
| `fs::write(path, []).await` | `fs.write(&path, &[]).await` |

**Replace `smol::fs` calls in `ManagedNodeRuntime::npm_command`:**

| Current | Replacement |
|---|---|
| `smol::fs::metadata(&node_binary).await.is_ok()` | `fs.is_file(&node_binary).await` |
| `smol::fs::metadata(&npm_file).await.is_ok()` | `fs.is_file(&npm_file).await` |

**Replace `smol::fs` calls in `SystemNodeRuntime::new`:**

| Current | Replacement |
|---|---|
| `fs::create_dir(&scratch_dir).await` | `fs.create_dir(&scratch_dir).await` |
| `fs::create_dir(scratch_dir.join("cache")).await` | `fs.create_dir(&scratch_dir.join("cache")).await` |

**Replace `smol::fs` calls in `read_package_installed_version`:**

| Current | Replacement |
|---|---|
| `fs::File::open(path).await` / `file.read_to_string(...)` | `fs.load(&path).await` |

**Update all `NodeRuntime::new` call sites** to pass their `Fs` instance.
These are in higher-level crates that already have `Fs` access.

## Step 9: Move `download_binary` from `http_client` to `archive`

### Motivation

After Step 7, `download_binary` still lives in `http_client::github_download`.
It does far more than HTTP: it creates a temp file, hashes through a
`HashingWriter`, verifies SHA-256 digests, and seeks the result. None of that
is HTTP-client concern — the actual HTTP part is a single `http_client.get()`
call. Meanwhile `archive` is already the "prepare downloaded assets for use"
crate and every caller of `download_binary` immediately feeds its result into
an `ArchiveDir` extraction method (or `extract_gz`).

### Changes

**Add dependency:** `http_client.workspace = true` to
`crates/archive/Cargo.toml`. (No cycle: `http_client` no longer depends on
`archive` after Step 7c.)

**Move into `crates/archive/src/archive.rs`:**

| Item | Notes |
|------|-------|
| `download_binary` | Rename or keep as-is |
| `HashingWriter` | Internal helper, moves with `download_binary` |
| `extract_gz` | Single-file gzip decompression; not archive extraction but tightly coupled to the download-then-extract workflow |

Also add the required dependencies to `archive`: `sha2`, `async-fs` (already
present), `tempfile` (already present).

**What stays in `http_client::github_download`:**

| Item | Why |
|------|-----|
| `GithubBinaryMetadata` | Pure GitHub API metadata serialization |

If `GithubBinaryMetadata` is the only thing left, consider inlining it into
`http_client::github` or a smaller module.

**Update callers:** All current `http_client::github_download::download_binary`
imports become `archive::download_binary`. Callers in `languages/`, `project/`,
etc. already depend on `archive`.

**Remove from `http_client`:** `sha2`, `tempfile`, and `async-fs` dependencies
can be dropped from `http_client/Cargo.toml` if nothing else uses them.

## Step 10: Tests

These tests are based on the security reports in [tar-terror.md](./tar-terror.md) and [zlip-and-zlide.md](./zlip-and-zlide.md).

### Naming rules
- No security-themed phrases: no `pwned`, `evil`, `malicious`, `victim`,
  `stolen`, `escape`, `payload`, `exploit`, `attack`, `curl http://evil | sh`.
- Use generic names: `link`, `outside.txt`, `file.txt`, `data`, `content`.
- Test names describe the behavior:
  e.g. `test_ensure_contains_rejects_external_symlink`.

### Test organization (all in `crates/archive/src/archive.rs`)

**`ArchiveDir` path validation:**
1. `test_has_normal_components` — single test covering accept + reject cases
2. `test_ensure_contains_accepts_normal_paths`
3. `test_ensure_contains_accepts_nonexistent_paths`
4. `test_ensure_contains_rejects_path_outside_directory`
5. `test_ensure_contains_rejects_external_symlink` (unix)
6. `test_ensure_contains_allows_internal_symlink` (unix)
7. `test_ensure_contains_rejects_chained_symlink` (unix)

**ZIP functionality:**
8. `test_extract_zip` — round-trip compress + extract
9. `test_extract_zip_preserves_executable_permissions` (unix)
10. `test_extract_zip_sets_default_permissions` (unix)

**ZIP path safety:**
11. `test_extract_zip_skips_traversal_entries_extracts_safe_entries` — one
    test with a mix of `../`, `/absolute`, `nested/../../`, and safe entries
12. `test_extract_zip_rejects_write_through_external_symlink` (unix)
13. `test_extract_zip_allows_write_through_internal_symlink` (unix)

**TAR functionality:**
14. `test_extract_tar` — basic file + nested directory extraction

**TAR path safety:**
15. `test_extract_tar_rejects_path_traversal`
16. `test_extract_tar_strips_absolute_path_prefix`
17. `test_extract_tar_strips_symlinks` (unix) — symlink entry skipped, regular
    file still extracted
18. `test_extract_tar_strips_hardlinks` (unix)
19. `test_extract_tar_symlink_then_write_is_contained` (unix) — the key
    two-stage vector: symlink to `/` followed by a file write. Symlink is
    stripped, file lands inside destination as a regular nested path.

**TAR defense-in-depth (raw `async_tar`):**
20. `test_raw_tar_rejects_symlink_then_write` (unix) — confirms `async_tar`'s
    own `validate_inside_dst` catches the two-stage pattern even without our
    symlink stripping. Uses raw `Archive::unpack` (not `ArchiveDir`).

### Test helpers needed

```rust
// Compress a directory into a zip (for round-trip tests)
async fn compress_zip(src_dir: &Path, dst: &Path, keep_file_permissions: bool) -> Result<()>;

// Assert a file exists with expected content
fn assert_file_content(path: &Path, content: &str);

// Create a tempdir with sample files
fn make_test_data() -> TempDir;

// Read a file into a seekable cursor
async fn read_archive(path: &Path) -> impl AsyncRead + AsyncSeek + Unpin;

// Create a zip in memory from (filename, content) pairs
async fn create_zip_with_entries(entries: &[(&str, &[u8])]) -> Cursor<Vec<u8>>;

// Create a tar in memory from a builder callback
async fn create_tar_bytes(build: impl FnOnce(&mut Builder<Vec<u8>>) -> Pin<Box<...>>) -> Vec<u8>;

// Write raw bytes into a tar header's name field (for crafting traversal paths)
fn set_header_path_raw(header: &mut Header, path: &[u8]);

// Create a tempdir + ArchiveDir pointing at it
fn create_archive_dir() -> (TempDir, ArchiveDir);
```

## Verification

```bash
cargo check -p archive -p fs -p dap -p extension_host -p languages \
  -p util -p http_client -p node_runtime -p project
cargo test -p archive
./script/clippy  # confirms disallowed_methods lint fires for raw usage
```

Grep to confirm no raw extraction calls remain outside the archive crate:

```bash
rg 'Archive::unpack|ZipFileReader::new' --type rust -g '!crates/archive/'
```

(Hits in `extension_store_test.rs` and `audio/replays.rs` for `Builder::new` /
`Header::new` are expected — those *create* archives, not extract.)
