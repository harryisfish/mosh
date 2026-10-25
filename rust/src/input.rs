use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;

use crate::model::{InputEvent, Key, KeyCode, KeyPhase, MAX_PENDING_INPUT, Mouse, MouseKind, Size};

const ESC: u8 = 0x1b;
const MAX_SEQUENCE: usize = 4096;
const MAX_CLIPBOARD: usize = MAX_PENDING_INPUT / 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalEvent {
    Input(InputEvent),
    Clipboard {
        selection: String,
        data: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EscapeAction {
    Send(Vec<InputEvent>),
    Detach,
    Close,
    Help,
}

struct PendingEscape {
    key: KeyCode,
    events: Vec<InputEvent>,
}

#[derive(Default)]
pub struct EscapeSequence {
    pending: Option<PendingEscape>,
    suppressed: Vec<KeyCode>,
}

impl EscapeSequence {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, event: InputEvent) -> EscapeAction {
        let InputEvent::Key(key) = &event else {
            return EscapeAction::Send(vec![event]);
        };
        let identity = key_identity(key);
        match key.phase {
            KeyPhase::Release => {
                if let Some(index) = self.suppressed.iter().position(|item| item == &identity) {
                    self.suppressed.remove(index);
                    return EscapeAction::Send(Vec::new());
                }
            }
            KeyPhase::Repeat if self.suppressed.contains(&identity) => {
                return EscapeAction::Send(Vec::new());
            }
            KeyPhase::Press => self.suppressed.retain(|item| item != &identity),
            KeyPhase::Repeat => (),
        }
        let Some(mut pending) = self.pending.take() else {
            if key.phase == KeyPhase::Press && is_escape_prefix(key) {
                self.pending = Some(PendingEscape {
                    key: identity,
                    events: vec![event],
                });
                return EscapeAction::Send(Vec::new());
            }
            return EscapeAction::Send(vec![event]);
        };
        if identity == pending.key && key.phase != KeyPhase::Press {
            pending.events.push(event);
            if pending.events.len() >= 32 {
                return EscapeAction::Send(pending.events);
            }
            self.pending = Some(pending);
            return EscapeAction::Send(Vec::new());
        }
        if key.phase != KeyPhase::Press || is_modifier_key(&key.code) {
            // A modifier press may already be remote, so its release cannot be held.
            self.pending = Some(pending);
            return EscapeAction::Send(vec![event]);
        }
        let suffix = escape_suffix(key);
        let action = match suffix {
            Some('.') => EscapeAction::Detach,
            Some('q') => EscapeAction::Close,
            Some('?') => EscapeAction::Help,
            Some('^') => {
                let prefix_released = pending.events.iter().any(
                    |event| matches!(event, InputEvent::Key(key) if key.phase == KeyPhase::Release),
                );
                if identity != pending.key || prefix_released {
                    self.suppress(identity);
                }
                return EscapeAction::Send(pending.events);
            }
            _ => {
                pending.events.push(event);
                return EscapeAction::Send(pending.events);
            }
        };
        self.suppress(pending.key);
        self.suppress(identity);
        action
    }

    fn suppress(&mut self, key: KeyCode) {
        if self.suppressed.contains(&key) {
            return;
        }
        if self.suppressed.len() == 32 {
            self.suppressed.remove(0);
        }
        self.suppressed.push(key);
    }
}

fn key_identity(key: &Key) -> KeyCode {
    key.base
        .map(KeyCode::Char)
        .unwrap_or_else(|| key.code.clone())
}

fn is_escape_prefix(key: &Key) -> bool {
    if key.modifiers & 4 == 0 || key.modifiers & !(1 | 4 | 64 | 128) != 0 {
        return false;
    }
    matches!(key.code, KeyCode::Char('^'))
        || (matches!(key.code, KeyCode::Char('6')) && key.modifiers & 1 != 0)
}

fn escape_suffix(key: &Key) -> Option<char> {
    if is_escape_prefix(key) {
        return Some('^');
    }
    if key.modifiers & !(1 | 64 | 128) != 0 {
        return None;
    }
    if let Some(text) = &key.text {
        let mut chars = text.chars();
        let first = chars.next()?;
        return chars.next().is_none().then_some(first);
    }
    if key.modifiers & 1 != 0 {
        if let Some(shifted) = key.shifted {
            return Some(shifted);
        }
    }
    let KeyCode::Char(ch) = key.code else {
        return None;
    };
    if key.modifiers & 1 != 0 {
        match ch {
            '/' => Some('?'),
            '6' => Some('^'),
            '.' => Some('>'),
            'a'..='z' => Some(ch.to_ascii_uppercase()),
            _ => Some(ch),
        }
    } else {
        Some(ch)
    }
}

fn is_modifier_key(code: &KeyCode) -> bool {
    matches!(code, KeyCode::Named(name) if ["LeftShift", "RightShift", "LeftControl", "RightControl", "LeftAlt", "RightAlt", "LeftSuper", "RightSuper", "LeftHyper", "RightHyper", "LeftMeta", "RightMeta", "Level3Shift", "Level5Shift", "CapsLock", "NumLock", "ScrollLock"].contains(&name.as_str()))
}

#[derive(Default)]
pub struct InputDecoder {
    pending: Vec<u8>,
    paste: Option<Vec<u8>>,
}

impl InputDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<LocalEvent>> {
        let mut events = Vec::new();
        for chunk in bytes.chunks(4096) {
            ensure!(
                self.pending.len() + chunk.len() <= MAX_PENDING_INPUT,
                "local input buffer exceeds limit"
            );
            self.pending.extend_from_slice(chunk);
            self.decode(&mut events)?;
        }
        Ok(events)
    }

    pub fn flush_escape(&mut self) -> Vec<LocalEvent> {
        if self.paste.is_none() && self.pending == [ESC] {
            self.pending.clear();
            vec![LocalEvent::Input(named("Escape", 0))]
        } else {
            Vec::new()
        }
    }

    pub fn has_pending_escape(&self) -> bool {
        self.paste.is_none() && self.pending == [ESC]
    }

    fn decode(&mut self, events: &mut Vec<LocalEvent>) -> Result<()> {
        loop {
            if let Some(paste) = self.paste.as_mut() {
                if let Some(end) = find(&self.pending, b"\x1b[201~") {
                    ensure!(
                        paste.len() + end <= MAX_CLIPBOARD,
                        "paste exceeds input limit"
                    );
                    paste.extend_from_slice(&self.pending[..end]);
                    self.pending.drain(..end + 6);
                    let paste = self.paste.take().unwrap();
                    let text = String::from_utf8(paste).context("paste is not UTF-8")?;
                    events.push(LocalEvent::Input(InputEvent::Paste(text)));
                    continue;
                }
                let keep = suffix_prefix_len(&self.pending, b"\x1b[201~");
                let take = self.pending.len() - keep;
                ensure!(
                    paste.len() + take <= MAX_CLIPBOARD,
                    "paste exceeds input limit"
                );
                paste.extend_from_slice(&self.pending[..take]);
                self.pending.drain(..take);
                break;
            }
            let Some(&byte) = self.pending.first() else {
                break;
            };
            if byte != ESC {
                let Some((event, consumed)) = decode_plain(&self.pending, 0)? else {
                    break;
                };
                self.pending.drain(..consumed);
                events.push(LocalEvent::Input(event));
                continue;
            }
            if self.pending.len() < 2 {
                break;
            }
            match self.pending[1] {
                b'[' => {
                    if self.pending.starts_with(b"\x1b[M") {
                        if self.pending.len() < 6 {
                            break;
                        }
                        let code = self.pending[3]
                            .checked_sub(32)
                            .context("invalid X10 mouse button")?;
                        let x = self.pending[4]
                            .checked_sub(33)
                            .context("invalid X10 mouse column")?;
                        let y = self.pending[5]
                            .checked_sub(33)
                            .context("invalid X10 mouse row")?;
                        let event = (code < 128)
                            .then(|| mouse(u16::from(code), u16::from(x), u16::from(y), false))
                            .transpose()?;
                        self.pending.drain(..6);
                        if let Some(event) = event {
                            events.push(LocalEvent::Input(event));
                        }
                        continue;
                    }
                    let end = self.pending[2..]
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                        .map(|i| i + 2);
                    let Some(end) = end else {
                        ensure!(
                            self.pending.len() <= MAX_SEQUENCE,
                            "keyboard sequence exceeds limit"
                        );
                        break;
                    };
                    ensure!(end <= MAX_SEQUENCE, "keyboard sequence exceeds limit");
                    let body = std::str::from_utf8(&self.pending[2..end])
                        .context("non-ASCII CSI input")?;
                    let final_byte = self.pending[end];
                    if body == "200" && final_byte == b'~' {
                        self.pending.drain(..end + 1);
                        self.paste = Some(Vec::new());
                        continue;
                    }
                    let event = decode_csi(body, final_byte)?;
                    self.pending.drain(..end + 1);
                    if let Some(event) = event {
                        events.push(LocalEvent::Input(event));
                    }
                }
                b'O' => {
                    let end = self.pending[2..]
                        .iter()
                        .position(|b| (0x40..=0x7e).contains(b))
                        .map(|i| i + 2);
                    let Some(end) = end else {
                        ensure!(
                            self.pending.len() <= MAX_SEQUENCE,
                            "SS3 input exceeds limit"
                        );
                        break;
                    };
                    let body =
                        std::str::from_utf8(&self.pending[2..end]).context("invalid SS3 input")?;
                    if let Some(event) = decode_ss3(body, self.pending[end])? {
                        events.push(LocalEvent::Input(event));
                    }
                    self.pending.drain(..end + 1);
                }
                b']' | b'_' | b'P' => {
                    let limit = if self.pending[1] == b']' {
                        MAX_CLIPBOARD * 4 / 3 + 128
                    } else {
                        MAX_SEQUENCE
                    };
                    let Some((end, terminator)) = string_end(&self.pending[2..]) else {
                        ensure!(self.pending.len() <= limit, "terminal reply exceeds limit");
                        break;
                    };
                    ensure!(end <= limit, "terminal reply exceeds limit");
                    if self.pending[1] == b']' {
                        if let Some(reply) = decode_osc(&self.pending[2..2 + end])? {
                            events.push(reply);
                        }
                    }
                    self.pending.drain(..2 + end + terminator);
                }
                ESC => {
                    self.pending.drain(..1);
                    events.push(LocalEvent::Input(named("Escape", 0)));
                }
                _ => {
                    let Some((event, consumed)) = decode_plain(&self.pending[1..], 2)? else {
                        break;
                    };
                    self.pending.drain(..consumed + 1);
                    events.push(LocalEvent::Input(event));
                }
            }
        }
        Ok(())
    }
}

