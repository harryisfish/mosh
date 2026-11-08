use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::image_paste::{ImageBlob, MAX_BATCH_BYTES, MAX_IMAGE_BYTES, MAX_IMAGES};
use crate::model::{InputEvent, Key, KeyCode, KeyPhase};

pub enum PreparedInput {
    Plain(Vec<InputEvent>),
    Images {
        events: Vec<InputEvent>,
        index: usize,
        images: Vec<ImageBlob>,
    },
}

pub fn is_paste_key(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Key(Key {
        code: KeyCode::Char('v' | 'V'),
        modifiers,
        phase: KeyPhase::Press,
        ..
    }) if modifiers & 4 != 0 && modifiers & !(4 | 64 | 128) == 0)
}

pub fn candidate(events: &[InputEvent]) -> bool {
    events.iter().any(event_candidate)
}

pub async fn prepare(events: Vec<InputEvent>) -> Result<PreparedInput> {
    let indices: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| event_candidate(event).then_some(index))
        .collect();
    ensure!(indices.len() <= 1, "paste one image batch at a time");
    let Some(&index) = indices.first() else {
        return Ok(PreparedInput::Plain(events));
    };
    let images = match &events[index] {
        InputEvent::Paste(text) => {
            Some(load_images(image_paths(text).context("invalid image paths")?).await?)
        }
        _ => clipboard_images().await?,
    };
    if let Some(images) = images {
        Ok(PreparedInput::Images {
            events,
            index,
            images,
        })
    } else {
        Ok(PreparedInput::Plain(events))
    }
}

fn event_candidate(event: &InputEvent) -> bool {
    is_paste_key(event) || matches!(event, InputEvent::Paste(text) if image_paths(text).is_some())
}

fn image_paths(text: &str) -> Option<Vec<String>> {
    if let Some(paths) = shell_words(text)
        && !paths.is_empty()
        && paths.iter().all(|path| image_path(path))
    {
        return Some(paths);
    }
    let path = text.trim();
    if path
        .chars()
        .any(|ch| ch.is_control() || ";&|<>`$()\\".contains(ch))
    {
        return None;
    }
    image_path(path).then(|| vec![path.to_owned()])
}

fn image_path(path: &str) -> bool {
    let path = if path.starts_with("file://") {
        let Some(path) = file_uri(path) else {
            return false;
        };
        path
    } else {
        path.to_owned()
    };
    (path.starts_with('/') || path.starts_with("~/"))
        && path.rsplit('.').next().is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp"
            )
        })
}

fn shell_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = text.chars().peekable();
    let mut quote = None;
    let mut started = false;
    while let Some(ch) = chars.next() {
        if ch.is_control() && !ch.is_whitespace() {
            return None;
        }
        match (quote, ch) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('\''), _) => word.push(ch),
            (Some('"'), '\\') => {
                let next = *chars.peek()?;
                if matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                    chars.next();
                    if next != '\n' {
                        word.push(next);
                    }
                } else {
                    word.push(ch);
                }
            }
            (Some('"'), _) => word.push(ch),
            (None, '\'' | '"') => {
                quote = Some(ch);
                started = true;
            }
            (None, '\\') => {
                let next = chars.next()?;
                if next != '\n' {
                    word.push(next);
                    started = true;
                }
            }
            (None, ch) if ch.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, ';' | '&' | '|' | '<' | '>' | '`' | '$' | '(' | ')') => return None,
            (None, _) => {
                word.push(ch);
                started = true;
            }
            _ => unreachable!(),
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

fn file_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let path = if rest.starts_with('/') {
        rest
    } else {
        rest.strip_prefix("localhost")
            .filter(|s| s.starts_with('/'))?
    };
    if path.contains(['?', '#']) {
        return None;
    }
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        });
    }
    let path = String::from_utf8(decoded).ok()?;
    (!path.contains('\0')).then_some(path)
}

async fn load_images(paths: Vec<String>) -> Result<Vec<ImageBlob>> {
    ensure!(paths.len() <= MAX_IMAGES, "too many pasted images");
    tokio::task::spawn_blocking(move || {
        let mut images = Vec::with_capacity(paths.len());
        let mut total = 0;
        for path in paths {
            let path = if path.starts_with("file://") {
                PathBuf::from(file_uri(&path).context("invalid pasted image URI")?)
            } else if let Some(relative) = path.strip_prefix("~/") {
                PathBuf::from(std::env::var_os("HOME").context("home directory is unavailable")?)
                    .join(relative)
            } else {
                PathBuf::from(path)
            };
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
                .context("cannot open pasted image")?;
            let before = file.metadata()?;
            ensure!(before.is_file(), "pasted image is not a regular file");
            ensure!(
                before.len() <= MAX_IMAGE_BYTES as u64,
                "pasted image exceeds the size limit"
            );
            ensure!(
                total + before.len() as usize <= MAX_BATCH_BYTES,
                "pasted images exceed the batch size limit"
            );
            let mut bytes = Vec::with_capacity(before.len() as usize);
            (&mut file)
                .take(MAX_IMAGE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            let after = file.metadata()?;
            ensure!(
                bytes.len() <= MAX_IMAGE_BYTES
                    && bytes.len() as u64 == before.len()
                    && unchanged(&before, &after),
                "pasted image changed while being read"
            );
            let image = ImageBlob::new(bytes)?;
            total += image.bytes().len();
            images.push(image);
        }
        Ok(images)
    })
    .await
    .context("image reader stopped")?
}

fn unchanged(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

struct HelperOutput {
    status: ExitStatus,
    bytes: Vec<u8>,
}

async fn helper(program: &str, args: &[&str], limit: usize) -> Result<Option<HelperOutput>> {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("start clipboard reader"),
    };
    let stdout = child
        .stdout
        .take()
        .context("clipboard reader has no output")?;
    let operation = async {
        let mut bytes = Vec::new();
        stdout
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await?;
        ensure!(
            bytes.len() <= limit,
            "clipboard data exceeds the size limit"
        );
        let status = child.wait().await?;
        Ok(Some(HelperOutput { status, bytes }))
    };
    tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .context("clipboard reader timed out")?
}

#[cfg(target_os = "macos")]
async fn clipboard_images() -> Result<Option<Vec<ImageBlob>>> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "lowercase")]
    enum Reply {
        Empty,
        Files { paths: Vec<String> },
        Image { data: String },
        Error,
    }

    const SCRIPT: &str = r#"
