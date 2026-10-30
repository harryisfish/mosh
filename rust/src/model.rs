use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 3;
pub const MAX_CONTROL: usize = 512 * 1024;
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
pub const MAX_CELL_IMAGES: usize = 4096;
pub const MAX_IMAGE_TILES: usize = 262144;
pub const MAX_RESOURCE: usize = 16 * 1024 * 1024;
pub const MAX_RESOURCES: usize = 64 * 1024 * 1024;
pub const MAX_PENDING_INPUT: usize = 256 * 1024;
pub const MAX_EFFECT_BYTES: usize = 1024 * 1024;
pub const MAX_EFFECTS: usize = 256;
pub type Digest = [u8; 32];

fn bounded_vec<'de, D, T, const MAX: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedVec<T, const MAX: usize>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>, const MAX: usize> serde::de::Visitor<'de> for BoundedVec<T, MAX> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX} entries")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Vec<T>, A::Error> {
            let capacity = sequence.size_hint().unwrap_or(0);
            if capacity > MAX {
                return Err(serde::de::Error::invalid_length(capacity, &self));
            }
            let mut values = Vec::with_capacity(capacity);
            while let Some(value) = sequence.next_element()? {
                if values.len() == MAX {
                    return Err(serde::de::Error::invalid_length(MAX + 1, &self));
                }
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_seq(BoundedVec::<T, MAX>(PhantomData))
}

pub fn resource_digest(width: u32, height: u32, rgba: &[u8]) -> Digest {
    let mut hash = blake3::Hasher::new();
    hash.update(&width.to_le_bytes());
    hash.update(&height.to_le_bytes());
    hash.update(rgba);
    *hash.finalize().as_bytes()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl Size {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=512).contains(&self.cols),
            "terminal columns must be 1..=512"
        );
        ensure!(
            (1..=256).contains(&self.rows),
            "terminal rows must be 1..=256"
        );
        ensure!(
            self.pixel_width <= 16384 && self.pixel_height <= 16384,
            "terminal pixel dimensions exceed limit"
        );
        Ok(())
    }
}