fn key(code: KeyCode, modifiers: u16) -> InputEvent {
    InputEvent::Key(Key {
        code,
        modifiers,
        phase: KeyPhase::Press,
        shifted: None,
        base: None,
        text: None,
    })
}

fn named(name: &str, modifiers: u16) -> InputEvent {
    key(KeyCode::Named(name.into()), modifiers)
}

fn decode_plain(bytes: &[u8], modifiers: u16) -> Result<Option<(InputEvent, usize)>> {
    let byte = bytes[0];
    let event = match byte {
        b'\r' | b'\n' => named("Enter", modifiers),
        b'\t' => named("Tab", modifiers),
        0x7f | 0x08 => named("Backspace", modifiers),
        0 => key(KeyCode::Char(' '), modifiers | 4),
        1..=26 => key(KeyCode::Char(char::from(b'a' + byte - 1)), modifiers | 4),
        0x1b => named("Escape", modifiers),
        0x1c..=0x1f => key(KeyCode::Char(char::from(byte + 0x40)), modifiers | 4),
        _ => {
            let len = match byte {
                0x20..=0x7e => 1,
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => bail!("invalid UTF-8 terminal input"),
            };
            if bytes.len() < len {
                return Ok(None);
            }
            let text =
                std::str::from_utf8(&bytes[..len]).context("invalid UTF-8 terminal input")?;
            return Ok(Some((
                key(KeyCode::Char(text.chars().next().unwrap()), modifiers),
                len,
            )));
        }
    };
    Ok(Some((event, 1)))
}

