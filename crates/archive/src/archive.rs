use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;

use anyhow::{Context as _, Result};
use async_compression::futures::bufread::GzipDecoder;
use async_zip::base::read;
use fs::Fs;
#[cfg(not(windows))]
use futures::AsyncSeek;
use futures::{AsyncRead, AsyncSeekExt, AsyncWrite, io::BufReader};
use http_client::HttpClient;
use sha2::{Digest, Sha256};

pub struct ArchiveDir {
    path: PathBuf,
}

impl ArchiveDir {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let path = path.canonicalize().unwrap_or(path);
        ArchiveDir { path }
    }

    pub async fn create(path: &Path, fs: &dyn Fs) -> Result<Self> {
        fs.create_dir(path)
            .await
            .with_context(|| format!("creating directory {path:?}"))?;
        let canonical = fs
            .canonicalize(path)
            .await
            .with_context(|| format!("canonicalizing {path:?}"))?;
        Ok(ArchiveDir { path: canonical })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn has_normal_components(relative_path: &str) -> bool {
        Path::new(relative_path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    }

    pub fn ensure_contains(&self, path: &Path) -> Result<()> {
        let mut validated = self.path.clone();

        let relative = path
            .strip_prefix(&self.path)
            .with_context(|| format!("path {path:?} is not under directory {:?}", self.path))?;

        for component in relative.components() {
            match component {
                Component::Normal(segment) => {
                    validated.push(segment);
                    if validated.is_symlink() {
                        let resolved = validated
                            .canonicalize()
                            .with_context(|| format!("canonicalizing symlink {validated:?}"))?;
                        anyhow::ensure!(
                            resolved.starts_with(&self.path),
                            "symlink {validated:?} resolves to {resolved:?} which is outside {:?}",
                            self.path,
                        );
                        validated = resolved;
                    }
                }
                _ => {
                    anyhow::bail!("unexpected path component {component:?} in {path:?}");
                }
            }
        }

        Ok(())
    }

    fn resolve_entry(&self, entry_name: &str) -> Result<Option<PathBuf>> {
        if !Self::has_normal_components(entry_name) {
            return Ok(None);
        }

        let target = self.path.join(entry_name);

        let mut validated = self.path.clone();
        for component in Path::new(entry_name).components() {
            if let Component::Normal(segment) = component {
                validated.push(segment);
                if validated.is_symlink() {
                    let resolved = validated
                        .canonicalize()
                        .with_context(|| format!("canonicalizing symlink {validated:?}"))?;
                    anyhow::ensure!(
                        resolved.starts_with(&self.path),
                        "symlink {validated:?} resolves to {resolved:?} which is outside {:?}",
                        self.path,
                    );
                    validated = resolved;
                }
            }
        }

        Ok(Some(target))
    }

    #[cfg(windows)]
    pub async fn extract_zip<R: AsyncRead + Unpin>(&self, reader: R) -> Result<()> {
        self.extract_zip_stream(reader).await
    }

    #[cfg(not(windows))]
    pub async fn extract_zip<R: AsyncRead + Unpin>(&self, reader: R) -> Result<()> {
        let mut file =
            async_fs::File::from(tempfile::tempfile().context("creating a temporary file")?);
        futures::io::copy(&mut BufReader::new(reader), &mut file)
            .await
            .context("saving archive contents into the temporary file")?;
        self.extract_seekable_zip(file).await
    }

    #[cfg(windows)]
    #[allow(clippy::disallowed_methods)]
    async fn extract_zip_stream<R: AsyncRead + Unpin>(&self, reader: R) -> Result<()> {
        let mut reader = read::stream::ZipFileReader::new(BufReader::new(reader));

        while let Some(mut item) = reader.next_with_entry().await? {
            let entry_reader = item.reader_mut();
            let entry = entry_reader.entry();
            let filename = entry
                .filename()
                .as_str()
                .context("reading zip entry file name")?;

            let path = match self.resolve_entry(filename)? {
                Some(path) => path,
                None => {
                    reader = item.skip().await.context("reading next zip entry")?;
                    continue;
                }
            };

            if entry
                .dir()
                .with_context(|| format!("reading zip entry metadata for path {path:?}"))?
            {
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("creating directory {path:?}"))?;
            } else {
                let parent_dir = path
                    .parent()
                    .with_context(|| format!("no parent directory for {path:?}"))?;
                std::fs::create_dir_all(parent_dir)
                    .with_context(|| format!("creating parent directory {parent_dir:?}"))?;
                let mut file = smol::fs::File::create(&path)
                    .await
                    .with_context(|| format!("creating file {path:?}"))?;
                futures::io::copy(entry_reader, &mut file)
                    .await
                    .with_context(|| format!("extracting into file {path:?}"))?;
            }

            reader = item.skip().await.context("reading next zip entry")?;
        }

        Ok(())
    }

    #[cfg(not(windows))]
    #[allow(clippy::disallowed_methods)]
    pub async fn extract_seekable_zip<R: AsyncRead + AsyncSeek + Unpin>(
        &self,
        reader: R,
    ) -> Result<()> {
        let mut reader = read::seek::ZipFileReader::new(BufReader::new(reader))
            .await
            .context("reading the zip archive")?;

        for (i, entry) in reader.file().entries().to_vec().into_iter().enumerate() {
            let filename = entry
                .filename()
                .as_str()
                .context("reading zip entry file name")?;

            let path = match self.resolve_entry(filename)? {
                Some(path) => path,
                None => continue,
            };

            if entry
                .dir()
                .with_context(|| format!("reading zip entry metadata for path {path:?}"))?
            {
                std::fs::create_dir_all(&path)
                    .with_context(|| format!("creating directory {path:?}"))?;
            } else {
                let parent_dir = path
                    .parent()
                    .with_context(|| format!("no parent directory for {path:?}"))?;
                std::fs::create_dir_all(parent_dir)
                    .with_context(|| format!("creating parent directory {parent_dir:?}"))?;
                let mut file = smol::fs::File::create(&path)
                    .await
                    .with_context(|| format!("creating file {path:?}"))?;
                let mut entry_reader = reader
                    .reader_with_entry(i)
                    .await
                    .with_context(|| format!("reading entry for path {path:?}"))?;
                futures::io::copy(&mut entry_reader, &mut file)
                    .await
                    .with_context(|| format!("extracting into file {path:?}"))?;

                if let Some(perms) = entry.unix_permissions()
                    && perms != 0o000
                {
                    use std::os::unix::fs::PermissionsExt;
                    let permissions = std::fs::Permissions::from_mode(u32::from(perms));
                    file.set_permissions(permissions)
                        .await
                        .with_context(|| format!("setting permissions for file {path:?}"))?;
                }
            }
        }

        Ok(())
    }

    pub async fn extract_tar<R: AsyncRead + Unpin + Send>(&self, reader: R) -> Result<()> {
        let archive = async_tar::Archive::new(reader);
        self.extract_tar_archive(archive).await
    }

    pub async fn extract_tar_gz<R: AsyncRead + Unpin + Send>(&self, reader: R) -> Result<()> {
        let decompressed = GzipDecoder::new(BufReader::new(reader));
        let archive = async_tar::Archive::new(decompressed);
        self.extract_tar_archive(archive).await
    }

    async fn extract_tar_archive<R: AsyncRead + Unpin + Send>(
        &self,
        archive: async_tar::Archive<R>,
    ) -> Result<()> {
        use futures::StreamExt as _;

        let mut entries = archive.entries().context("reading tar entries")?;
        let mut deferred_dirs: Vec<(PathBuf, async_tar::Header)> = Vec::new();

        while let Some(entry) = entries.next().await {
            let mut entry = entry.context("reading tar entry")?;
            let entry_type = entry.header().entry_type();

            if entry_type.is_symlink() || entry_type.is_hard_link() {
                let path_display = entry
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<invalid>".into());
                log::info!(
                    "stripping {} entry from tar archive: {path_display}",
                    if entry_type.is_symlink() {
                        "symlink"
                    } else {
                        "hardlink"
                    }
                );
                continue;
            }

            if entry_type.is_dir() {
                let path = entry.path().context("reading dir entry path")?;
                deferred_dirs.push((PathBuf::from(path.as_ref()), entry.header().clone()));
                continue;
            }

            if entry_type.is_file() || entry_type == async_tar::EntryType::Regular {
                entry
                    .unpack_in(&self.path)
                    .await
                    .context("unpacking tar file entry")?;
            }
        }

        for (dir_path, _header) in &deferred_dirs {
            let full_path = self.path.join(dir_path);
            std::fs::create_dir_all(&full_path)
                .with_context(|| format!("creating deferred directory {full_path:?}"))?;
        }

        Ok(())
    }
}