impl Default for Size {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            pixel_width: 800,
            pixel_height: 480,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Named(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyPhase {
    Press,
    Repeat,
    Release,
}

// Modifier bits follow kitty's order: shift, alt, ctrl, super, hyper, meta, caps, num.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub code: KeyCode,
    pub modifiers: u16,
    pub phase: KeyPhase,
    pub shifted: Option<char>,
    pub base: Option<char>,
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseKind {
    Press(u8),
    Release(u8),
    Move,
    Wheel(i16),
    WheelHorizontal(i16),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mouse {
    pub x: u16,
    pub y: u16,
    pub pixel_x: u16,
    pub pixel_y: u16,
    pub modifiers: u16,
    pub kind: MouseKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputEvent {
    Key(Key),
    Paste(String),
    Mouse(Mouse),
    Focus(bool),
    QueryResult { query: u64, data: Option<String> },
}

impl InputEvent {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Key(k) => {
                ensure!(k.modifiers < 256, "unknown keyboard modifier");
                if let KeyCode::Named(name) = &k.code {
                    ensure!(name.len() <= 32, "key name too long");
                }
                ensure!(
                    k.text.as_ref().is_none_or(|s| s.len() <= 4096),
                    "key text too long"
                );
            }
            Self::Paste(text) => ensure!(
                text.len() <= MAX_PENDING_INPUT / 2,
                "paste exceeds input limit"
            ),
            Self::Mouse(mouse) => ensure!(
                mouse.modifiers < 256 && mouse.x < 512 && mouse.y < 256,
                "invalid mouse event"
            ),
            Self::QueryResult { data, .. } => ensure!(
                data.as_ref()
                    .is_none_or(|s| s.len() <= MAX_PENDING_INPUT / 2),
                "clipboard reply too large"
            ),
            Self::Focus(_) => (),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRecord {
    pub seq: u64,
    pub event: InputEvent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    Rgb([u8; 3]),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hyperlink {
    pub params: String,
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
    pub underline: u8,
    pub underline_color: Color,
    pub hyperlink: Option<Hyperlink>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageTile {
    pub resource: Digest,
    pub source: [u32; 4],
    pub padding: [u16; 4],
    pub z: i32,
    pub placement: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub x: u16,
    pub text: String,
    pub width: u8,
    pub style: Style,
    pub images: Vec<ImageTile>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub stable_id: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub shape: u8,
    pub blink: bool,
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellMarker {
    pub row: i64,
    pub column: u16,
    pub kind: String,
    pub status: Option<i32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredictionContext {
    pub generation: u64,
    pub eligible: bool,
    pub echo_enabled: bool,
    pub alternate_screen: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceMeta {
    pub digest: Digest,
    pub width: u32,
    pub height: u32,
    pub bytes: u32,
}

#[derive(Clone, Debug)]
pub struct Resource {
    pub meta: ResourceMeta,
    pub rgba: Arc<[u8]>,
}

impl Resource {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=16384).contains(&self.meta.width) && (1..=16384).contains(&self.meta.height),
            "image dimensions exceed limit"
        );
        let len = u64::from(self.meta.width) * u64::from(self.meta.height) * 4;
        ensure!(
            self.meta.width > 0 && self.meta.height > 0 && len <= MAX_RESOURCE as u64,
            "image dimensions exceed limit"
        );
        ensure!(
            len == self.rgba.len() as u64 && self.meta.bytes as u64 == len,
            "image length mismatch"
        );
        ensure!(
            resource_digest(self.meta.width, self.meta.height, &self.rgba) == self.meta.digest,
            "image digest mismatch"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Screen {
    pub size: Size,
    pub rows: Vec<Row>,
    pub cursor: Cursor,
    pub title: String,
    pub directory: Option<String>,
    pub markers: Vec<ShellMarker>,
    pub reverse_video: bool,
    pub prediction: PredictionContext,
    pub resources: BTreeMap<Digest, Resource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPatch {
    pub y: u16,
    pub row: Row,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    pub epoch: u64,
    pub revision: u64,
    pub base: Option<u64>,
    pub echo_through: u64,
    pub size: Size,
    pub rows: Vec<RowPatch>,
    pub cursor: Cursor,
    pub title: String,
    pub directory: Option<String>,
    pub markers: Vec<ShellMarker>,
    pub reverse_video: bool,
    pub prediction: PredictionContext,
    pub resources: Vec<ResourceMeta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    Bell,
    ClipboardWrite { selection: String, data: String },
    ClipboardRead { query: u64, selection: String },
    Notification { title: String, body: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effect {
    pub id: u64,
    pub after: u64,
    pub kind: EffectKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attach {
    pub version: u16,
    pub session: String,
    pub attempt: u64,
    pub committed: u64,
    pub rendered: u64,
    pub effects: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    Attach(Attach),
    Input {
        epoch: u64,
        record: InputRecord,
    },
    Rendered {
        epoch: u64,
        revision: u64,
    },
    Resync {
        epoch: u64,
    },
    MissingResources {
        epoch: u64,
        #[serde(deserialize_with = "bounded_vec::<_, _, 128>")]
        digests: Vec<Digest>,
    },
    EffectsApplied {
        epoch: u64,
        through: u64,
    },
    Resize {
        epoch: u64,
        generation: u64,
        after: u64,
        size: Size,
    },
    Detach {
        epoch: u64,
    },
    Close {
        epoch: u64,
    },
    ExitAck {
        epoch: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Attached {
        epoch: u64,
        committed: u64,
        effect_floor: u64,
    },
    Committed {
        epoch: u64,
        through: u64,
    },
    Effects {
        epoch: u64,
        floor: u64,
        #[serde(deserialize_with = "bounded_vec::<_, _, MAX_EFFECTS>")]
        items: Vec<Effect>,
    },
    Exit {
        epoch: u64,
        code: i32,
        revision: u64,
        effects: u64,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BootstrapRequest {
    pub version: u16,
    pub client_cert: Vec<u8>,
    pub size: Size,
    pub command: Vec<String>,
    pub bind: Option<String>,
    pub port: Option<u16>,
    pub idle_seconds: u64,
    pub clipboard_read: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Grant {
    pub version: u16,
    pub session: String,
    pub server_cert: Vec<u8>,
    pub port: u16,
    pub address: String,
}
