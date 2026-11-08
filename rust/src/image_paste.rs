use std::collections::BTreeSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use quinn::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};

use crate::model::Digest;

pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BATCH_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_IMAGES: usize = 8;
const MAX_STORED_BYTES: usize = 128 * 1024 * 1024;
const MAX_STORED_IMAGES: usize = 64;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ERROR_BYTES: usize = 256;
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const MAGIC: &[u8; 5] = b"MIMG\x01";

#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    fn detect(bytes: &[u8]) -> Result<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Ok(Self::Png)
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Ok(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Ok(Self::Gif)
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Ok(Self::Webp)
        } else {
            anyhow::bail!("image must be PNG, JPEG, GIF or WebP")
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }
}

#[derive(Clone)]
pub struct ImageBlob {
    format: ImageFormat,
    digest: Digest,
    bytes: Arc<[u8]>,
}

impl ImageBlob {
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_IMAGE_BYTES,
            "image exceeds the 16 MiB limit"
        );
        Ok(Self {
            format: ImageFormat::detect(&bytes)?,
            digest: *blake3::hash(&bytes).as_bytes(),
            bytes: bytes.into(),
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum UploadReply {
    Ready(Vec<String>),
    Rejected(String),
}

pub async fn upload(
    connection: &Connection,
    epoch: u64,
    images: &[ImageBlob],
) -> Result<UploadReply> {
    if images.is_empty()
        || images.len() > MAX_IMAGES
        || images.iter().map(|image| image.bytes.len()).sum::<usize>() > MAX_BATCH_BYTES
    {
        return Ok(UploadReply::Rejected(
            "paste at most 8 images totaling 32 MiB".into(),
        ));
    }
    tokio::time::timeout(UPLOAD_TIMEOUT, async {
        let (mut output, mut input) = connection.open_bi().await?;
        output.set_priority(-10)?;
        let sending = async {
            output.write_all(MAGIC).await?;
            output.write_all(&epoch.to_be_bytes()).await?;
            output.write_all(&[images.len() as u8]).await?;
            for image in images {
                output
                    .write_all(&(image.bytes.len() as u32).to_be_bytes())
                    .await?;
                output.write_all(&image.digest).await?;
                output.write_all(&image.bytes).await?;
            }
            output.finish()?;
            Result::<()>::Ok(())
        };
        let receiving = async {
            let mut kind = [0];
            input.read_exact(&mut kind).await?;
            let reply = match kind[0] {
                0 => {
                    let mut count = [0];
                    input.read_exact(&mut count).await?;
                    ensure!(
                        count[0] as usize == images.len(),
                        "invalid image upload response count"
                    );
                    let mut paths = Vec::with_capacity(images.len());
                    for _ in 0..count[0] {
                        let path = read_text(&mut input, MAX_PATH_BYTES).await?;
                        ensure!(
                            path.starts_with('/') && !path.chars().any(char::is_control),
                            "invalid uploaded image path"
                        );
                        paths.push(path);
                    }
                    UploadReply::Ready(paths)
                }
                1 => UploadReply::Rejected(read_text(&mut input, MAX_ERROR_BYTES).await?),
                _ => anyhow::bail!("invalid image upload response"),
            };
            ensure_eof(&mut input).await?;
            Result::<UploadReply>::Ok(reply)
        };
        let (sent, received) = tokio::join!(sending, receiving);
        match received {
            Ok(reply @ UploadReply::Rejected(_)) => Ok(reply),
            result => {
                sent?;
                result
            }
        }
    })
    .await
    .context("image upload timed out")?
}

async fn read_text(input: &mut RecvStream, limit: usize) -> Result<String> {
    let mut length = [0; 2];
    input.read_exact(&mut length).await?;
    let length = u16::from_be_bytes(length) as usize;
    ensure!(
        length > 0 && length <= limit,
        "image response exceeds limit"
    );
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).await?;
    String::from_utf8(bytes).context("invalid image response text")
}

async fn write_text(output: &mut SendStream, text: &str) -> Result<()> {
    output.write_all(&(text.len() as u16).to_be_bytes()).await?;
    output.write_all(text.as_bytes()).await?;
    Ok(())
}

async fn ensure_eof(input: &mut RecvStream) -> Result<()> {
    ensure!(
        input.read(&mut [0]).await?.is_none(),
        "trailing image upload bytes"
    );
    Ok(())
}

pub(crate) async fn send_reply(output: &mut SendStream, reply: &UploadReply) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        match reply {
            UploadReply::Ready(paths) => {
                ensure!(
                    !paths.is_empty()
                        && paths.len() <= MAX_IMAGES
                        && paths.iter().all(|path| path.len() <= MAX_PATH_BYTES),
                    "invalid uploaded image paths"
                );
                output.write_all(&[0, paths.len() as u8]).await?;
                for path in paths {
                    write_text(output, path).await?;
                }
            }
            UploadReply::Rejected(reason) => {
                let reason: String = reason
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(MAX_ERROR_BYTES / 4)
                    .collect();
                output.write_all(&[1]).await?;
                write_text(output, &reason).await?;
            }
        }
        output.finish()?;
        Result::<()>::Ok(())
    })
    .await
    .context("image response timed out")?
}