fn decode_csi(body: &str, final_byte: u8) -> Result<Option<InputEvent>> {
    if final_byte == b'u' && !body.starts_with(['?', '>', '<', '=']) {
        return Ok(Some(decode_kitty(body)?));
    }
    if let Some(body) = body.strip_prefix('<') {
        if matches!(final_byte, b'M' | b'm') {
            let values = numeric_fields(body, ';')?;
            ensure!(
                values.len() == 3 && values[1] > 0 && values[2] > 0,
                "invalid SGR mouse event"
            );
            if values[0] >= 128 {
                return Ok(None);
            }
            return Ok(Some(mouse(
                values[0],
                values[1] - 1,
                values[2] - 1,
                final_byte == b'm',
            )?));
        }
        return Ok(None);
    }
    match (body, final_byte) {
        ("", b'I') => return Ok(Some(InputEvent::Focus(true))),
        ("", b'O') => return Ok(Some(InputEvent::Focus(false))),
        _ => (),
    }
    if body.starts_with(['?', '>', '<', '=']) || final_byte == b'c' {
        return Ok(None);
    }
    let fields = numeric_fields(body, ';')?;
    let modifiers = fields.get(1).copied().unwrap_or(1).saturating_sub(1);
    ensure!(modifiers < 256, "unknown keyboard modifiers");
    let name = match final_byte {
        b'A' => "Up",
        b'B' => "Down",
        b'C' => "Right",
        b'D' => "Left",
        b'H' => "Home",
        b'F' => "End",
        b'Z' => return Ok(Some(named("Tab", modifiers | 1))),
        b'P' => "F1",
        b'Q' => "F2",
        b'R' if fields.first().copied().unwrap_or(1) == 1 => "F3",
        b'S' => "F4",
        b'~' => {
            let code = fields.first().copied().unwrap_or(0);
            if code == 27 && fields.len() == 3 {
                let ch = char::from_u32(u32::from(fields[2]))
                    .context("invalid modifyOtherKeys codepoint")?;
                return Ok(Some(key(KeyCode::Char(ch), modifiers)));
            }
            match code {
                1 | 7 => "Home",
                2 => "Insert",
                3 => "Delete",
                4 | 8 => "End",
                5 => "PageUp",
                6 => "PageDown",
                11 => "F1",
                12 => "F2",
                13 => "F3",
                14 => "F4",
                15 => "F5",
                17 => "F6",
                18 => "F7",
                19 => "F8",
                20 => "F9",
                21 => "F10",
                23 => "F11",
                24 => "F12",
                25 => "F13",
                26 => "F14",
                28 => "F15",
                29 => "F16",
                31 => "F17",
                32 => "F18",
                33 => "F19",
                34 => "F20",
                201 => return Ok(None),
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(named(name, modifiers)))
}

fn decode_ss3(body: &str, final_byte: u8) -> Result<Option<InputEvent>> {
    let modifiers = numeric_fields(body, ';')?
        .get(1)
        .copied()
        .unwrap_or(1)
        .saturating_sub(1);
    let name = match final_byte {
        b'A' => "Up",
        b'B' => "Down",
        b'C' => "Right",
        b'D' => "Left",
        b'H' => "Home",
        b'F' => "End",
        b'P' => "F1",
        b'Q' => "F2",
        b'R' => "F3",
        b'S' => "F4",
        b'M' => "KeypadEnter",
        b'j' => "KeypadMultiply",
        b'k' => "KeypadAdd",
        b'l' => "KeypadSeparator",
        b'm' => "KeypadSubtract",
        b'n' => "KeypadDecimal",
        b'o' => "KeypadDivide",
        b'p'..=b'y' => {
            return Ok(Some(named(
                &format!("Keypad{}", final_byte - b'p'),
                modifiers,
            )));
        }
        _ => return Ok(None),
    };
    Ok(Some(named(name, modifiers)))
}

fn decode_kitty(body: &str) -> Result<InputEvent> {
    let fields: Vec<_> = body.split(';').collect();
    ensure!((1..=3).contains(&fields.len()), "invalid kitty key event");
    let codes: Vec<_> = fields[0].split(':').collect();
    ensure!(
        (1..=3).contains(&codes.len()),
        "invalid kitty alternate keys"
    );
    let code: u32 = codes[0].parse().context("invalid kitty key code")?;
    let shifted = codes
        .get(1)
        .filter(|s| !s.is_empty())
        .map(|s| parse_char(s))
        .transpose()?;
    let base = codes
        .get(2)
        .filter(|s| !s.is_empty())
        .map(|s| parse_char(s))
        .transpose()?;
    let mods: Vec<_> = fields.get(1).copied().unwrap_or("1").split(':').collect();
    ensure!(mods.len() <= 2, "invalid kitty event modifiers");
    let modifiers: u16 = mods[0]
        .parse::<u16>()
        .context("invalid kitty modifiers")?
        .checked_sub(1)
        .context("zero kitty modifiers")?;
    ensure!(modifiers < 256, "unknown kitty modifiers");
    let phase = match mods.get(1).copied().unwrap_or("1") {
        "1" | "" => KeyPhase::Press,
        "2" => KeyPhase::Repeat,
        "3" => KeyPhase::Release,
        _ => bail!("unknown kitty key event type"),
    };
    let text = fields
        .get(2)
        .filter(|text| !text.is_empty())
        .map(|text| text.split(':').map(parse_char).collect::<Result<String>>())
        .transpose()?;
    ensure!(
        text.as_ref().is_none_or(|s| s.len() <= 4096),
        "associated key text exceeds limit"
    );
    let code = kitty_code(code)?;
    Ok(InputEvent::Key(Key {
        code,
        modifiers,
        phase,
        shifted,
        base,
        text,
    }))
}

fn kitty_code(code: u32) -> Result<KeyCode> {
    let name = match code {
        27 | 57344 => "Escape",
        13 | 57345 => "Enter",
        9 | 57346 => "Tab",
        127 | 57347 => "Backspace",
        57348 => "Insert",
        57349 => "Delete",
        57350 => "Left",
        57351 => "Right",
        57352 => "Up",
        57353 => "Down",
        57354 => "PageUp",
        57355 => "PageDown",
        57356 => "Home",
        57357 => "End",
        57358 => "CapsLock",
        57359 => "ScrollLock",
        57360 => "NumLock",
        57361 => "PrintScreen",
        57362 => "Pause",
        57363 => "Menu",
        57364..=57398 => return Ok(KeyCode::Named(format!("F{}", code - 57363))),
        57399..=57408 => return Ok(KeyCode::Named(format!("Keypad{}", code - 57399))),
        57409 => "KeypadDecimal",
        57410 => "KeypadDivide",
        57411 => "KeypadMultiply",
        57412 => "KeypadSubtract",
        57413 => "KeypadAdd",
        57414 => "KeypadEnter",
        57415 => "KeypadEqual",
        57416 => "KeypadSeparator",
        57417 => "KeypadLeft",
        57418 => "KeypadRight",
        57419 => "KeypadUp",
        57420 => "KeypadDown",
        57421 => "KeypadPageUp",
        57422 => "KeypadPageDown",
        57423 => "KeypadHome",
        57424 => "KeypadEnd",
        57425 => "KeypadInsert",
        57426 => "KeypadDelete",
        57427 => "KeypadBegin",
        57428 => "MediaPlay",
        57429 => "MediaPause",
        57430 => "MediaPlayPause",
        57431 => "MediaReverse",
        57432 => "MediaStop",
        57433 => "MediaFastForward",
        57434 => "MediaRewind",
        57435 => "MediaNext",
        57436 => "MediaPrevious",
        57437 => "MediaRecord",
        57438 => "VolumeDown",
        57439 => "VolumeUp",
        57440 => "VolumeMute",
        57441 => "LeftShift",
        57442 => "LeftControl",
        57443 => "LeftAlt",
        57444 => "LeftSuper",
        57445 => "LeftHyper",
        57446 => "LeftMeta",
        57447 => "RightShift",
        57448 => "RightControl",
        57449 => "RightAlt",
        57450 => "RightSuper",
        57451 => "RightHyper",
        57452 => "RightMeta",
        57453 => "Level3Shift",
        57454 => "Level5Shift",
        57344..=63743 => bail!("unsupported kitty functional key {code}"),
        _ => {
            return Ok(KeyCode::Char(
                char::from_u32(code).context("invalid kitty Unicode key")?,
            ));
        }
    };
    Ok(KeyCode::Named(name.into()))
}

fn mouse(code: u16, x: u16, y: u16, released: bool) -> Result<InputEvent> {
    ensure!(x < 512 && y < 256, "mouse position exceeds terminal limits");
    let modifiers = ((code & 4) >> 2) | ((code & 8) >> 2) | ((code & 16) >> 2);
    let button = (code & 3) as u8;
    let kind = if code & 64 != 0 {
        let amount = if button & 1 == 0 { 1 } else { -1 };
        if button & 2 == 0 {
            MouseKind::Wheel(amount)
        } else {
            MouseKind::WheelHorizontal(amount)
        }
    } else if released {
        MouseKind::Release(button)
    } else if code & 32 != 0 {
        MouseKind::Move
    } else if button == 3 {
        MouseKind::Release(button)
    } else {
        MouseKind::Press(button)
    };
    Ok(InputEvent::Mouse(Mouse {
        x,
        y,
        pixel_x: 0,
        pixel_y: 0,
        modifiers,
        kind,
    }))
}

fn numeric_fields(body: &str, separator: char) -> Result<Vec<u16>> {
    if body.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(
        body.len() <= MAX_SEQUENCE,
        "terminal input parameter exceeds limit"
    );
    body.split(separator)
        .map(|field| {
            if field.is_empty() {
                Ok(1)
            } else {
                field.parse().context("invalid terminal input parameter")
            }
        })
        .collect()
}

fn parse_char(value: &str) -> Result<char> {
    char::from_u32(value.parse().context("invalid input codepoint")?)
        .context("invalid input codepoint")
}

fn decode_osc(bytes: &[u8]) -> Result<Option<LocalEvent>> {
    let body = std::str::from_utf8(bytes).context("invalid terminal OSC reply")?;
    let Some(body) = body.strip_prefix("52;") else {
        return Ok(None);
    };
    let Some((selection, data)) = body.split_once(';') else {
        bail!("invalid clipboard reply")
    };
    ensure!(
        selection.len() <= 16 && selection.bytes().all(|b| b.is_ascii_alphanumeric()),
        "invalid clipboard selection"
    );
    if data == "?" {
        return Ok(None);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("invalid clipboard base64")?;
    ensure!(
        decoded.len() <= MAX_CLIPBOARD,
        "clipboard reply exceeds input limit"
    );
    Ok(Some(LocalEvent::Clipboard {
        selection: selection.into(),
        data: String::from_utf8(decoded).ok(),
    }))
}

fn find(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|w| w == needle)
}

fn suffix_prefix_len(bytes: &[u8], needle: &[u8]) -> usize {
    (1..needle.len().min(bytes.len() + 1))
        .rev()
        .find(|&len| bytes.ends_with(&needle[..len]))
        .unwrap_or(0)
}

fn string_end(bytes: &[u8]) -> Option<(usize, usize)> {
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == 7 {
            return Some((index, 1));
        }
        if *byte == ESC && bytes.get(index + 1) == Some(&b'\\') {
            return Some((index, 2));
        }
    }
    None
}

pub struct LocalTerminal {
    tty: File,
    saved: libc::termios,
}

impl LocalTerminal {
    pub fn enter() -> Result<Self> {
        // These descriptors are process-owned and remain valid throughout the call.
        ensure!(
            unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 },
            "mosh-rs requires a terminal on stdin and stdout"
        );
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .context("open controlling terminal")?;
        let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
        // tcgetattr initializes the structure on success.
        ensure!(
            unsafe { libc::tcgetattr(tty.as_raw_fd(), saved.as_mut_ptr()) } == 0,
            "read terminal modes: {}",
            io::Error::last_os_error()
        );
        let saved = unsafe { saved.assume_init() };
        let mut raw = saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        ensure!(
            unsafe { libc::tcsetattr(tty.as_raw_fd(), libc::TCSANOW, &raw) } == 0,
            "set raw terminal modes: {}",
            io::Error::last_os_error()
        );
        let mut guard = Self { tty, saved };
        if let Err(error) = guard.tty.write_all(b"\x1b[22;0t\x1b[?1049h\x1b[?7l\x1b[?2004h\x1b[?1004h\x1b[?1003h\x1b[?1006h\x1b[>31u\x1b[?25l").and_then(|_| guard.tty.flush()) {
            drop(guard);
            return Err(error).context("enable terminal input modes");
        }
        Ok(guard)
    }

    pub fn size() -> Result<Size> {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        ensure!(
            unsafe { libc::ioctl(0, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0,
            "read terminal size: {}",
            io::Error::last_os_error()
        );
        let size = unsafe { size.assume_init() };
        let size = Size {
            cols: size.ws_col.max(1),
            rows: size.ws_row.max(1),
            pixel_width: u32::from(size.ws_xpixel),
            pixel_height: u32::from(size.ws_ypixel),
        };
        size.validate()?;
        Ok(size)
    }
}

impl Drop for LocalTerminal {
    fn drop(&mut self) {
        let _ = self.tty.write_all(b"\x1b[?2026l\x1b_Ga=d,d=A,q=2\x1b\\\x1b[<u\x1b[?1003l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b]8;;\x1b\\\x1b[0m\x1b[?5l\x1b[?25h\x1b[?7h\x1b[?1049l\x1b[23;0t");
        let _ = self.tty.flush();
        unsafe {
            libc::tcsetattr(self.tty.as_raw_fd(), libc::TCSANOW, &self.saved);
        }
    }
}