ObjC.import('AppKit');
(function () {
    try {
        var board = $.NSPasteboard.generalPasteboard;
        var items = board.pasteboardItems;
        var paths = [];
        for (var i = 0; i < Number(items.count); i++) {
            var value = items.objectAtIndex(i).stringForType('public.file-url');
            if (Number(value.length) > 0) {
                var url = $.NSURL.URLWithString(value);
                if (url.isFileURL) paths.push(ObjC.unwrap(url.path));
            }
        }
        if (paths.length) return JSON.stringify({kind:'files', paths:paths});
        var data = board.dataForType('public.png');
        if (!(Number(data.length) > 0)) {
            var tiff = board.dataForType('public.tiff');
            if (!(Number(tiff.length) > 0)) return JSON.stringify({kind:'empty'});
            if (Number(tiff.length) > 33554432) return JSON.stringify({kind:'error'});
            data = $.NSBitmapImageRep.imageRepWithData(tiff)
                .representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $.NSDictionary.dictionary);
        }
        if (!(Number(data.length) > 0) || Number(data.length) > 16777216)
            return JSON.stringify({kind:'error'});
        return JSON.stringify({kind:'image', data:ObjC.unwrap(data.base64EncodedStringWithOptions(0))});
    } catch (error) {
        return JSON.stringify({kind:'error'});
    }
})()
"#;
    let Some(output) = helper(
        "/usr/bin/osascript",
        &["-l", "JavaScript", "-e", SCRIPT],
        MAX_IMAGE_BYTES.div_ceil(3) * 4 + 128 * 1024,
    )
    .await?
    else {
        return Ok(None);
    };
    ensure!(output.status.success(), "cannot read the system clipboard");
    match serde_json::from_slice::<Reply>(&output.bytes)
        .context("invalid clipboard reader output")?
    {
        Reply::Empty => Ok(None),
        Reply::Files { paths } => {
            if paths.is_empty() || !paths.iter().all(|path| image_path(path)) {
                return Ok(None);
            }
            Ok(Some(load_images(paths).await?))
        }
        Reply::Image { data } => Ok(Some(vec![ImageBlob::new(
            STANDARD
                .decode(data)
                .context("invalid clipboard image data")?,
        )?])),
        Reply::Error => {
            anyhow::bail!("cannot read the clipboard image or it exceeds the size limit")
        }
    }
}

#[cfg(target_os = "linux")]
async fn clipboard_images() -> Result<Option<Vec<ImageBlob>>> {
    let reader = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        if let Some(output) = helper("wl-paste", &["--list-types"], 16 * 1024).await?
            && output.status.success()
        {
            Some(("wl-paste", output.bytes))
        } else {
            None
        }
    } else {
        None
    };
    let reader = if reader.is_none() && std::env::var_os("DISPLAY").is_some() {
        helper(
            "xclip",
            &["-selection", "clipboard", "-out", "-target", "TARGETS"],
            16 * 1024,
        )
        .await?
        .filter(|output| output.status.success())
        .map(|output| ("xclip", output.bytes))
    } else {
        reader
    };
    let Some((program, types)) = reader else {
        return Ok(None);
    };
    let types = String::from_utf8(types).context("invalid clipboard types")?;
    let target = [
        "text/uri-list",
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/gif",
    ]
    .into_iter()
    .find(|target| types.lines().any(|line| line.trim() == *target));
    let Some(target) = target else {
        return Ok(None);
    };
    let args = if program == "wl-paste" {
        vec!["--no-newline", "--type", target]
    } else {
        vec!["-selection", "clipboard", "-out", "-target", target]
    };
    let output = helper(program, &args, MAX_IMAGE_BYTES)
        .await?
        .context("clipboard reader became unavailable")?;
    ensure!(output.status.success(), "cannot read the clipboard image");
    if target == "text/uri-list" {
        let uris = String::from_utf8(output.bytes).context("invalid clipboard file list")?;
        let paths: Vec<_> = uris
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect();
        if paths.is_empty() || !paths.iter().all(|path| image_path(path)) {
            return Ok(None);
        }
        Ok(Some(load_images(paths).await?))
    } else {
        Ok(Some(vec![ImageBlob::new(output.bytes)?]))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
async fn clipboard_images() -> Result<Option<Vec<ImageBlob>>> {
    Ok(None)
}
