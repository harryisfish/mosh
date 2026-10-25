use anyhow::{ensure, Result};
use std::convert::TryInto;
use wezterm_escape_parser::osc::{ITermProprietary, OperatingSystemCommand};
use wezterm_escape_parser::{Action, DeviceControlMode, Sixel, SixelData};

pub const MAX_IMAGE: usize = 16 * 1024 * 1024;
pub const MAX_IMAGES: usize = 64 * 1024 * 1024;
pub const MAX_STRING: usize = 24 * 1024 * 1024;

#[derive(Default)]
pub struct ParserBudget {
    pending: usize,
    escaped: bool,
    string: Option<u8>,
    string_bytes: usize,
    string_escaped: bool,
}
impl ParserBudget {
    pub fn push(&mut self, byte: u8) -> Result<()> {
        self.pending += 1;
        ensure!(
            self.pending <= MAX_STRING,
            "unterminated terminal action exceeds limit"
        );
        if let Some(kind) = self.string {
            self.string_bytes += 1;
            let limit = if kind == b'P' { 256 * 1024 } else { MAX_STRING };
            ensure!(
                self.string_bytes <= limit,
                "terminal control string exceeds limit"
            );
            if (self.string_escaped && byte == b'\\')
                || (kind == b']' && byte == 7)
                || byte == 0x18
                || byte == 0x1a
            {
                self.string = None;
                self.string_bytes = 0;
            }
            self.string_escaped = byte == 0x1b;
        } else if self.escaped {
            if matches!(byte, b']' | b'P' | b'_' | b'^' | b'X') {
                self.string = Some(byte);
                self.string_bytes = 0;
                self.string_escaped = false;
            }
            self.escaped = byte == 0x1b;
        } else {
            self.escaped = byte == 0x1b;
        }
        Ok(())
    }
    pub fn action(&mut self, action: &Action) {
        if matches!(action, Action::DeviceControl(DeviceControlMode::Enter(_)))
            && self.string.is_none()
        {
            self.string = Some(b'P');
            self.string_bytes = 0;
        }
        if matches!(
            action,
            Action::Sixel(_)
                | Action::OperatingSystemCommand(_)
                | Action::KittyImage(_)
                | Action::DeviceControl(DeviceControlMode::Exit)
        ) {
            self.string = None;
        }
        if !matches!(
            action,
            Action::DeviceControl(DeviceControlMode::Enter(_) | DeviceControlMode::Data(_))
        ) {
            self.pending = 0;
        }
    }
}

pub fn sixel_dimensions(sixel: &Sixel) -> Result<(u32, u32)> {
    let mut x = 0u64;
    let mut y = 0u64;
    let mut width = u64::from(sixel.pixel_width.unwrap_or(0));
    let mut height = u64::from(sixel.pixel_height.unwrap_or(0));
    for data in &sixel.data {
        match data {
            SixelData::Data(_) => x += 1,
            SixelData::Repeat { repeat_count, .. } => x += u64::from(*repeat_count),
            SixelData::CarriageReturn => x = 0,
            SixelData::NewLine => {
                x = 0;
                y += 6;
            }
            _ => (),
        }
        width = width.max(x);
        height = height.max(y + 6);
        ensure!(
            width <= 16384
                && height <= 16384
                && width.saturating_mul(height).saturating_mul(4) <= MAX_IMAGE as u64,
            "SIXEL dimensions exceed image limit"
        );
    }
    ensure!(width > 0 && height > 0, "empty SIXEL");
    if let (Some(width), Some(height)) = (sixel.pixel_width, sixel.pixel_height) {
        crate::terminalstate::check_image_dimensions(width, height)?;
        Ok((width, height))
    } else {
        Ok((width as u32, height as u32))
    }
}

pub fn validate_action(action: &Action) -> Result<()> {
    match action {
        Action::Sixel(sixel) => {
            sixel_dimensions(sixel)?;
        }
        Action::OperatingSystemCommand(osc) => match osc.as_ref() {
            OperatingSystemCommand::ITermProprietary(ITermProprietary::File(file)) => {
                ensure!(
                    file.inline,
                    "file downloads are outside the terminal profile"
                );
                ensure!(
                    file.data.len() <= MAX_IMAGE,
                    "inline image data exceeds limit"
                );
            }
            _ => (),
        },
        _ => (),
    }
    Ok(())
}

/// Only static PNG and JPEG are admitted. Inspect PNG chunks before decoding so
/// APNG never allocates frame state. GIF/WebP/AVIF cannot enter an animation path.
fn static_reader(data: &[u8]) -> Result<image::ImageReader<std::io::Cursor<&[u8]>>> {
    ensure!(data.len() <= MAX_IMAGE, "encoded image exceeds limit");
    let format = image::guess_format(data)?;
    ensure!(
        matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg),
        "only static PNG/JPEG inline images are supported"
    );
    if format == image::ImageFormat::Png {
        let mut offset = 8usize;
        while offset + 12 <= data.len() {
            let len = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            ensure!(
                &data[offset + 4..offset + 8] != b"acTL",
                "animated PNG is outside the terminal profile"
            );
            offset = offset
                .checked_add(len)
                .and_then(|n| n.checked_add(12))
                .ok_or_else(|| anyhow::anyhow!("PNG chunk overflow"))?;
            ensure!(offset <= data.len(), "truncated PNG chunk");
        }
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(data), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(MAX_IMAGE as u64 * 2);
    reader.limits(limits);
    Ok(reader)
}

pub fn static_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    let dimensions = static_reader(data)?.into_dimensions()?;
    crate::terminalstate::check_image_dimensions(dimensions.0, dimensions.1)?;
    Ok(dimensions)
}

pub fn decode_static(data: &[u8]) -> Result<image::RgbaImage> {
    static_dimensions(data)?;
    Ok(static_reader(data)?.decode()?.into_rgba8())
}
