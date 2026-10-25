use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::model::*;
use anyhow::{Result, anyhow, bail, ensure};
use base64::Engine;
use core::escape::osc::{FinalTermSemanticPrompt as Marker, Selection};
use core::escape::{Action, OperatingSystemCommand as Osc};
use tattoy_wezterm_term as core;

const SYNC_TIMEOUT: Duration = Duration::from_millis(150);
const MAX_CELL_BYTES: usize = 64;

#[derive(Default)]
struct Collected {
    bytes: Vec<u8>,
    failed: bool,
}
#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Collected>>);
impl Write for Collector {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.0.lock().unwrap();
        if state.failed || bytes.len() > MAX_PENDING_INPUT.saturating_sub(state.bytes.len()) {
            state.failed = true;
            return Err(io::Error::other("terminal reply collector limit exceeded"));
        }
        state.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Collector {
    fn check(&self) -> Result<()> {
        ensure!(
            !self.0.lock().unwrap().failed,
            "terminal collector failed; session must terminate"
        );
        Ok(())
    }
    fn take(&self) -> Result<Vec<u8>> {
        self.check()?;
        Ok(std::mem::take(&mut self.0.lock().unwrap().bytes))
    }
}

#[derive(Debug)]
struct Config;
impl core::TerminalConfiguration for Config {
    fn color_palette(&self) -> core::color::ColorPalette {
        Default::default()
    }
    fn enable_kitty_graphics(&self) -> bool {
        true
    }
    fn enable_kitty_keyboard(&self) -> bool {
        true
    }
    fn scrollback_size(&self) -> usize {
        128
    }
}

pub struct ModelChange {
    pub replies: Vec<u8>,
    pub effects: Vec<EffectKind>,
    pub publish_allowed: bool,
}

pub struct TerminalModel {
    terminal: core::Terminal,
    collector: Collector,
    size: Size,
    state: Observer,
    resources: BTreeMap<([u8; 32], u32, u32), Resource>,
    failed: bool,
}
struct Observer {
    clipboard_read: bool,
    queries: BTreeMap<u64, String>,
    next_query: u64,
    markers: VecDeque<ShellMarker>,
    generation: u64,
    sync_started: Option<Instant>,
    sync_expired: bool,
}
impl TerminalModel {
    pub fn new(size: Size, clipboard_read: bool) -> Result<Self> {
        size.validate()?;
        let collector = Collector::default();
        let terminal = core::Terminal::new(
            core_size(size),
            Arc::new(Config),
            "mosh-rs",
            env!("CARGO_PKG_VERSION"),
            Box::new(collector.clone()),
        );
        Ok(Self {
            terminal,
            collector,
            size,
            state: Observer {
                clipboard_read,
                queries: BTreeMap::new(),
                next_query: 0,
                markers: VecDeque::new(),
                generation: 1,
                sync_started: None,
                sync_expired: false,
            },
            resources: BTreeMap::new(),
            failed: false,
        })
    }
    fn healthy(&self) -> Result<()> {
        ensure!(
            !self.failed,
            "terminal model failed; session must terminate"
        );
        self.collector.check()
    }
    pub fn advance(&mut self, bytes: &[u8]) -> Result<ModelChange> {
        self.healthy()?;
        ensure!(
            bytes.len() <= core::bounded::MAX_STRING,
            "terminal input batch exceeds limit"
        );
        let mut effects = Vec::new();
        let observer = &mut self.state;
        let collector = self.collector.clone();
        let mut reply_writer = collector.clone();
        let result = self.terminal.advance_bytes_checked(
            bytes,
            |action, terminal| observer.before(action, terminal, &mut effects, &mut reply_writer),
            || collector.check(),
        );
        if let Err(error) = result {
            self.failed = true;
            return Err(error);
        }
        Ok(ModelChange {
            replies: self.collector.take()?,
            effects,
            publish_allowed: self.publish_allowed(),
        })
    }
    pub fn publish_allowed(&mut self) -> bool {
        if self
            .state
            .sync_started
            .is_some_and(|start| start.elapsed() >= SYNC_TIMEOUT)
        {
            self.state.sync_started = None;
            self.state.sync_expired = true;
        }
        self.state.sync_started.is_none()
    }
    pub fn encode_input(&mut self, event: &InputEvent) -> Result<Vec<u8>> {
        self.healthy()?;
        event.validate()?;
        let result = match event {
            InputEvent::Key(key) => self.terminal.semantic_key(&core::SemanticKey {
                key: match key_code(&key.code) {
                    Ok(key) => Some(key),
                    Err(error) if functional_code(&key.code).is_some() => {
                        let _ = error;
                        None
                    }
                    Err(error) => return Err(error),
                },
                functional: functional_code(&key.code),
                modifiers: key.modifiers,
                phase: match key.phase {
                    KeyPhase::Press => 1,
                    KeyPhase::Repeat => 2,
                    KeyPhase::Release => 3,
                },
                shifted: key.shifted,
                base: key.base,
                text: key.text.clone(),
            }),
            InputEvent::Paste(text) => self.terminal.send_paste(text),
            InputEvent::Mouse(mouse) => {
                let (kind, button) = match mouse.kind {
                    MouseKind::Press(button) => {
                        (core::MouseEventKind::Press, mouse_button(button)?)
                    }
                    MouseKind::Release(button) => {
                        (core::MouseEventKind::Release, mouse_button(button)?)
                    }
                    MouseKind::Move => (core::MouseEventKind::Move, core::MouseButton::None),
                    MouseKind::WheelHorizontal(delta) => (
                        core::MouseEventKind::Press,
                        if delta > 0 {
                            core::MouseButton::WheelLeft(delta as usize)
                        } else {
                            core::MouseButton::WheelRight(delta.unsigned_abs() as usize)
                        },
                    ),
                    MouseKind::Wheel(delta) => (
                        core::MouseEventKind::Press,
                        if delta > 0 {
                            core::MouseButton::WheelUp(delta as usize)
                        } else {
                            core::MouseButton::WheelDown(delta.unsigned_abs() as usize)
                        },
                    ),
                };
                self.terminal.mouse_event(core::MouseEvent {
                    kind,
                    button,
                    x: mouse.x as usize,
                    y: mouse.y as i64,
                    x_pixel_offset: mouse.pixel_x as isize,
                    y_pixel_offset: mouse.pixel_y as isize,
                    modifiers: modifiers(mouse.modifiers),
                })
            }
            InputEvent::Focus(focused) => {
                self.terminal.focus_changed(*focused);
                Ok(())
            }
            InputEvent::QueryResult { query, data } => {
                if let Some(selection) = self.state.queries.remove(query) {
                    let data = data
                        .as_ref()
                        .map(|s| base64::engine::general_purpose::STANDARD.encode(s.as_bytes()))
                        .unwrap_or_default();
                    write!(self.collector, "\x1b]52;{selection};{data}\x1b\\")?;
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            self.failed = true;
            return Err(error);
        }
        match self.collector.take() {
            Ok(bytes) => Ok(bytes),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
    pub fn resize(&mut self, size: Size) -> Result<()> {
        self.healthy()?;
        size.validate()?;
        self.terminal.resize(core_size(size));
        self.size = size;
        self.state.generation = self
            .state
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("mode generation exhausted"))?;
        Ok(())
    }
    pub fn snapshot(&mut self) -> Result<Screen> {
        self.healthy()?;
        let live: BTreeSet<_> = self.terminal.live_image_keys().into_iter().collect();
        self.resources.retain(|key, _| live.contains(key));
        let screen = self.terminal.screen();
        let first = screen.phys_row(0);
        let lines = screen.lines_in_phys_range(first..first + usize::from(self.size.rows));
        let palette = self.terminal.palette();
        let mut resources = BTreeMap::new();
        let mut used = BTreeSet::new();
        let mut rows = Vec::new();
        let mut string_budget = 4 * 1024 * 1024usize;
        let mut encoded_budget = FrameBudget {
            remaining: MAX_FRAME - 1024 * 1024,
        };
        for (y, line) in lines.iter().enumerate() {
            let mut cells = Vec::new();
            for cell in line.visible_cells() {
                ensure!(
                    cell.str().len() <= MAX_CELL_BYTES,
                    "grapheme exceeds snapshot limit"
                );
                let attrs = cell.attrs();
                let string_bytes = cell.str().len()
                    + attrs.hyperlink().map_or(0, |link| {
                        link.uri().len()
                            + link
                                .params()
                                .iter()
                                .map(|(k, v)| k.len() + v.len() + 2)
                                .sum::<usize>()
                    });
                ensure!(
                    string_bytes <= string_budget,
                    "snapshot text and hyperlink budget exceeded"
                );
                string_budget -= string_bytes;
                let mut images = Vec::new();
                for image in attrs.images().unwrap_or_default() {
                    let data = image.image_data().data();
                    let (width, height) = data.dimensions()?;
                    let key = (image.image_data().hash(), width, height);
                    let resource = if let Some(resource) = self.resources.get(&key) {
                        resource.clone()
                    } else {
                        let core::image::ImageDataType::Rgba8 { data, .. } = &*data else {
                            bail!("core retained a non-static image");
                        };
                        let digest = resource_digest(width, height, data);
                        let resource = Resource {
                            meta: ResourceMeta {
                                digest,
                                width,
                                height,
                                bytes: data.len() as u32,
                            },
                            rgba: Arc::from(data.as_slice()),
                        };
                        resource.validate()?;
                        self.resources.insert(key, resource.clone());
                        resource
                    };
                    used.insert(key);
                    let top = image.top_left();
                    let bottom = image.bottom_right();
                    let x = (top.x.into_inner() * width as f32).round() as u32;
                    let y = (top.y.into_inner() * height as f32).round() as u32;
                    let right = (bottom.x.into_inner() * width as f32).round() as u32;
                    let bottom = (bottom.y.into_inner() * height as f32).round() as u32;
                    let padding = image.padding();
                    images.push(ImageTile {
                        resource: resource.meta.digest,
                        source: [x, y, right.saturating_sub(x), bottom.saturating_sub(y)],
                        padding: [padding.0, padding.1, padding.2, padding.3],
                        z: image.z_index(),
                        placement: (u64::from(image.image_id().unwrap_or(0)) << 32)
                            | u64::from(image.placement_id().unwrap_or(0)),
                    });
                    resources.insert(resource.meta.digest, resource);
                }
                let mut flags = 0;
                flags |= match attrs.intensity() {
                    core::Intensity::Bold => 1,
                    core::Intensity::Half => 2,
                    _ => 0,
                };
                for (present, flag) in [
                    (attrs.italic(), 4),
                    (attrs.blink() != core::Blink::None, 8),
                    (attrs.reverse(), 16),
                    (attrs.invisible(), 32),
                    (attrs.strikethrough(), 64),
                    (attrs.overline(), 128),
                ] {
                    if present {
                        flags |= flag;
                    }
                }
                let hyperlink = attrs.hyperlink().map(|link| {
                    let mut params: Vec<_> = link.params().iter().collect();
                    params.sort_by(|a, b| a.0.cmp(b.0));
                    Hyperlink {
                        params: params
                            .into_iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join(":"),
                        url: link.uri().to_owned(),
                    }
                });
                let underline = match attrs.underline() {
                    core::Underline::None => 0,
                    core::Underline::Single => 1,
                    core::Underline::Double => 2,
                    core::Underline::Curly => 3,
                    core::Underline::Dotted => 4,
                    core::Underline::Dashed => 5,
                };
                let projected = Cell {
                    x: cell.cell_index() as u16,
                    text: cell.str().to_owned(),
                    width: cell.width() as u8,
                    style: Style {
                        fg: resolved_color(attrs.foreground(), &palette, palette.foreground),
                        bg: resolved_color(attrs.background(), &palette, palette.background),
                        flags,
                        underline,
                        underline_color: color(attrs.underline_color(), &palette),
                        hyperlink,
                    },
                    images,
                };
                postcard::to_io(&projected, &mut encoded_budget)?;
                cells.push(projected);
            }
            rows.push(Row {
                cells,
                stable_id: screen.visible_row_to_stable_row(y as i64) as i64,
            });
        }
        let live: BTreeSet<_> = self.terminal.live_image_keys().into_iter().collect();
        self.resources
            .retain(|key, _| used.contains(key) || live.contains(key));
        ensure!(
            resources
                .values()
                .map(|resource| resource.rgba.len())
                .sum::<usize>()
                <= MAX_RESOURCES,
            "snapshot resource budget exceeded"
        );
        let cursor = self.terminal.cursor_pos();
        let shape = match cursor.shape {
            core::CursorShape::Default => 0,
            core::CursorShape::BlinkingBlock | core::CursorShape::SteadyBlock => 1,
            core::CursorShape::BlinkingUnderline | core::CursorShape::SteadyUnderline => 2,
            core::CursorShape::BlinkingBar | core::CursorShape::SteadyBar => 3,
        };
        let prediction = PredictionContext {
            generation: self.state.generation,
            eligible: self.terminal.prediction_eligible(),
            echo_enabled: false,
            alternate_screen: self.terminal.is_alt_screen_active(),
        };
        Ok(Screen {
            size: self.size,
            rows,
            cursor: Cursor {
                x: cursor.x as u16,
                y: cursor.y.max(0) as u16,
                shape,
                blink: cursor.shape.is_blinking(),
                visible: cursor.visibility == core::CursorVisibility::Visible,
            },
            title: self.terminal.get_title().to_owned(),
            directory: self.terminal.get_current_dir().map(ToString::to_string),
            markers: self.state.markers.iter().cloned().collect(),
            reverse_video: self.terminal.get_reverse_video(),
            prediction,
            resources,
        })
    }
}

impl Observer {
    fn before(
        &mut self,
        action: &Action,
        terminal: &core::TerminalState,
        effects: &mut Vec<EffectKind>,
        replies: &mut Collector,
    ) -> Result<bool> {
        if matches!(
            action,
            Action::CSI(core::escape::CSI::Mode(_) | core::escape::CSI::Keyboard(_))
                | Action::Esc(_)
        ) {
            self.generation = self
                .generation
                .checked_add(1)
                .ok_or_else(|| anyhow!("mode generation exhausted"))?;
        }
        if let Action::CSI(core::escape::CSI::Mode(mode)) = action {
            let mode = mode.to_string();
            if mode == "?2026h" {
                if !self.sync_expired && self.sync_started.is_none() {
                    self.sync_started = Some(Instant::now());
                }
                return Ok(false);
            }
            if mode == "?2026l" {
                self.sync_started = None;
                self.sync_expired = false;
                return Ok(false);
            }
        }
        let effect = match action {
            Action::Control(core::escape::ControlCode::Bell) => Some(EffectKind::Bell),
            Action::OperatingSystemCommand(osc) => match osc.as_ref() {
                Osc::SetSelection(selection, data) => {
                    ensure!(
                        data.len() <= MAX_PENDING_INPUT / 2,
                        "clipboard write exceeds limit"
                    );
                    Some(EffectKind::ClipboardWrite {
                        selection: selection_name(*selection),
                        data: data.clone(),
                    })
                }
                Osc::ClearSelection(selection) => Some(EffectKind::ClipboardWrite {
                    selection: selection_name(*selection),
                    data: String::new(),
                }),
                Osc::QuerySelection(selection) => {
                    let selection = selection_name(*selection);
                    if !self.clipboard_read
                        || self.queries.len() >= MAX_EFFECTS
                        || effects.len() >= MAX_EFFECTS
                    {
                        write!(replies, "\x1b]52;{selection};\x1b\\")?;
                        return Ok(false);
                    }
                    self.next_query = self
                        .next_query
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("clipboard query sequence exhausted"))?;
                    self.queries.insert(self.next_query, selection.clone());
                    Some(EffectKind::ClipboardRead {
                        query: self.next_query,
                        selection,
                    })
                }
                Osc::FinalTermSemanticPrompt(marker) => {
                    let (kind, status) = match marker {
                        Marker::FreshLineAndStartPrompt { .. } | Marker::StartPrompt(_) => {
                            ("A", None)
                        }
                        Marker::MarkEndOfPromptAndStartOfInputUntilNextMarker
                        | Marker::MarkEndOfPromptAndStartOfInputUntilEndOfLine => ("B", None),
                        Marker::MarkEndOfInputAndStartOfOutput { .. } => ("C", None),
                        Marker::CommandStatus { status, .. } => ("D", Some(*status)),
                        _ => ("L", None),
                    };
                    let cursor = terminal.cursor_pos();
                    self.markers.push_back(ShellMarker {
                        row: terminal.screen().visible_row_to_stable_row(cursor.y) as i64,
                        column: cursor.x as u16,
                        kind: kind.to_owned(),
                        status,
                    });
                    if self.markers.len() > 256 {
                        self.markers.pop_front();
                    }
                    None
                }
                Osc::SetHyperlink(Some(link)) => {
                    ensure!(
                        link.uri().len() <= 8192
                            && link
                                .params()
                                .iter()
                                .map(|(k, v)| k.len() + v.len())
                                .sum::<usize>()
                                <= 8192,
                        "hyperlink exceeds limit"
                    );
                    None
                }
                Osc::SystemNotification(body) => {
                    ensure!(body.len() <= 4096, "notification exceeds limit");
                    Some(EffectKind::Notification {
                        title: String::new(),
                        body: body.clone(),
                    })
                }
                Osc::CurrentWorkingDirectory(directory) => {
                    ensure!(directory.len() <= 8192, "directory URI exceeds limit");
                    None
                }
                Osc::SetWindowTitle(title)
                | Osc::SetIconNameAndWindowTitle(title)
                | Osc::SetWindowTitleSun(title) => {
                    ensure!(title.len() <= 4096, "title exceeds limit");
                    None
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(effect) = effect {
            let replace = effects
                .iter()
                .position(|existing| match (existing, &effect) {
                    (EffectKind::Bell, EffectKind::Bell) => true,
                    (EffectKind::Notification { .. }, EffectKind::Notification { .. }) => true,
                    (
                        EffectKind::ClipboardWrite { selection: a, .. },
                        EffectKind::ClipboardWrite { selection: b, .. },
                    ) => a == b,
                    _ => false,
                });
            if let Some(index) = replace {
                effects.remove(index);
            }
            let bytes = effects.iter().map(effect_size).sum::<usize>();
            if effects.len() < MAX_EFFECTS && bytes + effect_size(&effect) <= MAX_EFFECT_BYTES {
                effects.push(effect);
            } else if let EffectKind::ClipboardRead { query, selection } = effect {
                self.queries.remove(&query);
                write!(replies, "\x1b]52;{selection};\x1b\\")?;
            }
            return Ok(false);
        }
        Ok(true)
    }
}
fn effect_size(effect: &EffectKind) -> usize {
    match effect {
        EffectKind::Bell => 1,
        EffectKind::ClipboardWrite { data, .. } => data.len() + 32,
        EffectKind::ClipboardRead { .. } => 64,
        EffectKind::Notification { title, body } => title.len() + body.len() + 32,
    }
}
fn selection_name(selection: Selection) -> String {
    let mut result = String::new();
    for (flag, name) in [
        (Selection::CLIPBOARD, 'c'),
        (Selection::PRIMARY, 'p'),
        (Selection::SELECT, 's'),
        (Selection::CUT0, '0'),
        (Selection::CUT1, '1'),
        (Selection::CUT2, '2'),
        (Selection::CUT3, '3'),
        (Selection::CUT4, '4'),
        (Selection::CUT5, '5'),
        (Selection::CUT6, '6'),
        (Selection::CUT7, '7'),
        (Selection::CUT8, '8'),
        (Selection::CUT9, '9'),
    ] {
        if selection.contains(flag) {
            result.push(name);
        }
    }
    if result.is_empty() {
        result.push('c');
    }
    result
}
fn core_size(size: Size) -> core::TerminalSize {
    core::TerminalSize {
        rows: size.rows as usize,
        cols: size.cols as usize,
        pixel_width: if size.pixel_width == 0 {
            usize::from(size.cols) * 10
        } else {
            size.pixel_width.max(u32::from(size.cols)) as usize
        },
        pixel_height: if size.pixel_height == 0 {
            usize::from(size.rows) * 20
        } else {
            size.pixel_height.max(u32::from(size.rows)) as usize
        },
        dpi: 96,
    }
}
fn color(value: core::color::ColorAttribute, palette: &core::color::ColorPalette) -> Color {
    use core::color::ColorAttribute::*;
    let value = match value {
        Default => return Color::Default,
        PaletteIndex(index) => palette.colors.0[index as usize],
        TrueColorWithDefaultFallback(rgb) | TrueColorWithPaletteFallback(rgb, _) => rgb,
    };
    let (r, g, b, _) = value.as_rgba_u8();
    Color::Rgb([r, g, b])
}
fn modifiers(bits: u16) -> core::KeyModifiers {
    let mut mods = core::KeyModifiers::NONE;
    for (bit, flag) in [
        (1, core::KeyModifiers::SHIFT),
        (2, core::KeyModifiers::ALT),
        (4, core::KeyModifiers::CTRL),
        (8, core::KeyModifiers::SUPER),
    ] {
        if bits & bit != 0 {
            mods |= flag;
        }
    }
    mods
}
fn mouse_button(button: u8) -> Result<core::MouseButton> {
    Ok(match button {
        0 => core::MouseButton::Left,
        1 => core::MouseButton::Middle,
        2 => core::MouseButton::Right,
        _ => bail!("unsupported mouse button"),
    })
}
fn key_code(key: &KeyCode) -> Result<core::KeyCode> {
    use core::KeyCode as K;
    Ok(match key {
        KeyCode::Char(c) => K::Char(*c),
        KeyCode::Named(name) => match name.as_str() {
            "Enter" => K::Enter,
            "Escape" | "Esc" => K::Escape,
            "Tab" => K::Tab,
            "Backspace" => K::Backspace,
            "Up" => K::UpArrow,
            "Down" => K::DownArrow,
            "Left" => K::LeftArrow,
            "Right" => K::RightArrow,
            "Home" => K::Home,
            "End" => K::End,
            "PageUp" => K::PageUp,
            "PageDown" => K::PageDown,
            "Insert" => K::Insert,
            "Delete" => K::Delete,
            "CapsLock" => K::CapsLock,
            "ScrollLock" => K::ScrollLock,
            "NumLock" => K::NumLock,
            "PrintScreen" => K::PrintScreen,
            "Pause" => K::Pause,
            "Menu" => K::Menu,
            "LeftShift" => K::LeftShift,
            "RightShift" => K::RightShift,
            "LeftControl" => K::LeftControl,
            "RightControl" => K::RightControl,
            "LeftAlt" => K::LeftAlt,
            "RightAlt" => K::RightAlt,
            "KeypadLeft" => K::LeftArrow,
            "KeypadRight" => K::RightArrow,
            "KeypadUp" => K::UpArrow,
            "KeypadDown" => K::DownArrow,
            "KeypadPageUp" => K::PageUp,
            "KeypadPageDown" => K::PageDown,
            "KeypadHome" => K::Home,
            "KeypadEnd" => K::End,
            "KeypadInsert" => K::Insert,
            "KeypadDelete" => K::Delete,
            "KeypadBegin" => K::KeyPadBegin,
            "LeftSuper" => K::LeftWindows,
            "RightSuper" => K::RightWindows,
            "Hyper" => K::Hyper,
            "Meta" => K::Meta,
            name if name.starts_with('F') => {
                let number: u8 = name[1..].parse()?;
                ensure!((1..=35).contains(&number), "unsupported function key");
                K::Function(number)
            }
            _ => bail!("unsupported semantic key: {name}"),
        },
    })
}

fn resolved_color(
    value: core::color::ColorAttribute,
    palette: &core::color::ColorPalette,
    fallback: core::color::SrgbaTuple,
) -> Color {
    if value == core::color::ColorAttribute::Default {
        let (r, g, b, _) = fallback.as_rgba_u8();
        Color::Rgb([r, g, b])
    } else {
        color(value, palette)
    }
}

fn functional_code(key: &KeyCode) -> Option<u32> {
    let KeyCode::Named(name) = key else {
        return None;
    };
    let names = [
        "Keypad0",
        "Keypad1",
        "Keypad2",
        "Keypad3",
        "Keypad4",
        "Keypad5",
        "Keypad6",
        "Keypad7",
        "Keypad8",
        "Keypad9",
        "KeypadDecimal",
        "KeypadDivide",
        "KeypadMultiply",
        "KeypadSubtract",
        "KeypadAdd",
        "KeypadEnter",
        "KeypadEqual",
        "KeypadSeparator",
        "KeypadLeft",
        "KeypadRight",
        "KeypadUp",
        "KeypadDown",
        "KeypadPageUp",
        "KeypadPageDown",
        "KeypadHome",
        "KeypadEnd",
        "KeypadInsert",
        "KeypadDelete",
        "KeypadBegin",
        "MediaPlay",
        "MediaPause",
        "MediaPlayPause",
        "MediaReverse",
        "MediaStop",
        "MediaFastForward",
        "MediaRewind",
        "MediaNext",
        "MediaPrevious",
        "MediaRecord",
        "VolumeDown",
        "VolumeUp",
        "VolumeMute",
        "LeftShift",
        "LeftControl",
        "LeftAlt",
        "LeftSuper",
        "LeftHyper",
        "LeftMeta",
        "RightShift",
        "RightControl",
        "RightAlt",
        "RightSuper",
        "RightHyper",
        "RightMeta",
        "Level3Shift",
        "Level5Shift",
    ];
    names
        .iter()
        .position(|candidate| *candidate == name)
        .map(|index| 57399 + index as u32)
}

struct FrameBudget {
    remaining: usize,
}
impl Write for FrameBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("projected frame exceeds wire budget"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