/// Downloads a binary from `url`, optionally verifying its SHA-256 digest.
/// Returns a seekable file handle positioned at the start of the downloaded content.
///
/// Callers are responsible for extracting/processing the returned data
/// (e.g. via `ArchiveDir::extract_tar_gz`, `ArchiveDir::extract_seekable_zip`, or `extract_gz`).
pub async fn download_binary(
    http_client: &dyn HttpClient,
    url: &str,
    digest: Option<&str>,
) -> Result<async_fs::File> {
    log::info!("downloading github artifact from {url}");
    let mut response = http_client
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("downloading release from {url}"))?;
    let body = response.body_mut();

    let temp_file =
        tempfile::tempfile().with_context(|| format!("creating a temporary file for {url}"))?;

    match digest {
        Some(expected_sha_256) => {
            let mut writer = HashingWriter {
                writer: async_fs::File::from(temp_file),
                hasher: Sha256::new(),
            };
            futures::io::copy(&mut BufReader::new(body), &mut writer)
                .await
                .with_context(|| {
                    format!("saving archive contents into the temporary file for {url}")
                })?;
            let asset_sha_256 = format!("{:x}", writer.hasher.finalize());

            anyhow::ensure!(
                asset_sha_256 == expected_sha_256,
                "{url} asset got SHA-256 mismatch. Expected: {expected_sha_256}, Got: {asset_sha_256}",
            );
            writer
                .writer
                .seek(std::io::SeekFrom::Start(0))
                .await
                .with_context(|| format!("seeking temporary file for {url}"))?;
            Ok(writer.writer)
        }
        None => {
            let mut file = async_fs::File::from(temp_file);
            futures::io::copy(&mut BufReader::new(body), &mut file)
                .await
                .with_context(|| {
                    format!("saving archive contents into the temporary file for {url}")
                })?;
            file.seek(std::io::SeekFrom::Start(0))
                .await
                .with_context(|| format!("seeking temporary file for {url}"))?;
            Ok(file)
        }
    }
}

