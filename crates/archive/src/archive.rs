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
mod tests {}