pub(crate) struct ImageDirectory {
    path: PathBuf,
    sequence: AtomicU64,
}

impl Drop for ImageDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct StagedImage {
    path: PathBuf,
    blob: ImageBlob,
}

impl Drop for StagedImage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) struct StagedBatch {
    images: Vec<StagedImage>,
    directory: Arc<ImageDirectory>,
}

#[derive(Default)]
pub(crate) struct ImageStore {
    directory: Option<Arc<ImageDirectory>>,
    files: BTreeSet<Digest>,
    bytes: usize,
}

impl ImageStore {
    pub(crate) fn directory(&mut self, session: &str) -> Result<Arc<ImageDirectory>> {
        if let Some(directory) = &self.directory {
            return Ok(directory.clone());
        }
        let entropy = format!(
            "{session}:{}:{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        let digest = blake3::hash(entropy.as_bytes()).to_hex();
        let path = std::env::temp_dir().join(format!("mosh-rs-images-{digest}"));
        ensure!(
            path.is_absolute()
                && path
                    .to_str()
                    .is_some_and(|path| path.len() + 70 <= MAX_PATH_BYTES
                        && !path.chars().any(char::is_control)),
            "image directory is not a usable absolute path"
        );
        DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .context("create private image directory")?;
        let directory = Arc::new(ImageDirectory {
            path,
            sequence: AtomicU64::new(0),
        });
        self.directory = Some(directory.clone());
        Ok(directory)
    }

    pub(crate) fn publish(&mut self, batch: StagedBatch) -> Result<Vec<String>> {
        ensure!(
            self.directory
                .as_ref()
                .is_some_and(|directory| Arc::ptr_eq(directory, &batch.directory)),
            "obsolete image directory"
        );
        let mut new = BTreeSet::new();
        let additional = batch
            .images
            .iter()
            .filter(|image| !self.files.contains(&image.blob.digest))
            .filter(|image| new.insert(image.blob.digest))
            .map(|image| image.blob.bytes.len())
            .sum::<usize>();
        ensure!(
            self.bytes + additional <= MAX_STORED_BYTES
                && self.files.len() + new.len() <= MAX_STORED_IMAGES,
            "session image limit reached (128 MiB or 64 files)"
        );
        let mut paths = Vec::with_capacity(batch.images.len());
        for image in &batch.images {
            let name = format!(
                "{}.{}",
                blake3::Hash::from(image.blob.digest).to_hex(),
                image.blob.format.extension()
            );
            let path = batch.directory.path.join(name);
            fs::rename(&image.path, &path).context("publish uploaded image")?;
            if self.files.insert(image.blob.digest) {
                self.bytes += image.blob.bytes.len();
            }
            paths.push(path.to_str().context("invalid image path")?.to_owned());
        }
        Ok(paths)
    }
}

pub(crate) async fn receive(
    input: &mut RecvStream,
    epoch: u64,
    directory: Arc<ImageDirectory>,
) -> Result<StagedBatch> {
    tokio::time::timeout(UPLOAD_TIMEOUT, async {
        let mut header = [0; 14];
        input.read_exact(&mut header).await?;
        ensure!(
            &header[..5] == MAGIC && u64::from_be_bytes(header[5..13].try_into()?) == epoch,
            "invalid image upload attachment"
        );
        let count = header[13] as usize;
        ensure!(count > 0 && count <= MAX_IMAGES, "too many images in paste");
        let mut images = Vec::with_capacity(count);
        let mut total = 0;
        for _ in 0..count {
            let mut metadata = [0; 36];
            input.read_exact(&mut metadata).await?;
            let length = u32::from_be_bytes(metadata[..4].try_into()?) as usize;
            ensure!(
                length > 0 && length <= MAX_IMAGE_BYTES,
                "image exceeds the 16 MiB limit"
            );
            total += length;
            ensure!(total <= MAX_BATCH_BYTES, "image paste exceeds 32 MiB");
            let mut bytes = vec![0; length];
            input.read_exact(&mut bytes).await?;
            let blob = ImageBlob::new(bytes)?;
            ensure!(blob.digest == metadata[4..], "image upload digest mismatch");
            images.push(blob);
        }
        ensure_eof(input).await?;
        tokio::task::spawn_blocking(move || stage(directory, images))
            .await
            .context("image staging worker stopped")?
    })
    .await
    .context("image upload timed out")?
}

fn stage(directory: Arc<ImageDirectory>, images: Vec<ImageBlob>) -> Result<StagedBatch> {
    let mut batch = StagedBatch {
        images: Vec::with_capacity(images.len()),
        directory,
    };
    for blob in images {
        let sequence = batch.directory.sequence.fetch_add(1, Ordering::Relaxed);
        let path = batch.directory.path.join(format!(".upload-{sequence}"));
        let file: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .context("create staged image")?;
        let image = StagedImage { path, blob };
        let mut file = file;
        file.write_all(image.blob.bytes())
            .context("write staged image")?;
        batch.images.push(image);
    }
    Ok(batch)
}
