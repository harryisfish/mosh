use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use base64::Engine;
use unicode_segmentation::UnicodeSegmentation;

use crate::diagnostics::{self, Context as DiagnosticContext, Stage};
use crate::model::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Graphics {
    None,
    Kitty,
    Sixel,
}

impl Graphics {
    pub fn detect() -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        let program = std::env::var("TERM_PROGRAM").unwrap_or_default();
        if std::env::var_os("KITTY_WINDOW_ID").is_some()
            || std::env::var_os("WEZTERM_PANE").is_some()
            || term.contains("kitty")
            || matches!(program.as_str(), "WezTerm" | "ghostty")
        {
            Self::Kitty
        } else if ["foot", "mlterm", "contour"]
            .iter()
            .any(|name| term.contains(name))
        {
            Self::Sixel
        } else {
            Self::None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredictionPolicy {
    Never,
    Conservative,
    Adaptive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PredictionStamp {
    generation: u64,
    echo_enabled: bool,
    eligible: bool,
    alternate_screen: bool,
}

impl From<PredictionContext> for PredictionStamp {
    fn from(context: PredictionContext) -> Self {
        Self {
            generation: context.generation,
            echo_enabled: context.echo_enabled,
            eligible: context.eligible,
            alternate_screen: context.alternate_screen,
        }
    }
}

#[derive(Clone, Debug)]
struct PredictionToken {
    seq: u64,
    local_epoch: u64,
    remote: PredictionStamp,
    x: u16,
    y: u16,
    glyph: char,
    original: Option<Cell>,
    shown: bool,
    expires: Instant,
}

#[derive(Clone, Debug, Default)]
struct PredictionLedger {
    local_epoch: u64,
    remote: Option<PredictionStamp>,
    tokens: VecDeque<PredictionToken>,
    confidence: u8,
}

impl PredictionLedger {
    fn clear(&mut self) {
        self.tokens.clear();
        self.confidence = 0;
    }

    fn bump_epoch(&mut self) {
        self.local_epoch = self.local_epoch.saturating_add(1);
        self.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Placement {
    x: u16,
    y: u16,
    digest: Digest,
    z: i32,
}

struct PreparedGraphics {
    images: BTreeMap<Digest, Resource>,
    placements: Vec<Placement>,
}

pub struct Display<W> {
    writer: W,
    viewport: Size,
    epoch: u64,
    revision: u64,
    installed_echo_through: Option<u64>,
    effects: u64,
    screen: Option<Screen>,
    graphics: Graphics,
    policy: PredictionPolicy,
    predictions: PredictionLedger,
    image_ids: BTreeMap<Digest, u32>,
    placements: Vec<Placement>,
    next_image_id: u32,
    status: String,
    emitted_markers: VecDeque<ShellMarker>,
    checkpoint_required: bool,
}

impl<W> Display<W> {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    fn with_writer<T>(self, writer: T) -> (Display<T>, W) {
        (
            Display {
                writer,
                viewport: self.viewport,
                epoch: self.epoch,
                revision: self.revision,
                installed_echo_through: self.installed_echo_through,
                effects: self.effects,
                screen: self.screen,
                graphics: self.graphics,
                policy: self.policy,
                predictions: self.predictions,
                image_ids: self.image_ids,
                placements: self.placements,
                next_image_id: self.next_image_id,
                status: self.status,
                emitted_markers: self.emitted_markers,
                checkpoint_required: self.checkpoint_required,
            },
            self.writer,
        )
    }
}

impl Display<()> {
    pub fn bind<W: Write>(self, writer: W) -> Display<W> {
        self.with_writer(writer).0
    }
}

impl<W: Write> Display<W> {
    pub fn new(
        writer: W,
        graphics: Graphics,
        prediction: PredictionPolicy,
        viewport: Size,
    ) -> Result<Self> {
        viewport.validate()?;
        Ok(Self {
            writer,
            viewport,
            epoch: 0,
            revision: 0,
            installed_echo_through: None,
            effects: 0,
            screen: None,
            graphics,
            policy: prediction,
            predictions: PredictionLedger::default(),
            image_ids: BTreeMap::new(),
            placements: Vec::new(),
            next_image_id: 1,
            status: String::new(),
            emitted_markers: VecDeque::new(),
            checkpoint_required: true,
        })
    }

    pub fn set_viewport(&mut self, viewport: Size) -> Result<()> {
        viewport.validate()?;
        self.viewport = viewport;
        Ok(())
    }

    pub fn set_epoch(&mut self, epoch: u64) -> Result<()> {
        if epoch != self.epoch {
            self.clear_guesses()?;
            self.epoch = epoch;
            self.installed_echo_through = None;
            self.checkpoint_required = true;
            self.predictions.bump_epoch();
            self.predictions.remote = None;
        }
        Ok(())
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn installed_echo_through(&self) -> Option<u64> {
        self.installed_echo_through
    }
    pub fn needs_checkpoint(&self) -> bool {
        self.checkpoint_required
    }
    pub fn effects_applied(&self) -> u64 {
        self.effects
    }
    pub fn screen(&self) -> Option<&Screen> {
        self.screen.as_ref()
    }
    pub fn into_inner(self) -> W {
        self.writer
    }

    pub fn into_cached(mut self) -> (Display<()>, W) {
        self.predictions.clear();
        self.image_ids.clear();
        self.placements.clear();
        self.with_writer(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }

    pub fn set_effect_floor(&mut self, floor: u64) {
        self.effects = self.effects.max(floor);
    }

    pub fn install(
        &mut self,
        frame: &Frame,
        resources: &BTreeMap<Digest, Resource>,
    ) -> Result<bool> {
        let _operation = diagnostics::operation(
            Stage::InstallFrame,
            DiagnosticContext {
                epoch: frame.epoch,
                revision: frame.revision,
                ..Default::default()
            },
        );
        if frame.epoch != self.epoch || frame.revision <= self.revision {
            return Ok(false);
        }
        if frame.base.is_some_and(|base| base != self.revision)
            || (self.checkpoint_required && frame.base.is_some())
        {
            self.checkpoint_required = true;
            return Ok(false);
        }
        let mut screen = self.assemble(frame, resources)?;
        let stamp = PredictionStamp::from(screen.prediction);
        let mode_changed = self.predictions.remote.is_some_and(|old| old != stamp);
        let mut repaint = BTreeSet::new();
        for token in &self.predictions.tokens {
            if token.shown {
                repaint.insert(token.y);
            }
        }
        if mode_changed {
            self.predictions.bump_epoch();
        } else {
            while self
                .predictions
                .tokens
                .front()
                .is_some_and(|token| token.seq <= frame.echo_through)
            {
                let token = self.predictions.tokens.pop_front().unwrap();
                let new_glyph = char_at(&screen, token.x, token.y);
                let original_glyph = token.original.as_ref().and_then(cell_glyph);
                if token.local_epoch == self.predictions.local_epoch
                    && new_glyph == Some(token.glyph)
                    && original_glyph != Some(token.glyph)
                {
                    self.predictions.confidence =
                        self.predictions.confidence.saturating_add(1).min(8);
                } else if new_glyph != Some(token.glyph) {
                    self.predictions.confidence = 0;
                    self.predictions.tokens.clear();
                    break;
                }
            }
            self.predictions
                .tokens
                .retain(|token| token.expires > Instant::now());
        }
        self.predictions.remote = Some(stamp);
        let checkpoint = frame.base.is_none()
            || self
                .screen
                .as_ref()
                .is_none_or(|old| old.size != screen.size);
        if checkpoint || self.graphics == Graphics::Sixel {
            repaint.extend(0..screen.size.rows);
        } else {
            repaint.extend(frame.rows.iter().map(|patch| patch.y));
        }
        if !self.status.is_empty() {
            repaint.insert(0);
        }
        if self
            .predictions
            .tokens
            .iter()
            .any(|token| token.remote != stamp)
        {
            self.predictions.clear();
        }
        self.paint(
            &screen,
            repaint,
            checkpoint || self.graphics == Graphics::Sixel,
            true,
        )?;
        screen
            .resources
            .retain(|digest, _| frame.resources.iter().any(|meta| &meta.digest == digest));
        self.screen = Some(screen);
        self.revision = frame.revision;
        self.installed_echo_through = Some(frame.echo_through);
        self.checkpoint_required = false;
        Ok(true)
    }

    fn assemble(&self, frame: &Frame, resources: &BTreeMap<Digest, Resource>) -> Result<Screen> {
        frame.size.validate()?;
        ensure!(
            frame.cursor.x < frame.size.cols
                && frame.cursor.y < frame.size.rows
                && frame.cursor.shape <= 3,
            "invalid frame cursor"
        );
        ensure!(
            frame.title.len() <= 4096 && safe_text(&frame.title),
            "invalid terminal title"
        );
        ensure!(
            frame
                .directory
                .as_ref()
                .is_none_or(|s| s.len() <= 8192 && safe_text(s)),
            "invalid terminal directory"
        );
        ensure!(frame.markers.len() <= 4096, "too many shell markers");
        for marker in &frame.markers {
            ensure!(
                marker.column < frame.size.cols
                    && ["A", "B", "C", "D", "Prompt", "Input", "Output", "End"]
                        .contains(&marker.kind.as_str()),
                "invalid shell marker"
            );
        }
        ensure!(
            frame.rows.len() <= usize::from(frame.size.rows),
            "too many frame rows"
        );
        let mut rows = match frame.base {
            Some(_) => {
                let old = self.screen.as_ref().context("delta has no base screen")?;
                ensure!(old.size == frame.size, "delta changes terminal size");
                old.rows.clone()
            }
            None => vec![Row::default(); usize::from(frame.size.rows)],
        };
        let mut seen = BTreeSet::new();
        for patch in &frame.rows {
            ensure!(
                patch.y < frame.size.rows && seen.insert(patch.y),
                "invalid or duplicate frame row"
            );
            rows[usize::from(patch.y)] = patch.row.clone();
        }
        ensure!(
            frame.base.is_some() || seen.len() == usize::from(frame.size.rows),
            "checkpoint omits a row"
        );
        let mut pinned = BTreeMap::new();
        let mut bytes = 0usize;
        ensure!(frame.resources.len() <= 4096, "too many frame resources");
        for meta in &frame.resources {
            ensure!(
                !pinned.contains_key(&meta.digest),
                "duplicate frame resource"
            );
            let resource = resources
                .get(&meta.digest)
                .context("frame resource is not available")?;
            ensure!(resource.meta == *meta, "frame resource metadata mismatch");
            resource.validate()?;
            bytes = bytes
                .checked_add(resource.rgba.len())
                .context("resource byte overflow")?;
            ensure!(bytes <= MAX_RESOURCES, "frame resources exceed limit");
            pinned.insert(meta.digest, resource.clone());
        }
        let mut tiles = 0usize;
        for (y, row) in rows.iter().enumerate() {
            row.validate_wrap(frame.size, y as u16)?;
            ensure!(
                row.cells.len() <= usize::from(frame.size.cols),
                "too many cells in row"
            );
            let mut end = 0u16;
            for cell in &row.cells {
                ensure!(
                    (1..=2).contains(&cell.width) && cell.x >= end,
                    "overlapping or invalid cell width"
                );
                end = cell
                    .x
                    .checked_add(u16::from(cell.width))
                    .context("cell column overflow")?;
                ensure!(
                    end <= frame.size.cols
                        && !cell.text.is_empty()
                        && cell.text.len() <= 1024
                        && safe_text(&cell.text)
                        && cell.text.graphemes(true).count() == 1,
                    "invalid cell text or column"
                );
                ensure!(
                    cell.style.flags < 256 && cell.style.underline <= 5,
                    "invalid cell attributes"
                );
                if let Some(link) = &cell.style.hyperlink {
                    ensure!(
                        link.params.len() <= 4096
                            && link.url.len() <= 8192
                            && safe_text(&link.params)
                            && safe_text(&link.url),
                        "invalid hyperlink"
                    );
                }
                ensure!(
                    cell.images.len() <= MAX_CELL_IMAGES,
                    "too many image tiles in a cell"
                );
                tiles += cell.images.len();
                ensure!(tiles <= MAX_IMAGE_TILES, "too many image placements");
                for tile in &cell.images {
                    let resource = pinned
                        .get(&tile.resource)
                        .context("image tile refers to missing resource")?;
                    let [x, y, width, height] = tile.source;
                    ensure!(
                        width > 0
                            && height > 0
                            && u64::from(x) + u64::from(width) <= u64::from(resource.meta.width)
                            && u64::from(y) + u64::from(height) <= u64::from(resource.meta.height),
                        "image crop is out of bounds"
                    );
                }
            }
        }
        Ok(Screen {
            size: frame.size,
            rows,
            cursor: frame.cursor,
            title: frame.title.clone(),
            directory: frame.directory.clone(),
            markers: frame.markers.clone(),
            reverse_video: frame.reverse_video,
            prediction: frame.prediction,
            resources: pinned,
        })
    }

    pub fn predict(&mut self, seq: u64, event: &InputEvent) -> Result<()> {
        let InputEvent::Key(key) = event else {
            self.invalidate_predictions()?;
            return Ok(());
        };
        if key.phase == KeyPhase::Release {
            return Ok(());
        }
        let KeyCode::Char(text) = key.code else {
            self.invalidate_predictions()?;
            return Ok(());
        };
        if !text.is_ascii()
            || text.is_ascii_control()
            || key.modifiers & !1 != 0
            || key.text.as_ref().is_some_and(|s| s != &text.to_string())
        {
            self.invalidate_predictions()?;
            return Ok(());
        }
        if self.policy == PredictionPolicy::Never {
            return Ok(());
        }
        let Some(screen) = &self.screen else {
            return Ok(());
        };
        if !screen.prediction.eligible || screen.size.cols != self.viewport.cols {
            return Ok(());
        };
        if self.predictions.tokens.len() >= 256 {
            self.clear_guesses()?;
            return Ok(());
        }
        let (x, y) = self
            .predictions
            .tokens
            .back()
            .map(|token| (token.x + 1, token.y))
            .unwrap_or((screen.cursor.x, screen.cursor.y));
        if x >= screen.size.cols.saturating_sub(1) || y >= self.viewport.rows {
            return Ok(());
        }
        if screen.rows.get(usize::from(y)).is_none_or(|row| {
            row.cells
                .iter()
                .any(|cell| cell.x == x && (!cell.images.is_empty() || cell.width != 1))
        }) {
            return Ok(());
        }
        let shown = screen.prediction.echo_enabled
            || (self.policy == PredictionPolicy::Adaptive && self.predictions.confidence >= 3);
        let token = PredictionToken {
            seq,
            local_epoch: self.predictions.local_epoch,
            remote: PredictionStamp::from(screen.prediction),
            x,
            y,
            glyph: text,
            original: cell_at(screen, x, y).cloned(),
            shown,
            expires: Instant::now() + Duration::from_millis(750),
        };
        self.predictions.tokens.push_back(token);
        if shown {
            let screen = screen.clone();
            self.paint(&screen, BTreeSet::from([y]), false, false)?;
        }
        Ok(())
    }

    pub fn tick(&mut self) -> Result<()> {
        if self
            .predictions
            .tokens
            .iter()
            .any(|token| token.expires <= Instant::now())
        {
            self.clear_guesses()?;
        }
        Ok(())
    }

    fn clear_guesses(&mut self) -> Result<()> {
        let rows: BTreeSet<_> = self
            .predictions
            .tokens
            .iter()
            .filter(|token| token.shown)
            .map(|token| token.y)
            .collect();
        self.predictions.clear();
        if !rows.is_empty()
            && let Some(screen) = self.screen.clone()
        {
            self.paint(&screen, rows, false, false)?;
        }
        Ok(())
    }

    fn invalidate_predictions(&mut self) -> Result<()> {
        self.clear_guesses()?;
        self.predictions.bump_epoch();
        Ok(())
    }

    fn overlay_cursor(&self, screen: &Screen) -> (u16, u16) {
        self.predictions
            .tokens
            .iter()
            .rev()
            .find(|token| token.shown)
            .map(|token| (token.x + 1, token.y))
            .unwrap_or((screen.cursor.x, screen.cursor.y))
    }

    pub fn write_status(&mut self, text: &str) -> Result<()> {
        ensure!(
            text.len() <= 4096 && safe_text(text),
            "invalid connection status"
        );
        let mut rows: BTreeSet<_> = self
            .predictions
            .tokens
            .iter()
            .filter(|token| token.shown)
            .map(|token| token.y)
            .collect();
        rows.insert(0);
        self.predictions.clear();
        self.status = text.into();
        if let Some(screen) = self.screen.clone() {
            self.paint(&screen, rows, false, false)?;
        } else {
            let text: String = text
                .chars()
                .map(|ch| if ch.is_ascii() { ch } else { '?' })
                .take(usize::from(self.viewport.cols))
                .collect();
            write!(
                self.writer,
                "\x1b[?2026h\x1b[?7l\x1b[H\x1b[2K\x1b[0;7m{text}\x1b[0m\x1b[?2026l"
            )?;
            self.writer.flush()?;
        }
        Ok(())
    }

    pub fn redraw_cached(&mut self, size: Size) -> Result<()> {
        self.set_viewport(size)?;
        self.predictions.clear();
        let Some(screen) = self.screen.clone() else {
            return self.write_status(&self.status.clone());
        };
        self.image_ids.clear();
        self.placements.clear();
        self.paint(&screen, BTreeSet::new(), true, false)
    }

    fn paint(
        &mut self,
        source: &Screen,
        mut rows: BTreeSet<u16>,
        reset: bool,
        metadata: bool,
    ) -> Result<()> {
        let mut screen = source.clone();
        screen.size = self.viewport;
        screen.rows.truncate(usize::from(self.viewport.rows));
        for (y, row) in screen.rows.iter_mut().enumerate() {
            if source.size.cols != self.viewport.cols
                || y + 1 >= usize::from(source.size.rows.min(self.viewport.rows))
                || (y == 0 && !self.status.is_empty())
            {
                row.wrap = RowWrap::Hard;
            }
            row.cells.retain(|cell| cell.x < self.viewport.cols);
            for cell in &mut row.cells {
                if cell.x + u16::from(cell.width) > self.viewport.cols {
                    cell.width = 1;
                    cell.text = " ".into();
                }
            }
        }
        screen
            .rows
            .resize_with(usize::from(self.viewport.rows), Row::default);
        screen.cursor.x = screen.cursor.x.min(self.viewport.cols - 1);
        screen.cursor.y = screen.cursor.y.min(self.viewport.rows - 1);
        let graphics = prepare_graphics(&screen, self.graphics)?;
        if reset || self.graphics == Graphics::Sixel {
            rows.extend(0..self.viewport.rows);
        }
        rows.retain(|y| *y < self.viewport.rows);
        for start in 0..self.viewport.rows {
            if !rows.contains(&start) {
                continue;
            }
            let connected = |y: u16| {
                screen.rows[usize::from(y)].wrap.is_soft()
                    || self
                        .screen
                        .as_ref()
                        .and_then(|old| old.rows.get(usize::from(y)))
                        .is_some_and(|row| row.wrap.is_soft())
            };
            let mut y = start;
            while y > 0 && connected(y - 1) {
                y -= 1;
                rows.insert(y);
            }
            y = start;
            while y + 1 < self.viewport.rows && connected(y) {
                y += 1;
                rows.insert(y);
            }
        }
        self.writer.write_all(b"\x1b[?2026h\x1b[?25l\x1b[?7l")?;
        if reset {
            self.writer.write_all(b"\x1b[0m\x1b[2J")?;
        }
        self.writer.write_all(if screen.reverse_video {
            b"\x1b[?5h"
        } else {
            b"\x1b[?5l"
        })?;
        if reset && !metadata && self.graphics == Graphics::Kitty {
            self.writer.write_all(b"\x1b_Ga=d,d=A,q=2\x1b\\")?;
        }
        for &y in &rows {
            cup(&mut self.writer, 0, y)?;
            self.writer.write_all(b"\x1b]8;;\x1b\\\x1b[0m\x1b[2K")?;
        }
        for y in rows {
            cup(&mut self.writer, 0, y)?;
            if y > 0 && screen.rows[usize::from(y - 1)].wrap == RowWrap::WidePadding {
                self.writer.write_all(b"\x1b[2K")?;
            }
            self.writer.write_all(b"\x1b[?7h")?;
            render_row(&mut self.writer, &screen, y)?;
            match screen.rows[usize::from(y)].wrap {
                RowWrap::Hard => (),
                RowWrap::Soft => self.writer.write_all(b" ")?,
                RowWrap::WidePadding => {
                    let padding = screen.rows[usize::from(y)].cells.last().unwrap();
                    render_style(&mut self.writer, &padding.style)?;
                    self.writer.write_all("界".as_bytes())?;
                }
            }
            self.writer.write_all(b"\x1b[?7l")?;
        }
        self.render_graphics(&graphics)?;
        if metadata {
            self.render_metadata(source)?;
        } else if reset {
            write!(
                self.writer,
                "\x1b]2;{}\x1b\\\x1b]7;{}\x1b\\",
                screen.title,
                screen.directory.as_deref().unwrap_or("")
            )?;
        }
        render_guesses(
            &mut self.writer,
            &self.predictions.tokens,
            self.predictions.local_epoch,
            &screen,
        )?;
        if !self.status.is_empty() {
            render_status(&mut self.writer, &screen, &self.status)?;
        }
        let cursor = self.overlay_cursor(&screen);
        render_cursor(&mut self.writer, &screen, cursor)?;
        self.writer.write_all(b"\x1b[?2026l")?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn apply_effect(
        &mut self,
        effect: &Effect,
        allow_clipboard_read: bool,
    ) -> Result<Option<InputEvent>> {
        if effect.id <= self.effects {
            return Ok(None);
        }
        ensure!(effect.id == self.effects + 1, "effect sequence has a gap");
        ensure!(
            effect.after <= self.revision,
            "effect display barrier has not been installed"
        );
        let mut response = None;
        match &effect.kind {
            EffectKind::Bell => self.writer.write_all(b"\x07")?,
            EffectKind::ClipboardWrite { selection, data } => {
                validate_selection(selection)?;
                ensure!(
                    data.len() <= MAX_PENDING_INPUT / 2,
                    "clipboard write exceeds limit"
                );
                write!(
                    self.writer,
                    "\x1b]52;{};{}\x1b\\",
                    selection,
                    base64::engine::general_purpose::STANDARD.encode(data.as_bytes())
                )?;
            }
            EffectKind::ClipboardRead { query, selection } => {
                validate_selection(selection)?;
                if allow_clipboard_read {
                    write!(self.writer, "\x1b]52;{selection};?\x1b\\")?;
                } else {
                    response = Some(InputEvent::QueryResult {
                        query: *query,
                        data: None,
                    });
                }
            }
            EffectKind::Notification { title, body } => {
                ensure!(
                    title.len() <= 4096
                        && body.len() <= 8192
                        && safe_text(title)
                        && safe_text(body)
                        && !title.contains(';'),
                    "invalid terminal notification"
                );
                write!(self.writer, "\x1b]777;notify;{title};{body}\x1b\\")?;
            }
        }
        self.writer.flush()?;
        self.effects = effect.id;
        Ok(response)
    }

    fn render_metadata(&mut self, screen: &Screen) -> Result<()> {
        if self
            .screen
            .as_ref()
            .is_none_or(|old| old.title != screen.title)
        {
            write!(self.writer, "\x1b]2;{}\x1b\\", screen.title)?;
        }
        if self.screen.as_ref().and_then(|old| old.directory.as_ref()) != screen.directory.as_ref()
        {
            write!(
                self.writer,
                "\x1b]7;{}\x1b\\",
                screen.directory.as_deref().unwrap_or("")
            )?;
        }
        for marker in &screen.markers {
            if self.emitted_markers.contains(marker) {
                continue;
            }
            if let Some(y) = screen
                .rows
                .iter()
                .position(|row| row.stable_id == marker.row)
                .filter(|y| {
                    *y < usize::from(self.viewport.rows) && marker.column < self.viewport.cols
                })
            {
                cup(&mut self.writer, marker.column, y as u16)?;
                let kind = match marker.kind.as_str() {
                    "Prompt" => "A",
                    "Input" => "B",
                    "Output" => "C",
                    "End" => "D",
                    kind => kind,
                };
                if let Some(status) = marker.status {
                    write!(self.writer, "\x1b]133;{kind};{status}\x1b\\")?;
                } else {
                    write!(self.writer, "\x1b]133;{kind}\x1b\\")?;
                }
                self.emitted_markers.push_back(marker.clone());
                while self.emitted_markers.len() > 4096 {
                    self.emitted_markers.pop_front();
                }
            }
        }
        Ok(())
    }

    fn render_graphics(&mut self, graphics: &PreparedGraphics) -> Result<()> {
        match self.graphics {
            Graphics::None => (),
            Graphics::Kitty => {
                let dead: Vec<_> = self
                    .image_ids
                    .keys()
                    .filter(|digest| !graphics.images.contains_key(*digest))
                    .copied()
                    .collect();
                for digest in dead {
                    let id = self.image_ids.remove(&digest).unwrap();
                    write!(self.writer, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
                }
                for (digest, resource) in &graphics.images {
                    if self.image_ids.contains_key(digest) {
                        continue;
                    }
                    let id = self.next_image_id;
                    self.next_image_id = self
                        .next_image_id
                        .checked_add(1)
                        .context("local image identifiers exhausted")?;
                    upload_kitty(&mut self.writer, id, resource)?;
                    self.image_ids.insert(*digest, id);
                }
                self.writer.write_all(b"\x1b_Ga=d,d=a,q=2\x1b\\")?;
                for (index, placement) in graphics.placements.iter().enumerate() {
                    cup(&mut self.writer, placement.x, placement.y)?;
                    let id = self.image_ids[&placement.digest];
                    let p = index + 1;
                    write!(
                        self.writer,
                        "\x1b_Ga=p,i={id},p={p},c=1,r=1,z={},C=1,q=2\x1b\\",
                        placement.z
                    )?;
                }
                self.placements = graphics.placements.clone();
            }
            Graphics::Sixel => {
                for placement in &graphics.placements {
                    cup(&mut self.writer, placement.x, placement.y)?;
                    self.writer.write_all(b"\x1b7")?;
                    write_sixel(&mut self.writer, &graphics.images[&placement.digest])?;
                    self.writer.write_all(b"\x1b8")?;
                }
                self.placements = graphics.placements.clone();
            }
        }
        Ok(())
    }
}

fn safe_text(text: &str) -> bool {
    text.chars().all(|ch| !ch.is_control())
}

fn validate_selection(selection: &str) -> Result<()> {
    ensure!(
        selection.len() <= 16 && selection.bytes().all(|b| b"cpsq01234567".contains(&b)),
        "invalid clipboard selection"
    );
    Ok(())
}

fn char_at(screen: &Screen, x: u16, y: u16) -> Option<char> {
    cell_at(screen, x, y).and_then(cell_glyph)
}

fn cell_at(screen: &Screen, x: u16, y: u16) -> Option<&Cell> {
    screen
        .rows
        .get(usize::from(y))?
        .cells
        .iter()
        .find(|cell| cell.x == x && cell.width == 1)
}

fn cell_glyph(cell: &Cell) -> Option<char> {
    let mut chars = cell.text.chars();
    let ch = chars.next()?;
    chars.next().is_none().then_some(ch)
}

fn cup(writer: &mut impl Write, x: u16, y: u16) -> Result<()> {
    write!(writer, "\x1b[{};{}H", y + 1, x + 1)?;
    Ok(())
}

fn render_row(writer: &mut impl Write, screen: &Screen, y: u16) -> Result<()> {
    let row = screen
        .rows
        .get(usize::from(y))
        .context("render row out of range")?;
    let mut x = 0;
    let mut style = None;
    let blank = Style::default();
    let end = screen.size.cols - u16::from(row.wrap == RowWrap::WidePadding);
    for cell in &row.cells {
        if cell.x >= end {
            break;
        }
        if cell.x > x {
            render_style(writer, &blank)?;
            for _ in x..cell.x {
                writer.write_all(b" ")?;
            }
            style = Some(&blank);
        }
        if style != Some(&cell.style) {
            render_style(writer, &cell.style)?;
            style = Some(&cell.style);
        }
        writer.write_all(cell.text.as_bytes())?;
        x = cell.x + u16::from(cell.width);
    }
    if x < end {
        render_style(writer, &blank)?;
        for _ in x..end {
            writer.write_all(b" ")?;
        }
    }
    writer.write_all(b"\x1b]8;;\x1b\\\x1b[0m")?;
    Ok(())
}

fn render_style(writer: &mut impl Write, style: &Style) -> Result<()> {
    writer.write_all(b"\x1b]8;;\x1b\\\x1b[0")?;
    for (bit, sgr) in [
        (1, 1),
        (2, 2),
        (4, 3),
        (8, 5),
        (16, 7),
        (32, 8),
        (64, 9),
        (128, 53),
    ] {
        if style.flags & bit != 0 {
            write!(writer, ";{sgr}")?;
        }
    }
    if style.underline != 0 {
        write!(writer, ";4:{}", style.underline)?;
    }
    for (color, code) in [(style.fg, 38), (style.bg, 48), (style.underline_color, 58)] {
        if let Color::Rgb([red, green, blue]) = color {
            write!(writer, ";{code};2;{red};{green};{blue}")?;
        }
    }
    writer.write_all(b"m")?;
    if let Some(link) = &style.hyperlink {
        write!(writer, "\x1b]8;{};{}\x1b\\", link.params, link.url)?;
    }
    Ok(())
}

fn render_cursor(writer: &mut impl Write, screen: &Screen, position: (u16, u16)) -> Result<()> {
    cup(
        writer,
        position.0.min(screen.size.cols - 1),
        position.1.min(screen.size.rows - 1),
    )?;
    let base = match screen.cursor.shape {
        2 => 3,
        3 => 5,
        _ => 1,
    };
    let style = base + u8::from(!screen.cursor.blink);
    write!(writer, "\x1b[{style} q")?;
    writer.write_all(if screen.cursor.visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    })?;
    Ok(())
}

fn render_status(writer: &mut impl Write, screen: &Screen, text: &str) -> Result<()> {
    let text: String = text
        .chars()
        .map(|ch| if ch.is_ascii() { ch } else { '?' })
        .take(usize::from(screen.size.cols))
        .collect();
    let x = screen.size.cols - text.len() as u16;
    cup(writer, x, 0)?;
    write!(writer, "\x1b]8;;\x1b\\\x1b[0;7m{text}\x1b[0m")?;
    Ok(())
}

fn prepare_graphics(screen: &Screen, graphics: Graphics) -> Result<PreparedGraphics> {
    let mut prepared = PreparedGraphics {
        images: BTreeMap::new(),
        placements: Vec::new(),
    };
    if graphics == Graphics::None {
        return Ok(prepared);
    }
    let cell_width = (screen.size.pixel_width / u32::from(screen.size.cols)).max(1);
    let cell_height = (screen.size.pixel_height / u32::from(screen.size.rows)).max(1);
    let cell_width = if screen.size.pixel_width == 0 {
        10
    } else {
        cell_width
    };
    let cell_height = if screen.size.pixel_height == 0 {
        20
    } else {
        cell_height
    };
    let tile_bytes = u64::from(cell_width) * u64::from(cell_height) * 4;
    ensure!(
        tile_bytes <= MAX_RESOURCE as u64,
        "local graphics cell exceeds image limit"
    );
    let mut total_bytes = 0usize;
    for (y, row) in screen.rows.iter().enumerate() {
        for cell in &row.cells {
            for tile in &cell.images {
                let image = &screen.resources[&tile.resource];
                let mut rgba = vec![0; tile_bytes as usize];
                let [left, top, right, bottom] = tile.padding.map(u32::from);
                let width = cell_width.saturating_sub(left.saturating_add(right));
                let height = cell_height.saturating_sub(top.saturating_add(bottom));
                let [source_x, source_y, source_width, source_height] = tile.source;
                if width == 0 || height == 0 {
                    continue;
                }
                for target_y in 0..height {
                    let sy = source_y
                        + (u64::from(target_y) * u64::from(source_height) / u64::from(height))
                            as u32;
                    for target_x in 0..width {
                        let sx = source_x
                            + (u64::from(target_x) * u64::from(source_width) / u64::from(width))
                                as u32;
                        let src = ((u64::from(sy) * u64::from(image.meta.width) + u64::from(sx))
                            * 4) as usize;
                        let dst = ((u64::from(target_y + top) * u64::from(cell_width)
                            + u64::from(target_x + left))
                            * 4) as usize;
                        rgba[dst..dst + 4].copy_from_slice(&image.rgba[src..src + 4]);
                    }
                }
                let digest = resource_digest(cell_width, cell_height, &rgba);
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    prepared.images.entry(digest)
                {
                    total_bytes += rgba.len();
                    ensure!(
                        total_bytes <= MAX_RESOURCES,
                        "local graphics cache exceeds limit"
                    );
                    entry.insert(Resource {
                        meta: ResourceMeta {
                            digest,
                            width: cell_width,
                            height: cell_height,
                            bytes: rgba.len() as u32,
                        },
                        rgba: Arc::from(rgba),
                    });
                }
                prepared.placements.push(Placement {
                    x: cell.x,
                    y: y as u16,
                    digest,
                    z: tile.z,
                });
            }
        }
    }
    prepared.placements.sort_by_key(|placement| placement.z);
    Ok(prepared)
}

fn upload_kitty(writer: &mut impl Write, id: u32, image: &Resource) -> Result<()> {
    let mut chunks = image.rgba.chunks(3072).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            write!(
                writer,
                "\x1b_Ga=t,f=32,s={},v={},i={id},q=2,m={more};",
                image.meta.width, image.meta.height
            )?;
            first = false;
        } else {
            write!(writer, "\x1b_Gm={more};")?;
        }
        writer.write_all(
            base64::engine::general_purpose::STANDARD
                .encode(chunk)
                .as_bytes(),
        )?;
        writer.write_all(b"\x1b\\")?;
    }
    Ok(())
}

fn write_sixel(writer: &mut impl Write, image: &Resource) -> Result<()> {
    let width = image.meta.width as usize;
    let height = image.meta.height as usize;
    write!(writer, "\x1bP0;1q\"1;1;{width};{height}")?;
    let mut palette = BTreeSet::new();
    let mut pixels = Vec::with_capacity(width * height);
    for rgba in image.rgba.as_chunks::<4>().0 {
        if rgba[3] < 128 {
            pixels.push(None);
            continue;
        }
        let red = (u16::from(rgba[0]) * 5 + 127) / 255;
        let green = (u16::from(rgba[1]) * 5 + 127) / 255;
        let blue = (u16::from(rgba[2]) * 5 + 127) / 255;
        let index = red * 36 + green * 6 + blue;
        palette.insert(index);
        pixels.push(Some(index));
    }
    for index in &palette {
        write!(
            writer,
            "#{index};2;{};{};{}",
            (index / 36) * 20,
            ((index / 6) % 6) * 20,
            (index % 6) * 20
        )?;
    }
    for y in (0..height).step_by(6) {
        let mut first = true;
        for index in &palette {
            let mut bytes = Vec::with_capacity(width);
            for x in 0..width {
                let mut bits = 0u8;
                for row in 0..6 {
                    if y + row < height && pixels[(y + row) * width + x] == Some(*index) {
                        bits |= 1 << row;
                    }
                }
                bytes.push(bits + 63);
            }
            if bytes.iter().all(|byte| *byte == 63) {
                continue;
            }
            if !first {
                writer.write_all(b"$")?;
            }
            first = false;
            write!(writer, "#{index}")?;
            let mut offset = 0;
            while offset < bytes.len() {
                let byte = bytes[offset];
                let run = bytes[offset..]
                    .iter()
                    .take_while(|&&next| next == byte)
                    .count();
                if run > 3 {
                    write!(writer, "!{run}{}", char::from(byte))?;
                } else {
                    for _ in 0..run {
                        writer.write_all(&[byte])?;
                    }
                }
                offset += run;
            }
        }
        if y + 6 < height {
            writer.write_all(b"-")?;
        }
    }
    writer.write_all(b"\x1b\\")?;
    Ok(())
}

fn render_guesses(
    writer: &mut impl Write,
    tokens: &VecDeque<PredictionToken>,
    local_epoch: u64,
    screen: &Screen,
) -> Result<()> {
    let stamp = PredictionStamp::from(screen.prediction);
    for token in tokens {
        if !token.shown
            || token.local_epoch != local_epoch
            || token.remote != stamp
            || token.x >= screen.size.cols.saturating_sub(1)
            || token.y >= screen.size.rows
        {
            continue;
        }
        cup(writer, token.x, token.y)?;
        writer.write_all(b"\x1b]8;;\x1b\\\x1b[0;4m")?;
        write!(writer, "{}", token.glyph)?;
    }
    writer.write_all(b"\x1b[0m")?;
    Ok(())
}