/// Decompresses a single gzip-compressed file (not a tar.gz archive) to `destination_path`.
pub async fn extract_gz(
    destination_path: &Path,
    url: &str,
    from: impl AsyncRead + Unpin,
) -> Result<(), anyhow::Error> {
    let mut decompressed_bytes = GzipDecoder::new(BufReader::new(from));
    let mut file = async_fs::File::create(&destination_path)
        .await
        .with_context(|| {
            format!("creating a file {destination_path:?} for a download from {url}")
        })?;
    futures::io::copy(&mut decompressed_bytes, &mut file)
        .await
        .with_context(|| format!("extracting {url} to {destination_path:?}"))?;
    Ok(())
}

struct HashingWriter<W: AsyncWrite + Unpin> {
    writer: W,
    hasher: Sha256,
}

impl<W: AsyncWrite + Unpin> AsyncWrite for HashingWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<std::result::Result<usize, std::io::Error>> {
        match Pin::new(&mut self.writer).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.hasher.update(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<std::result::Result<(), std::io::Error>> {
        Pin::new(&mut self.writer).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_archive_dir() -> (TempDir, ArchiveDir) {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let archive_dir = ArchiveDir::new(temp_dir.path());
        (temp_dir, archive_dir)
    }

    fn assert_file_content(path: &Path, expected: &str) {
        let content = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read {path:?}: {error}"));
        assert_eq!(content, expected);
    }

    fn make_test_data() -> TempDir {
        let dir = TempDir::new().expect("failed to create temp dir");
        std::fs::write(dir.path().join("file.txt"), "hello world")
            .expect("failed to write file.txt");
        std::fs::create_dir(dir.path().join("nested")).expect("failed to create nested dir");
        std::fs::write(dir.path().join("nested/inner.txt"), "nested content")
            .expect("failed to write inner.txt");
        dir
    }

    #[allow(clippy::disallowed_methods)]
    async fn create_zip_with_entries(entries: &[(&str, &[u8])]) -> futures::io::Cursor<Vec<u8>> {
        use async_zip::base::write::ZipFileWriter;
        use async_zip::{Compression, ZipEntryBuilder};

        let mut writer = ZipFileWriter::new(Vec::<u8>::new());
        for &(name, data) in entries {
            let entry = ZipEntryBuilder::new(String::from(name).into(), Compression::Stored);
            writer
                .write_entry_whole(entry, data)
                .await
                .expect("failed to write zip entry");
        }
        let bytes = writer.close().await.expect("failed to close zip writer");
        futures::io::Cursor::new(bytes)
    }

    #[cfg(not(windows))]
    #[allow(clippy::disallowed_methods)]
    async fn create_zip_with_permissions(
        entries: &[(&str, &[u8], u16)],
    ) -> futures::io::Cursor<Vec<u8>> {
        use async_zip::base::write::ZipFileWriter;
        use async_zip::{AttributeCompatibility, Compression, ZipEntryBuilder};

        let mut writer = ZipFileWriter::new(Vec::<u8>::new());
        for &(name, data, mode) in entries {
            let entry = ZipEntryBuilder::new(String::from(name).into(), Compression::Stored)
                .attribute_compatibility(AttributeCompatibility::Unix)
                .unix_permissions(mode);
            writer
                .write_entry_whole(entry, data)
                .await
                .expect("failed to write zip entry");
        }
        let bytes = writer.close().await.expect("failed to close zip writer");
        futures::io::Cursor::new(bytes)
    }

    fn set_header_path_raw(header: &mut async_tar::Header, path: &[u8]) {
        let bytes = header.as_mut_bytes();
        let len = path.len().min(100);
        bytes[..len].copy_from_slice(&path[..len]);
        for byte in &mut bytes[len..100] {
            *byte = 0;
        }
        header.set_cksum();
    }

    #[allow(clippy::disallowed_methods)]
    async fn compress_zip(src_dir: &Path) -> Vec<u8> {
        use async_zip::base::write::ZipFileWriter;
        use async_zip::{Compression, ZipEntryBuilder};
        use walkdir::WalkDir;

        let mut writer = ZipFileWriter::new(Vec::<u8>::new());
        for entry in WalkDir::new(src_dir).sort_by_file_name() {
            let entry = entry.expect("failed to walk directory");
            let path = entry.path();
            let relative = path.strip_prefix(src_dir).expect("failed to strip prefix");
            if relative.as_os_str().is_empty() {
                continue;
            }
            if path.is_dir() {
                let dir_name = format!("{}/", relative.display());
                let builder = ZipEntryBuilder::new(dir_name.into(), Compression::Stored);
                writer
                    .write_entry_whole(builder, &[])
                    .await
                    .expect("failed to write dir entry");
            } else {
                let builder = ZipEntryBuilder::new(
                    relative.display().to_string().into(),
                    Compression::Stored,
                );
                let data = std::fs::read(path).expect("failed to read file");
                writer
                    .write_entry_whole(builder, &data)
                    .await
                    .expect("failed to write file entry");
            }
        }
        writer.close().await.expect("failed to close zip writer")
    }

    // ── ArchiveDir path validation ───────────────────────────────────────

    #[test]
    fn test_has_normal_components() {
        // Accepted
        assert!(ArchiveDir::has_normal_components("file.txt"));
        assert!(ArchiveDir::has_normal_components("nested/file.txt"));
        assert!(ArchiveDir::has_normal_components("a/b/c/d.txt"));
        assert!(ArchiveDir::has_normal_components("./file.txt"));

        // Rejected
        assert!(!ArchiveDir::has_normal_components("../file.txt"));
        assert!(!ArchiveDir::has_normal_components("/absolute/path.txt"));
        assert!(!ArchiveDir::has_normal_components(
            "nested/../../outside.txt"
        ));
        assert!(!ArchiveDir::has_normal_components(".."));
        assert!(!ArchiveDir::has_normal_components("/"));
    }

    #[test]
    fn test_ensure_contains_accepts_normal_paths() {
        let (_temp, archive_dir) = create_archive_dir();
        let base = archive_dir.path();

        std::fs::create_dir(base.join("subdir")).expect("failed to create subdir");
        std::fs::write(base.join("subdir/file.txt"), "content").expect("failed to write file");

        archive_dir
            .ensure_contains(&base.join("subdir/file.txt"))
            .expect("should accept path inside directory");
        archive_dir
            .ensure_contains(&base.join("subdir"))
            .expect("should accept subdirectory");
    }

    #[test]
    fn test_ensure_contains_accepts_nonexistent_paths() {
        let (_temp, archive_dir) = create_archive_dir();
        let base = archive_dir.path();

        archive_dir
            .ensure_contains(&base.join("does_not_exist.txt"))
            .expect("should accept nonexistent path inside directory");
        archive_dir
            .ensure_contains(&base.join("deep/nested/path.txt"))
            .expect("should accept nonexistent deep path inside directory");
    }

    #[test]
    fn test_ensure_contains_rejects_path_outside_directory() {
        let (_temp, archive_dir) = create_archive_dir();
        let outside_path = PathBuf::from("/tmp/outside.txt");

        let result = archive_dir.ensure_contains(&outside_path);
        assert!(result.is_err(), "should reject path outside directory");
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_contains_rejects_external_symlink() {
        let (_temp, archive_dir) = create_archive_dir();
        let base = archive_dir.path();

        let outside = TempDir::new().expect("failed to create outside dir");
        std::fs::write(outside.path().join("target.txt"), "outside content")
            .expect("failed to write outside file");

        std::os::unix::fs::symlink(outside.path(), base.join("link"))
            .expect("failed to create symlink");

        let result = archive_dir.ensure_contains(&base.join("link/target.txt"));
        assert!(
            result.is_err(),
            "should reject path through external symlink"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_contains_allows_internal_symlink() {
        let (_temp, archive_dir) = create_archive_dir();
        let base = archive_dir.path();

        std::fs::create_dir(base.join("target_dir")).expect("failed to create target dir");
        std::fs::write(base.join("target_dir/file.txt"), "content").expect("failed to write file");

        std::os::unix::fs::symlink(base.join("target_dir"), base.join("link"))
            .expect("failed to create symlink");

        archive_dir
            .ensure_contains(&base.join("link/file.txt"))
            .expect("should accept path through internal symlink");
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_contains_rejects_chained_symlink() {
        let (_temp, archive_dir) = create_archive_dir();
        let base = archive_dir.path();

        let outside = TempDir::new().expect("failed to create outside dir");

        std::fs::create_dir(base.join("dir1")).expect("failed to create dir1");
        std::fs::create_dir(base.join("dir2")).expect("failed to create dir2");

        // dir1/link1 -> dir2 (internal, OK on its own)
        std::os::unix::fs::symlink(base.join("dir2"), base.join("dir1/link1"))
            .expect("failed to create link1");

        // dir2/link2 -> outside (escapes)
        std::os::unix::fs::symlink(outside.path(), base.join("dir2/link2"))
            .expect("failed to create link2");

        let result = archive_dir.ensure_contains(&base.join("dir1/link1/link2/file.txt"));
        assert!(
            result.is_err(),
            "should reject path through chained symlinks that escape"
        );
    }

    // ── ZIP functionality ────────────────────────────────────────────────

    #[test]
    fn test_extract_zip() {
        smol::block_on(async {
            let test_data = make_test_data();
            let zip_bytes = compress_zip(test_data.path()).await;

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_zip(futures::io::Cursor::new(zip_bytes))
                .await
                .expect("failed to extract zip");

            assert_file_content(&archive_dir.path().join("file.txt"), "hello world");
            assert_file_content(
                &archive_dir.path().join("nested/inner.txt"),
                "nested content",
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_zip_preserves_executable_permissions() {
        smol::block_on(async {
            let cursor = create_zip_with_permissions(&[
                ("script.sh", b"#!/bin/sh\necho hi", 0o755),
                ("readonly.txt", b"data", 0o444),
            ])
            .await;

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_zip(cursor)
                .await
                .expect("failed to extract zip");

            use std::os::unix::fs::PermissionsExt;
            let script_perms = std::fs::metadata(archive_dir.path().join("script.sh"))
                .expect("script.sh missing")
                .permissions()
                .mode();
            assert_eq!(
                script_perms & 0o777,
                0o755,
                "executable permission should be preserved"
            );

            let readonly_perms = std::fs::metadata(archive_dir.path().join("readonly.txt"))
                .expect("readonly.txt missing")
                .permissions()
                .mode();
            assert_eq!(
                readonly_perms & 0o777,
                0o444,
                "read-only permission should be preserved"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_zip_sets_default_permissions() {
        smol::block_on(async {
            // Entry with no unix permissions set (mode = 0o000 / not set)
            let cursor = create_zip_with_entries(&[("file.txt", b"content")]).await;

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_zip(cursor)
                .await
                .expect("failed to extract zip");

            // File should exist and be readable (permissions not explicitly set means
            // the file is created with the process umask default, not 0o000)
            let content = std::fs::read_to_string(archive_dir.path().join("file.txt"))
                .expect("file.txt should be readable");
            assert_eq!(content, "content");
        });
    }

    // ── ZIP path safety ──────────────────────────────────────────────────

    #[test]
    fn test_extract_zip_skips_traversal_entries_extracts_safe_entries() {
        smol::block_on(async {
            let cursor = create_zip_with_entries(&[
                ("safe.txt", b"safe content"),
                ("../outside.txt", b"traversal 1"),
                ("/absolute.txt", b"traversal 2"),
                ("nested/../../outside2.txt", b"traversal 3"),
                ("nested/safe_inner.txt", b"inner content"),
            ])
            .await;

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_zip(cursor)
                .await
                .expect("failed to extract zip");

            assert_file_content(&archive_dir.path().join("safe.txt"), "safe content");
            assert_file_content(
                &archive_dir.path().join("nested/safe_inner.txt"),
                "inner content",
            );

            assert!(
                !archive_dir.path().join("outside.txt").exists(),
                "traversal entry ../outside.txt should be skipped"
            );
            assert!(
                !archive_dir.path().join("absolute.txt").exists(),
                "absolute path entry should be skipped"
            );
            assert!(
                !archive_dir.path().join("outside2.txt").exists(),
                "nested traversal entry should be skipped"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_zip_rejects_write_through_external_symlink() {
        smol::block_on(async {
            let (_temp, archive_dir) = create_archive_dir();

            let outside = TempDir::new().expect("failed to create outside dir");
            std::os::unix::fs::symlink(outside.path(), archive_dir.path().join("link"))
                .expect("failed to create symlink");

            let cursor = create_zip_with_entries(&[("link/file.txt", b"should not escape")]).await;

            let result = archive_dir.extract_zip(cursor).await;
            assert!(
                result.is_err(),
                "should reject zip entry that writes through external symlink"
            );
            assert!(
                !outside.path().join("file.txt").exists(),
                "file should not be written outside archive dir"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_zip_allows_write_through_internal_symlink() {
        smol::block_on(async {
            let (_temp, archive_dir) = create_archive_dir();

            std::fs::create_dir(archive_dir.path().join("target_dir"))
                .expect("failed to create target_dir");
            std::os::unix::fs::symlink(
                archive_dir.path().join("target_dir"),
                archive_dir.path().join("link"),
            )
            .expect("failed to create symlink");

            let cursor = create_zip_with_entries(&[("link/file.txt", b"internal content")]).await;

            archive_dir
                .extract_zip(cursor)
                .await
                .expect("should allow zip entry through internal symlink");

            assert_file_content(
                &archive_dir.path().join("target_dir/file.txt"),
                "internal content",
            );
        });
    }

    // ── TAR functionality ────────────────────────────────────────────────

    #[test]
    fn test_extract_tar() {
        smol::block_on(async {
            let mut builder = async_tar::Builder::new(Vec::new());

            let data = b"file content";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "file.txt", data.as_slice())
                .await
                .expect("failed to append file");

            let data = b"nested content";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "nested/inner.txt", data.as_slice())
                .await
                .expect("failed to append nested file");

            let bytes = builder
                .into_inner()
                .await
                .expect("failed to finish tar builder");

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_tar(futures::io::Cursor::new(bytes))
                .await
                .expect("failed to extract tar");

            assert_file_content(&archive_dir.path().join("file.txt"), "file content");
            assert_file_content(
                &archive_dir.path().join("nested/inner.txt"),
                "nested content",
            );
        });
    }

    // ── TAR path safety ──────────────────────────────────────────────────

    #[test]
    fn test_extract_tar_rejects_path_traversal() {
        smol::block_on(async {
            let mut builder = async_tar::Builder::new(Vec::new());

            // A safe file that should still be extracted
            let safe_data = b"safe";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(safe_data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "safe.txt", safe_data.as_slice())
                .await
                .expect("failed to append safe file");

            // A traversal entry that should be rejected/skipped by unpack_in.
            // append_data rejects ".." paths, so we craft the header manually.
            let traversal_data = b"outside";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(traversal_data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            set_header_path_raw(&mut header, b"../outside.txt");
            builder
                .append(&header, traversal_data.as_slice())
                .await
                .expect("failed to append traversal entry");

            let bytes = builder
                .into_inner()
                .await
                .expect("failed to finish tar builder");

            let (_temp, archive_dir) = create_archive_dir();
            // extract_tar should succeed — traversal entries are silently skipped
            // by async_tar's unpack_in
            archive_dir
                .extract_tar(futures::io::Cursor::new(bytes))
                .await
                .expect("extract_tar should succeed even with traversal entries");

            assert_file_content(&archive_dir.path().join("safe.txt"), "safe");
            assert!(
                !archive_dir.path().join("outside.txt").exists(),
                "traversal entry should not be extracted inside archive dir"
            );
            assert!(
                !archive_dir
                    .path()
                    .parent()
                    .unwrap()
                    .join("outside.txt")
                    .exists(),
                "traversal entry should not be extracted outside archive dir"
            );
        });
    }

    #[test]
    fn test_extract_tar_strips_absolute_path_prefix() {
        smol::block_on(async {
            let mut builder = async_tar::Builder::new(Vec::new());

            // append_data rejects absolute paths, so we craft the header manually.
            let data = b"absolute content";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            set_header_path_raw(&mut header, b"/absolute/file.txt");
            builder
                .append(&header, data.as_slice())
                .await
                .expect("failed to append absolute path entry");

            let bytes = builder
                .into_inner()
                .await
                .expect("failed to finish tar builder");

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_tar(futures::io::Cursor::new(bytes))
                .await
                .expect("failed to extract tar");

            // async_tar's unpack_in strips the leading / so the file lands at
            // archive_dir/absolute/file.txt
            assert_file_content(
                &archive_dir.path().join("absolute/file.txt"),
                "absolute content",
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_tar_strips_symlinks() {
        smol::block_on(async {
            let mut builder = async_tar::Builder::new(Vec::new());

            // Symlink entry — should be stripped
            let mut header = async_tar::Header::new_gnu();
            header.set_entry_type(async_tar::EntryType::Symlink);
            header.set_size(0);
            header.set_link_name("/").expect("failed to set link name");
            header.set_mode(0o777);
            header.set_cksum();
            builder
                .append_data(&mut header, "link", &[] as &[u8])
                .await
                .expect("failed to append symlink");

            // Regular file that should still be extracted
            let data = b"safe file";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "regular.txt", data.as_slice())
                .await
                .expect("failed to append regular file");

            let bytes = builder
                .into_inner()
                .await
                .expect("failed to finish tar builder");

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_tar(futures::io::Cursor::new(bytes))
                .await
                .expect("failed to extract tar");

            // Symlink should not exist
            assert!(
                !archive_dir.path().join("link").exists()
                    || !archive_dir.path().join("link").is_symlink(),
                "symlink entry should be stripped"
            );
            // Regular file should be extracted
            assert_file_content(&archive_dir.path().join("regular.txt"), "safe file");
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_tar_strips_hardlinks() {
        smol::block_on(async {
            let mut builder = async_tar::Builder::new(Vec::new());

            // Regular file first
            let data = b"original";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "original.txt", data.as_slice())
                .await
                .expect("failed to append original file");

            // Hardlink entry — should be stripped
            let mut header = async_tar::Header::new_gnu();
            header.set_entry_type(async_tar::EntryType::Link);
            header.set_size(0);
            header
                .set_link_name("original.txt")
                .expect("failed to set link name");
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "hardlink.txt", &[] as &[u8])
                .await
                .expect("failed to append hardlink");

            let bytes = builder
                .into_inner()
                .await
                .expect("failed to finish tar builder");

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_tar(futures::io::Cursor::new(bytes))
                .await
                .expect("failed to extract tar");

            assert_file_content(&archive_dir.path().join("original.txt"), "original");
            assert!(
                !archive_dir.path().join("hardlink.txt").exists(),
                "hardlink entry should be stripped"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_tar_symlink_then_write_is_contained() {
        smol::block_on(async {
            let outside = TempDir::new().expect("failed to create outside dir");

            let mut builder = async_tar::Builder::new(Vec::new());

            // Symlink entry pointing outside — will be stripped by ArchiveDir
            let mut header = async_tar::Header::new_gnu();
            header.set_entry_type(async_tar::EntryType::Symlink);
            header.set_size(0);
            header
                .set_link_name(outside.path())
                .expect("failed to set link name");
            header.set_mode(0o777);
            header.set_cksum();
            builder
                .append_data(&mut header, "link", &[] as &[u8])
                .await
                .expect("failed to append symlink");

            // File that would write through the symlink if it existed
            let data = b"should stay inside";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "link/file.txt", data.as_slice())
                .await
                .expect("failed to append file through symlink");

            let bytes = builder
                .into_inner()
                .await
                .expect("failed to finish tar builder");

            let (_temp, archive_dir) = create_archive_dir();
            archive_dir
                .extract_tar(futures::io::Cursor::new(bytes))
                .await
                .expect("extract should succeed");

            // The symlink was stripped, so the file lands as a regular nested
            // path inside the archive dir
            assert!(
                !outside.path().join("file.txt").exists(),
                "file must not be written outside via symlink"
            );
            // "link" was not created as a symlink, but unpack_in creates it as
            // a directory for "link/file.txt"
            assert_file_content(
                &archive_dir.path().join("link/file.txt"),
                "should stay inside",
            );
        });
    }

    // ── TAR defense-in-depth (raw async_tar) ─────────────────────────────

    #[cfg(unix)]
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn test_raw_tar_rejects_symlink_then_write() {
        smol::block_on(async {
            let temp = TempDir::new().expect("failed to create temp dir");
            let dest = temp.path().join("dest");
            std::fs::create_dir(&dest).expect("failed to create dest dir");

            let outside = TempDir::new().expect("failed to create outside dir");

            let mut builder = async_tar::Builder::new(Vec::new());

            // Symlink entry: link -> outside directory
            let mut header = async_tar::Header::new_gnu();
            header.set_entry_type(async_tar::EntryType::Symlink);
            header.set_size(0);
            header
                .set_link_name(outside.path())
                .expect("failed to set link name");
            header.set_mode(0o777);
            header.set_cksum();
            builder
                .append_data(&mut header, "link", &[] as &[u8])
                .await
                .expect("failed to append symlink");

            // File that writes through the symlink
            let data = b"should not escape";
            let mut header = async_tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_entry_type(async_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "link/file.txt", data.as_slice())
                .await
                .expect("failed to append file through symlink");

            let bytes = builder
                .into_inner()
                .await
                .expect("failed to finish tar builder");

            // Use raw Archive::unpack (NOT ArchiveDir) to verify async_tar's
            // own validate_inside_dst catches the two-stage symlink vector.
            let archive = async_tar::Archive::new(futures::io::Cursor::new(bytes));
            let result = archive.unpack(&dest).await;

            assert!(
                result.is_err(),
                "raw Archive::unpack should reject symlink-then-write that escapes destination"
            );
            assert!(
                !outside.path().join("file.txt").exists(),
                "file must not be written outside via symlink"
            );
        });
    }
}
