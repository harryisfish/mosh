use anyhow::{Context, Result};
use base64::Engine;
use mosh_rs::model::*;
use mosh_rs::terminal::TerminalModel;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct AllocationProbe;
static MEASURE_ALLOCATIONS: AtomicBool = AtomicBool::new(false);
static MAX_ALLOCATION: AtomicUsize = AtomicUsize::new(0);

fn record_allocation(size: usize) {
    if MEASURE_ALLOCATIONS.load(Ordering::Relaxed) {
        MAX_ALLOCATION.fetch_max(size, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for AllocationProbe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation(size);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: AllocationProbe = AllocationProbe;

fn key(code: KeyCode) -> InputEvent {
    InputEvent::Key(Key {
        code,
        modifiers: 0,
        phase: KeyPhase::Press,
        shifted: None,
        base: None,
        text: None,
    })
}
fn model() -> TerminalModel {
    TerminalModel::new(
        Size {
            cols: 20,
            rows: 4,
            pixel_width: 200,
            pixel_height: 80,
        },
        true,
    )
    .unwrap()
}
fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|arg| arg == "--combining-transcript")
    {
        return verify_combining_transcript(&arguments[1..]);
    }
    if arguments.first().is_some_and(|arg| arg == "--soft-wrap") {
        return verify_soft_wrap_metadata();
    }
    verify_soft_wrap_metadata()?;
    verify_default_colors()?;
    verify_terminal_boundaries()?;
    verify_png_metadata()?;
    verify_projection_regressions()?;
    verify_combining_regressions()?;
    verify_scrollback_wrap()?;
    let maximum = Size {
        cols: 512,
        rows: 256,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut large = TerminalModel::new(maximum, false)?;
    let mut paint = String::from("\x1b[38;2;12;34;56m");
    for row in 1..=256 {
        paint.push_str(&format!("\x1b[{row};1H"));
        paint.push_str(&"A".repeat(512));
    }
    large.advance(paint.as_bytes())?;
    let screen = large.snapshot()?;
    assert_eq!(screen.rows.len(), 256);
    assert!(screen.rows.iter().all(|row| {
        row.cells.len() == 512
            && row
                .cells
                .iter()
                .all(|cell| cell.text == "A" && cell.style.fg == Color::Rgb([12, 34, 56]))
    }));
    let frame = Frame {
        epoch: 1,
        revision: 1,
        base: None,
        echo_through: 0,
        size: maximum,
        rows: screen
            .rows
            .into_iter()
            .enumerate()
            .map(|(y, row)| RowPatch { y: y as u16, row })
            .collect(),
        cursor: screen.cursor,
        title: screen.title,
        directory: screen.directory,
        markers: screen.markers,
        reverse_video: screen.reverse_video,
        prediction: screen.prediction,
        resources: Vec::new(),
    };
    let rows_bytes = frame
        .rows
        .iter()
        .map(|patch| postcard::to_io(&patch.row, CountBytes::default()).map(|count| count.0))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .sum::<usize>();
    let legacy_rows_bytes = frame
        .rows
        .iter()
        .map(|patch| {
            postcard::to_io(
                &(&patch.row.cells, patch.row.stable_id),
                CountBytes::default(),
            )
            .map(|count| count.0)
        })
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .sum::<usize>();
    assert_eq!(rows_bytes - legacy_rows_bytes, 256);
    let bytes = postcard::to_io(&frame, CountBytes::default())?.0;
    assert!(bytes <= MAX_FRAME);
    println!(
        "maximum 512x256 full RGB text frame: {bytes} postcard bytes, {rows_bytes} Row bytes including 256 wrap metadata bytes"
    );
    drop(frame);
    drop(large);

    let mut fallback = TerminalModel::new(
        Size {
            cols: 20,
            rows: 4,
            pixel_width: 0,
            pixel_height: 0,
        },
        false,
    )?;
    fallback.advance(b"\x1b_Ga=T,f=24,s=1,v=1,i=1;/wAA\x1b\\")?;
    assert_eq!(
        fallback.snapshot()?.rows[0].cells[0].images[0].padding,
        [0, 0, 9, 19]
    );
    let mut explicit = TerminalModel::new(
        Size {
            cols: 20,
            rows: 4,
            pixel_width: 20,
            pixel_height: 4,
        },
        false,
    )?;
    explicit.advance(b"\x1b_Ga=T,f=24,s=1,v=1,i=1;/wAA\x1b\\")?;
    assert_eq!(
        explicit.snapshot()?.rows[0].cells[0].images[0].padding,
        [0, 0, 0, 0]
    );
    println!("zero-pixel fallback and explicit pixel dimensions passed");

    let mut term = model();
    term.advance(
        b"\x1b[38;2;12;34;56mR\x1b[0m \x1b]8;id=one;https://example.com\x1b\\L\x1b]8;;\x1b\\ e",
    )?;
    term.advance("\u{301}".as_bytes())?;
    let screen = term.snapshot()?;
    assert_eq!(screen.rows[0].cells[0].style.fg, Color::Rgb([12, 34, 56]));
    assert_eq!(
        screen.rows[0].cells[2].style.hyperlink.as_ref().unwrap(),
        &Hyperlink {
            params: "id=one".into(),
            url: "https://example.com".into()
        }
    );
    assert_eq!(screen.rows[0].cells[4].text, "e\u{301}");
    assert_eq!(screen.cursor.x, 5);
    assert_eq!(
        term.encode_input(&key(KeyCode::Named("Up".into())))?,
        b"\x1b[A"
    );
    term.advance(b"\x1b[?1h")?;
    assert_eq!(
        term.encode_input(&key(KeyCode::Named("Up".into())))?,
        b"\x1bOA"
    );
    term.advance(b"\x1b[?2004h")?;
    assert_eq!(
        term.encode_input(&InputEvent::Paste("a\n\x1b[201~b".into()))?,
        b"\x1b[200~a\nb\x1b[201~"
    );
    term.advance(b"\x1b[>31u")?;
    for (phase, number) in [
        (KeyPhase::Press, 1),
        (KeyPhase::Repeat, 2),
        (KeyPhase::Release, 3),
    ] {
        let event = InputEvent::Key(Key {
            code: KeyCode::Char('a'),
            modifiers: 1,
            phase,
            shifted: Some('A'),
            base: Some('q'),
            text: Some("A".into()),
        });
        let expected = format!(
            "\x1b[97:65:113;2:{number}{}u",
            if number == 3 { "" } else { ";65" }
        );
        assert_eq!(term.encode_input(&event)?, expected.as_bytes());
    }
    assert_eq!(
        term.encode_input(&key(KeyCode::Named("KeypadEnter".into())))?,
        b"\x1b[57414;1:1u"
    );
    assert_eq!(
        term.encode_input(&key(KeyCode::Named("MediaPlayPause".into())))?,
        b"\x1b[57430;1:1u"
    );
    term.advance(b"\x1b[<u\x1b[?1000h\x1b[?1006h")?;
    assert_eq!(
        term.encode_input(&InputEvent::Mouse(Mouse {
            x: 2,
            y: 1,
            pixel_x: 0,
            pixel_y: 0,
            modifiers: 0,
            kind: MouseKind::Press(0)
        }))?,
        b"\x1b[<0;3;2M"
    );
    assert_eq!(
        term.encode_input(&InputEvent::Mouse(Mouse {
            x: 2,
            y: 1,
            pixel_x: 0,
            pixel_y: 0,
            modifiers: 0,
            kind: MouseKind::WheelHorizontal(1)
        }))?,
        b"\x1b[<66;3;2M"
    );
    let effects = term
        .advance(b"\x07\x1b]52;c;aGVsbG8=\x1b\\\x1b]52;c;?\x1b\\")?
        .effects;
    assert_eq!(effects[0], EffectKind::Bell);
    assert_eq!(
        effects[1],
        EffectKind::ClipboardWrite {
            selection: "c".into(),
            data: "hello".into()
        }
    );
    let EffectKind::ClipboardRead { query, .. } = effects[2] else {
        panic!("missing query")
    };
    assert_eq!(
        term.encode_input(&InputEvent::QueryResult {
            query,
            data: Some("reply".into())
        })?,
        b"\x1b]52;c;cmVwbHk=\x1b\\"
    );
    assert!(
        term.encode_input(&InputEvent::QueryResult {
            query,
            data: Some("duplicate".into())
        })?
        .is_empty()
    );
    term.advance(b"\x1b]133;A\x1b\\\x1b]133;B\x1b\\\x1b]133;C\x1b\\\x1b]133;D;7\x1b\\")?;
    assert_eq!(term.snapshot()?.markers.last().unwrap().status, Some(7));
    assert!(!term.advance(b"\x1b[?2026hhello")?.publish_allowed);
    std::thread::sleep(std::time::Duration::from_millis(170));
    assert!(term.publish_allowed());
    assert_eq!(
        term.advance(&vec![7u8; 32768])?.effects,
        vec![EffectKind::Bell]
    );
    assert_eq!(
        term.encode_input(&InputEvent::Key(Key {
            code: KeyCode::Char('c'),
            modifiers: 4,
            phase: KeyPhase::Press,
            shifted: None,
            base: None,
            text: None
        }))?,
        vec![3]
    );
    println!(
        "text, attributes, split grapheme, semantic keyboard phases, paste, mouse, effects, query dedup, markers, sync timeout passed"
    );

    let mut kitty = model();
    let change = kitty.advance(b"\x1b_Ga=T,f=24,s=1,v=1,i=1;/wAA\x1b\\")?;
    assert_eq!(change.replies, b"\x1b_Gi=1;OK\x1b\\");
    let one = kitty.snapshot()?;
    let two = kitty.snapshot()?;
    let first = one.resources.values().next().unwrap();
    assert_eq!((first.meta.width, first.meta.height), (1, 1));
    assert_eq!(&*first.rgba, &[255, 0, 0, 255]);
    assert!(Arc::ptr_eq(
        &first.rgba,
        &two.resources.values().next().unwrap().rgba
    ));
    kitty.advance(b"\x1b_Ga=d,d=I,i=1\x1b\\")?;
    assert!(kitty.snapshot()?.resources.is_empty());
    let mut keypad = model();
    assert_eq!(
        keypad.encode_input(&key(KeyCode::Named("Keypad1".into())))?,
        b"1"
    );
    keypad.advance(b"\x1b=")?;
    assert_eq!(
        keypad.encode_input(&key(KeyCode::Named("Keypad1".into())))?,
        b"\x1bOq"
    );
    let mut sixel = model();
    sixel.advance(b"\x1bPq\"1;1;1;6#1;2;100;0;0#1~\x1b\\")?;
    assert_eq!(
        sixel
            .snapshot()?
            .resources
            .values()
            .next()
            .unwrap()
            .meta
            .height,
        6
    );
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&[1, 2, 3, 255])?;
    }
    let mut iterm = model();
    iterm.advance(
        format!(
            "\x1b]1337;File=inline=1:{}\x1b\\",
            base64::engine::general_purpose::STANDARD.encode(&png_bytes)
        )
        .as_bytes(),
    )?;
    assert_eq!(
        &*iterm.snapshot()?.resources.values().next().unwrap().rgba,
        &[1, 2, 3, 255]
    );
    let mut shapes = model();
    let pixels = base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 255, 1, 2, 3, 255]);
    shapes.advance(
        format!(
            "\x1b_Ga=T,f=32,s=1,v=2,i=1;{pixels}\x1b\\\x1b_Ga=T,f=32,s=2,v=1,i=2;{pixels}\x1b\\"
        )
        .as_bytes(),
    )?;
    assert_eq!(shapes.snapshot()?.resources.len(), 2);
    println!("static kitty/SIXEL/iTerm, resource Arc reuse, deletion passed");

    for bytes in [
        b"\x1b_Ga=q,t=f,f=24,s=1,v=1;L2V0Yy9wYXNzd2Q=\x1b\\".as_slice(),
        b"\x1b_Ga=T,t=s,f=24,s=1,v=1;L3Byb2Jl\x1b\\",
        b"\x1b_Ga=f,f=24,s=1,v=1;/wAA\x1b\\",
        b"\x1b_Ga=T,f=32,s=16384,v=16384;AAAA\x1b\\",
        b"\x1bPq!4294967295~\x1b\\",
    ] {
        assert!(model().advance(bytes).is_err(), "accepted unsafe image");
    }
    let mut quota = model();
    let image = format!(
        "\x1b_Ga=t,f=32,s=1024,v=1024,i=1;{}\x1b\\",
        base64::engine::general_purpose::STANDARD.encode(vec![1u8; 4 * 1024 * 1024])
    );
    quota.advance(image.as_bytes())?;
    let mut registry = model();
    for index in 0..5 {
        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_bytes, 2048, 2048);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()?
                .write_image_data(&vec![index as u8; 16 * 1024 * 1024])?;
        }
        let command = format!(
            "\x1b_Ga=t,f=100,i={};{}\x1b\\",
            index + 1,
            base64::engine::general_purpose::STANDARD.encode(&png_bytes)
        );
        let result = registry.advance(command.as_bytes());
        if index < 4 {
            result?;
        } else {
            assert!(result.is_err(), "unplaced registry exceeded 64 MiB");
        }
    }
    let mut chunks = model();
    for _ in 0..4096 {
        chunks.advance(b"\x1b_Ga=t,f=32,s=1,v=1,m=1;\x1b\\")?;
    }
    assert!(chunks.advance(b"\x1b_Gm=1;\x1b\\").is_err());
    let mut flood = model();
    assert!(
        flood
            .advance(&b"\x1b[6n".repeat(MAX_PENDING_INPUT))
            .is_err()
    );
    assert!(flood.encode_input(&key(KeyCode::Char('x'))).is_err());
    let mut unterminated = model();
    assert!(
        unterminated
            .advance(&[b"\x1bPq".as_slice(), &vec![b'?'; 256 * 1024]].concat())
            .is_err()
    );
    println!(
        "query/file/shared-memory/animation/dimensions/repeat limits, unplaced upload, collector sticky error, parser limit passed"
    );
    Ok(())
}

fn verify_default_colors() -> Result<()> {
    let mut term = model();
    term.advance(b"D\x1b[40mB\x1b[49mR\x1b[30mF\x1b[39mD")?;
    let screen = term.snapshot()?;
    let cells = &screen.rows[0].cells;
    assert_eq!(cells[0].style.fg, Color::Default);
    assert_eq!(cells[0].style.bg, Color::Default);
    assert_eq!(cells[1].style.bg, Color::Rgb([0, 0, 0]));
    assert_eq!(cells[2].style.bg, Color::Default);
    assert_eq!(cells[3].style.fg, Color::Rgb([0, 0, 0]));
    assert_eq!(cells[4].style.fg, Color::Default);

    term.advance(b"\x1b]11;#000000\x07")?;
    let screen = term.snapshot()?;
    assert_eq!(screen.rows[0].cells[0].style.bg, Color::Rgb([0, 0, 0]));
    assert_eq!(screen.rows[0].cells[0].style.fg, Color::Default);
    term.advance(b"\x1b]111\x07\x1b]10;#b2b2b2\x07")?;
    let screen = term.snapshot()?;
    assert_eq!(screen.rows[0].cells[0].style.bg, Color::Default);
    assert_eq!(screen.rows[0].cells[0].style.fg, Color::Rgb([178; 3]));
    assert_eq!(screen.rows[0].cells[3].style.fg, Color::Rgb([0, 0, 0]));

    term.advance(b"\x1b]10;#123456;#654321\x07\x1b]11;?\x07")?;
    let screen = term.snapshot()?;
    assert_eq!(screen.rows[0].cells[0].style.fg, Color::Rgb([18, 52, 86]));
    assert_eq!(screen.rows[0].cells[0].style.bg, Color::Rgb([101, 67, 33]));
    assert_eq!(screen.rows[0].cells[1].style.bg, Color::Rgb([0, 0, 0]));
    term.advance(b"\x1b]110\x07\x1b]111\x07\x1b]4;0;#010203\x07")?;
    let screen = term.snapshot()?;
    assert_eq!(screen.rows[0].cells[0].style.fg, Color::Default);
    assert_eq!(screen.rows[0].cells[0].style.bg, Color::Default);
    assert_eq!(screen.rows[0].cells[1].style.bg, Color::Rgb([1, 2, 3]));

    for reset in [b"\x1b]104\x07".as_slice(), b"\x1bc"] {
        term.advance(b"\x1b]10;#123456;#000000\x07")?;
        term.advance(reset)?;
        term.advance(b"\rZ")?;
        let screen = term.snapshot()?;
        assert_eq!(screen.rows[0].cells[0].style.fg, Color::Default);
        assert_eq!(screen.rows[0].cells[0].style.bg, Color::Default);
    }
    println!(
        "default terminal colors stay symbolic; explicit black, dynamic colors and resets survive snapshots"
    );
    Ok(())
}

fn verify_terminal_boundaries() -> Result<()> {
    use mosh_rs::{
        display::{Display, Graphics, PredictionPolicy},
        state, transport,
    };

    let size = Size {
        cols: 80,
        rows: 24,
        pixel_width: 800,
        pixel_height: 480,
    };
    let mut query = TerminalModel::new(size, false)?;
    for row in 1..=24 {
        query.advance(format!("\x1b[{row};1H{}", "X".repeat(80)).as_bytes())?;
    }
    for command in [b"\x1b[7;1;1;1;24;80*y".as_slice(), b"\x1b[7;1;1;1;25;80*y"] {
        assert_eq!(query.advance(command)?.replies, b"\x1bP7!~9400\x1b\\");
    }
    for command in [b"\x1b[7;1;25;1;26;80*y".as_slice(), b"\x1b[7;1;2;2;1;1*y"] {
        assert_eq!(query.advance(command)?.replies, b"\x1bP7!~0000\x1b\\");
    }
    let mut wide = TerminalModel::new(size, false)?;
    wide.advance("界A".as_bytes())?;
    assert_eq!(
        wide.advance(b"\x1b[1;1;1;3;1;3*y")?.replies,
        b"\x1bP1!~0041\x1b\\"
    );
    let mut blank = TerminalModel::new(
        Size {
            cols: 64,
            rows: 32,
            ..size
        },
        false,
    )?;
    assert_eq!(
        blank.advance(b"\x1b[2;1;1;1;32;64*y")?.replies,
        b"\x1bP2!~0000\x1b\\"
    );

    let size = Size {
        cols: 20,
        rows: 4,
        pixel_width: 200,
        pixel_height: 80,
    };
    let rgba = [10, 20, 30, 255].repeat(210);
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 210, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.write_header()?.write_image_data(&rgba)?;
    }
    let commands = [
        (
            "Kitty",
            format!(
                "\x1b_Ga=T,f=32,s=210,v=1;{}\x1b\\",
                base64::engine::general_purpose::STANDARD.encode(&rgba)
            ),
        ),
        (
            "iTerm",
            format!(
                "\x1b]1337;File=inline=1;width=210px;height=1px;preserveAspectRatio=0:{}\x1b\\",
                base64::engine::general_purpose::STANDARD.encode(&png)
            ),
        ),
        ("SIXEL", "\x1bPq#1;2;100;0;0#1!210~\x1b\\".into()),
    ];
    for (kind, command) in commands {
        for column in [1, 19] {
            let mut image = TerminalModel::new(size, false)?;
            image
                .advance(format!("\x1b[1;{column}H{command}").as_bytes())
                .with_context(|| format!("{kind} native image input at column {column}"))?;
            let screen = image.snapshot()?;
            let frame = state::frame(1, 1, 0, &screen, None)
                .with_context(|| format!("{kind} native image at column {column}"))?;
            transport::validate_frame(&frame)
                .with_context(|| format!("{kind} native image validation at column {column}"))?;
            assert!(
                screen
                    .rows
                    .iter()
                    .flat_map(|row| &row.cells)
                    .all(|cell| usize::from(cell.x) + usize::from(cell.width) <= 20)
            );
            let tiles: Vec<_> = screen
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .flat_map(|cell| &cell.images)
                .collect();
            assert!(!tiles.is_empty());
            assert!(
                tiles
                    .iter()
                    .all(|tile| tile.source[0] + tile.source[2] < 210)
            );
            let original = screen.rows.clone();
            let mut displays = [Graphics::Kitty, Graphics::Sixel].map(|graphics| {
                let mut display =
                    Display::new(Vec::new(), graphics, PredictionPolicy::Never, screen.size)
                        .expect("display viewport");
                display.set_epoch(1).expect("set display epoch");
                display
            });
            for display in &mut displays {
                assert!(display.install(&frame, &screen.resources)?);
            }
            for (revision, cols) in [(2, 16), (3, 20), (4, 21)] {
                image.resize(Size {
                    cols,
                    pixel_width: u32::from(cols) * 10,
                    ..size
                })?;
                let screen = image.snapshot()?;
                let frame = state::frame(1, revision, 0, &screen, None).with_context(|| {
                    format!("{kind} image at {cols} columns from column {column}")
                })?;
                transport::validate_frame(&frame)?;
                for display in &mut displays {
                    assert!(display.install(&frame, &screen.resources)?);
                }
                if cols == 16 && column == 19 {
                    assert!(screen.resources.is_empty());
                } else if cols == 20 {
                    assert_eq!(screen.rows, original);
                } else if cols == 21 {
                    assert!(
                        screen
                            .rows
                            .iter()
                            .flat_map(|row| &row.cells)
                            .any(|cell| { cell.x == 20 && !cell.images.is_empty() })
                    );
                }
            }
        }
    }

    let mut narrow = TerminalModel::new(Size { cols: 2, ..size }, false)?;
    narrow.advance("界".as_bytes())?;
    narrow.resize(Size { cols: 1, ..size })?;
    let screen = narrow.snapshot()?;
    transport::validate_frame(&state::frame(1, 1, 0, &screen, None)?)?;
    assert_eq!(
        (
            screen.rows[0].cells[0].text.as_str(),
            screen.rows[0].cells[0].width
        ),
        (" ", 1)
    );
    narrow.resize(Size { cols: 2, ..size })?;
    assert_eq!(narrow.snapshot()?.rows[0].cells[0].text, "界");

    let mut controls = TerminalModel::new(size, false)?;
    controls.advance(b"\x1b[20G\x1b[4294967295D")?;
    assert_eq!(controls.snapshot()?.cursor.x, 0);
    controls.advance(b"\x1b[4294967295I")?;
    assert_eq!(controls.snapshot()?.cursor.x, 19);
    controls.advance(b"\x1b[4294967295Z")?;
    assert_eq!(controls.snapshot()?.cursor.x, 0);
    controls.advance(b"abc\r\x1b[4294967295@")?;
    assert!(
        controls.snapshot()?.rows[0]
            .cells
            .iter()
            .all(|cell| cell.text == " ")
    );
    assert_eq!(
        controls
            .advance(b"\x1b[>31u\x1b[<4294967295u\x1b[?u")?
            .replies,
        b"\x1b[?0u"
    );
    let mut reverse = TerminalModel::new(
        Size {
            cols: 4,
            rows: 3,
            ..size
        },
        false,
    )?;
    reverse.advance(b"\x1b[?45h\x1b[4294967295D")?;
    assert_eq!(
        (reverse.snapshot()?.cursor.x, reverse.snapshot()?.cursor.y),
        (1, 2)
    );
    let mut repeat = TerminalModel::new(size, false)?;
    repeat.advance(b"X\x1b[32768b")?;
    let screen = repeat.snapshot()?;
    transport::validate_frame(&state::frame(1, 1, 0, &screen, None)?)?;
    for count in [32769u32, u32::MAX] {
        let mut repeat = TerminalModel::new(size, false)?;
        assert!(
            repeat
                .advance(format!("X\x1b[{count}b").as_bytes())
                .is_err()
        );
    }
    println!("rectangle checksums, clipped native images and bounded control counts passed");
    Ok(())
}

fn verify_png_metadata() -> Result<()> {
    fn encode(info: png::Info<'_>, pixels: &[u8]) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        {
            let mut writer = png::Encoder::with_info(&mut bytes, info)?.write_header()?;
            writer.write_image_data(pixels)?;
            writer.finish()?;
        }
        Ok(bytes)
    }
    fn command(bytes: &[u8]) -> String {
        format!(
            "\x1b_Ga=T,f=100,i=1;{}\x1b\\",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }
    fn pixels(bytes: &[u8], expected: &[u8]) -> Result<Digest> {
        let mut terminal = model();
        assert_eq!(
            terminal.advance(command(bytes).as_bytes())?.replies,
            b"\x1b_Gi=1;OK\x1b\\"
        );
        let screen = terminal.snapshot()?;
        let resource = screen.resources.values().next().unwrap();
        assert_eq!(&*resource.rgba, expected);
        Ok(resource.meta.digest)
    }

    let mut info = png::Info::with_size(1, 1);
    info.color_type = png::ColorType::Rgba;
    let plain = encode(info.clone(), &[1, 2, 3, 255])?;
    let digest = pixels(&plain, &[1, 2, 3, 255])?;
    // Public-domain sRGB profile generated by moxcms 0.8.1 ColorProfile::new_srgb().encode().
    let profile = base64::engine::general_purpose::STANDARD.decode(concat!(
        "AAACZAAAAAAEMAAAbW50clJHQiBYWVogB+oACgAZABEAMgAtYWNzcAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAPbD",
        "AAEAAAAA0yMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAALclhZWgAAAQgAAAAU",
        "Z1hZWgAAARwAAAAUYlhZWgAAATAAAAAUY2hhZAAAAUQAAAAsclRSQwAAAXAAAAAgZ1RSQwAAAZAAAAAgYlRSQwAAAbAAAAAg",
        "d3RwdAAAAdAAAAAUY2ljcAAAAeQAAAAMZGVzYwAAAfAAAAA+Y3BydAAAAi4AAAA2WFlaIAAAAAAAAG+TAAA47QAAA5FYWVog",
        "AAAAAAAAYqQAALePAAAY31hZWiAAAAAAAAAkoAAAD4MAALbPc2YzMgAAAAAAAOUlAABEM///1q7//z/zAAG2qAAACWUAAAn1",
        "///udwABB5RwYXJhAAAAAAADAAAAAmZmAADypwAADVkAABPQAAAKW3BhcmEAAAAAAAMAAAACZmYAAPKnAAANWQAAE9AAAApb",
        "cGFyYQAAAAAAAwAAAAJmZgAA8qcAAA1ZAAAT0AAACltYWVogAAAAAAAA8z0AAQAAAAEWmGNpY3AAAAAAAQ0BAG1sdWMAAAAA",
        "AAAAAQAAAAxlblVTAAAAIgAAABwAcwBSAEcAQgAgAEkARQBDADYAMQA5ADYANgAtADIALgAxbWx1YwAAAAAAAAABAAAADGVu",
        "VVMAAAAaAAAAHABQAHUAYgBsAGkAYwAgAEQAbwBtAGEAaQBu",
    ))?;
    let mut metadata = info.clone();
    metadata.icc_profile = Some(profile.into());
    metadata.exif_metadata = Some(b"II*\0\x08\0\0\0\0\0\0\0\0\0".as_slice().into());
    metadata
        .compressed_latin1_text
        .push(png::text_metadata::ZTXtChunk::new("comment", "ignored"));
    let mut text = png::text_metadata::ITXtChunk::new("comment", "ignored \u{754c}");
    text.compressed = true;
    metadata.utf8_text.push(text);
    assert_eq!(
        pixels(&encode(metadata, &[1, 2, 3, 255])?, &[1, 2, 3, 255])?,
        digest
    );

    let mut palette = png::Info::with_size(2, 1);
    palette.color_type = png::ColorType::Indexed;
    palette.palette = Some(vec![255, 0, 0, 0, 255, 0].into());
    palette.trns = Some(vec![0, 128].into());
    pixels(&encode(palette, &[0, 1])?, &[255, 0, 0, 0, 0, 255, 0, 128])?;
    let mut gray = png::Info::with_size(2, 1);
    gray.trns = Some(vec![0, 7].into());
    pixels(&encode(gray, &[7, 255])?, &[7, 7, 7, 0, 255, 255, 255, 255])?;
    let mut sixteen = info.clone();
    sixteen.bit_depth = png::BitDepth::Sixteen;
    pixels(
        &encode(sixteen, &[0x12, 0xff, 0x56, 0x78, 0x9a, 0x00, 0xff, 0xff])?,
        &[19, 86, 153, 255],
    )?;

    let mut tail = Vec::new();
    {
        let mut writer = png::Encoder::with_info(&mut tail, info.clone())?.write_header()?;
        writer.write_image_data(&[1, 2, 3, 255])?;
        writer.write_text_chunk(&png::text_metadata::TEXtChunk::new("tail", "ignored"))?;
        writer.finish()?;
    }
    assert_eq!(pixels(&tail, &[1, 2, 3, 255])?, digest);
    assert!(
        model()
            .advance(command(&tail[..tail.len() - 13]).as_bytes())
            .is_err()
    );
    for name in [*b"ABCD", *b"ab1d", *b"abcd"] {
        let mut invalid = Vec::new();
        {
            let mut writer = png::Encoder::with_info(&mut invalid, info.clone())?.write_header()?;
            writer.write_chunk(png::chunk::ChunkType(name), &[])?;
            writer.write_image_data(&[1, 2, 3, 255])?;
            writer.finish()?;
        }
        assert!(model().advance(command(&invalid).as_bytes()).is_err());
    }
    let mut incomplete = plain.clone();
    incomplete.extend_from_slice(b"\0\0\0\0t");
    assert!(model().advance(command(&incomplete).as_bytes()).is_err());
    let mut animated = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut animated, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_animated(1, 0)?;
        encoder.write_header()?.write_image_data(&[1, 2, 3, 255])?;
    }
    assert!(model().advance(command(&animated).as_bytes()).is_err());

    info.icc_profile = Some(vec![0; 40 * 1024 * 1024].into());
    let oversized = encode(info, &[1, 2, 3, 255])?;
    let payload = base64::engine::general_purpose::STANDARD.encode(&oversized);
    let commands = [
        ("query", format!("\x1b_Ga=q,f=100,i=1;{payload}\x1b\\")),
        ("transmit", format!("\x1b_Ga=T,f=100,i=1;{payload}\x1b\\")),
        ("iTerm", format!("\x1b]1337;File=inline=1:{payload}\x1b\\")),
    ];
    let mut allocations = [0; 3];
    for (index, (route, command)) in commands.iter().enumerate() {
        let mut terminal = model();
        MAX_ALLOCATION.store(0, Ordering::Relaxed);
        MEASURE_ALLOCATIONS.store(true, Ordering::Relaxed);
        let result = terminal.advance(command.as_bytes());
        MEASURE_ALLOCATIONS.store(false, Ordering::Relaxed);
        allocations[index] = MAX_ALLOCATION.load(Ordering::Relaxed);
        let change = result?;
        if *route == "iTerm" {
            assert!(change.replies.is_empty());
        } else {
            assert_eq!(change.replies, b"\x1b_Gi=1;OK\x1b\\");
        }
        let screen = terminal.snapshot()?;
        if *route == "query" {
            assert!(screen.resources.is_empty());
        } else {
            let resource = screen.resources.values().next().unwrap();
            assert_eq!((resource.meta.width, resource.meta.height), (1, 1));
            assert_eq!(&*resource.rgba, &[1, 2, 3, 255]);
        }
        println!(
            "PNG {route} largest Rust allocation request: {} bytes",
            allocations[index]
        );
    }
    assert!(
        allocations.iter().all(|size| *size <= 32 * 1024 * 1024),
        "PNG metadata exceeded 32 MiB per allocation request: {allocations:?}"
    );
    println!(
        "PNG ICC/query/transmit/iTerm, transparency, 16-bit, metadata tail, unknown critical and APNG checks passed"
    );
    Ok(())
}

#[derive(Default)]
struct CountBytes(usize);
impl std::io::Write for CountBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn checked(model: &mut TerminalModel) -> Result<Screen> {
    use mosh_rs::{
        display::{Display, Graphics, PredictionPolicy},
        state,
    };
    let screen = model.snapshot()?;
    assert_eq!(screen.rows.len(), usize::from(screen.size.rows));
    assert!(screen.cursor.x < screen.size.cols && screen.cursor.y < screen.size.rows);
    let dsr = model.advance(b"\x1b[6n")?.replies;
    assert_eq!(
        dsr,
        format!("\x1b[{};{}R", screen.cursor.y + 1, screen.cursor.x + 1).as_bytes()
    );
    let frame = state::frame(1, 1, 0, &screen, None)?;
    let mut display = Display::new(
        Vec::new(),
        Graphics::None,
        PredictionPolicy::Never,
        screen.size,
    )?;
    display.set_epoch(1)?;
    assert!(display.install(&frame, &screen.resources)?);
    Ok(screen)
}

fn verify_projection_regressions() -> Result<()> {
    use mosh_rs::{
        display::{Display, Graphics, PredictionPolicy},
        state,
    };
    fn small() -> Result<TerminalModel> {
        TerminalModel::new(
            Size {
                cols: 4,
                rows: 3,
                pixel_width: 0,
                pixel_height: 0,
            },
            false,
        )
    }
    for chunks in [
        vec!["12🇨", "🇳"],
        vec!["123🇨", "🇳"],
        vec!["123❤", "\u{fe0f}"],
        vec!["12👩", "\u{200d}", "💻"],
    ] {
        for suffix in ["Z", "\rZ", "\x08Z", "界", "\r\n終"] {
            let mut split = small()?;
            let mut whole = small()?;
            for chunk in &chunks {
                split.advance(chunk.as_bytes())?;
            }
            whole.advance(chunks.concat().as_bytes())?;
            let split_screen = checked(&mut split)?;
            let whole_screen = checked(&mut whole)?;
            assert_eq!(split_screen.cursor, whole_screen.cursor, "{chunks:?}");
            assert_eq!(split_screen.rows, whole_screen.rows, "{chunks:?}");
            split.advance(suffix.as_bytes())?;
            whole.advance(suffix.as_bytes())?;
            let split_screen = checked(&mut split)?;
            let whole_screen = checked(&mut whole)?;
            assert_eq!(
                split_screen.cursor, whole_screen.cursor,
                "{chunks:?} then {suffix:?}"
            );
            assert_eq!(
                split_screen.rows, whole_screen.rows,
                "{chunks:?} then {suffix:?}"
            );
        }
    }
    let mut flag = small()?;
    flag.advance("12🇨".as_bytes())?;
    flag.advance("🇳".as_bytes())?;
    assert_eq!(
        (checked(&mut flag)?.cursor.x, flag.snapshot()?.cursor.y),
        (3, 0)
    );
    flag.advance(b"Z")?;
    let screen = checked(&mut flag)?;
    assert_eq!((screen.cursor.x, screen.cursor.y), (1, 1));
    assert_eq!(screen.rows[1].cells[0].text, "Z");

    let mut prompt = small()?;
    prompt.advance(b"1234\x1b]133;A\x1b\\")?;
    let screen = checked(&mut prompt)?;
    assert_eq!(
        (screen.markers[0].row, screen.markers[0].column),
        (screen.rows[1].stable_id, 0)
    );
    prompt.advance(b"x\x1b]133;L\x1b\\")?;
    assert_eq!(checked(&mut prompt)?.markers.len(), 1);
    prompt.advance(b"x\x1b]133;N\x1b\\")?;
    let screen = checked(&mut prompt)?;
    let marker = screen.markers.last().unwrap();
    assert_eq!(marker.kind, "A");
    assert_eq!(
        (marker.row, marker.column),
        (screen.rows[screen.cursor.y as usize].stable_id, 0)
    );

    let mut image = small()?;
    image.advance(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=4,r=2,C=1;/wAA/w==\x1b\\")?;
    let screen = checked(&mut image)?;
    let tiles: Vec<_> = screen
        .rows
        .iter()
        .flat_map(|row| &row.cells)
        .flat_map(|cell| &cell.images)
        .collect();
    assert_eq!(tiles.len(), 8);
    assert!(tiles.iter().all(|tile| tile.source == [0, 0, 1, 1]));
    for graphics in [Graphics::Kitty, Graphics::Sixel] {
        let mut display = Display::new(Vec::new(), graphics, PredictionPolicy::Never, screen.size)?;
        display.set_epoch(1)?;
        display.install(&state::frame(1, 1, 0, &screen, None)?, &screen.resources)?;
    }

    let mut keyboard = small()?;
    assert_eq!(
        keyboard.encode_input(&InputEvent::Key(Key {
            code: KeyCode::Char('6'),
            modifiers: 3,
            phase: KeyPhase::Press,
            shifted: Some('^'),
            base: Some('6'),
            text: Some("^".into()),
        }))?,
        b"\x1b^"
    );
    for title in [
        "\x1b]2;before\tafter\x1b\\",
        "\x1b]2;before\rafter\x1b\\",
        "\x1b]2;before\u{009b}after\x1b\\",
        "\x1bkhello\x1b\\",
    ] {
        let mut model = small()?;
        model.advance(title.as_bytes())?;
        let screen = checked(&mut model)?;
        assert!(screen.title.chars().all(|c| !c.is_control()));
        assert!(screen.title.len() <= 4096);
        model.advance(b"Z")?;
        checked(&mut model)?;
    }
    let mut metadata = small()?;
    metadata.advance(format!("\x1bk{}😃\x1b\\", "a".repeat(4095)).as_bytes())?;
    assert_eq!(checked(&mut metadata)?.title, "a".repeat(4095));
    metadata
        .advance(format!("\x1b]8;id={};https://example.com\x1b\\Z", "a".repeat(6000)).as_bytes())?;
    let screen = checked(&mut metadata)?;
    assert_eq!(
        screen.rows[0].cells[0]
            .style
            .hyperlink
            .as_ref()
            .unwrap()
            .params
            .len(),
        4096
    );
    assert!(
        metadata
            .advance(b"\x1b]52;8;aGVsbG8=\x1b\\")?
            .effects
            .is_empty()
    );
    let refused = metadata.advance(b"\x1b]52;9;?\x1b\\")?;
    assert!(refused.effects.is_empty());
    assert_eq!(refused.replies, b"\x1b]52;9;\x1b\\");
    for cols in [4, 8] {
        let mut resized = small()?;
        resized.advance(b"1234")?;
        resized.resize(Size {
            cols,
            rows: 4,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        checked(&mut resized)?;
        resized.advance(b"Z")?;
        let screen = checked(&mut resized)?;
        let (x, y) = if cols == 4 { (1, 1) } else { (5, 0) };
        assert_eq!((screen.cursor.x, screen.cursor.y), (x, y));
        assert_eq!(
            screen.rows[y as usize]
                .cells
                .iter()
                .find(|cell| cell.x == x - 1)
                .unwrap()
                .text,
            "Z"
        );
    }
    let mut resized = TerminalModel::new(
        Size {
            cols: 80,
            rows: 12,
            pixel_width: 0,
            pixel_height: 0,
        },
        false,
    )?;
    resized.advance(&vec![b'x'; 120000])?;
    checked(&mut resized)?;
    resized.resize(Size {
        cols: 100,
        rows: 20,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    checked(&mut resized)?;
    resized.advance("界\rQ\x08Z".as_bytes())?;
    checked(&mut resized)?;
    println!(
        "Unicode split/whole continuation, DSR, resize, semantic markers, scaled images, legacy symbols, metadata validation passed"
    );
    Ok(())
}

fn text_cells(screen: &Screen) -> Vec<(u16, u16, &str, u8)> {
    screen
        .rows
        .iter()
        .enumerate()
        .flat_map(|(y, row)| {
            row.cells.iter().filter_map(move |cell| {
                (cell.text != " ").then_some((cell.x, y as u16, cell.text.as_str(), cell.width))
            })
        })
        .collect()
}

fn expect_cells(screen: &Screen, cells: &[(u16, u16, &str, u8)], cursor: (u16, u16)) {
    assert_eq!(text_cells(screen), cells);
    assert_eq!((screen.cursor.x, screen.cursor.y), cursor);
}

fn verify_combining_regressions() -> Result<()> {
    let size = Size {
        cols: 20,
        rows: 4,
        pixel_width: 0,
        pixel_height: 0,
    };
    fn check(
        size: Size,
        chunks: &[&str],
        cells: &[(u16, u16, &str, u8)],
        cursor: (u16, u16),
    ) -> Result<Screen> {
        let whole = chunks.concat();
        let variants = [
            vec![whole.as_bytes()],
            chunks.iter().map(|chunk| chunk.as_bytes()).collect(),
            whole.as_bytes().chunks(1).collect(),
        ];
        let mut result = None;
        for variant in variants {
            let mut term = TerminalModel::new(size, false)?;
            term.advance(b"\x1b[2J\x1b[H")?;
            for chunk in variant {
                term.advance(chunk)?;
            }
            let screen = checked(&mut term)?;
            expect_cells(&screen, cells, cursor);
            result = Some(screen);
        }
        Ok(result.unwrap())
    }
    let leading = [(0, 0, "\u{00a0}\u{0302}", 1), (1, 0, "X", 1)];
    check(size, &["\u{0302}", "X"], &leading, (2, 0))?;
    check(size, &["A\r\x1b[2K", "\u{0302}", "X"], &leading, (2, 0))?;
    check(
        size,
        &["e\r\n", "\u{0302}", "X"],
        &[
            (0, 0, "e", 1),
            (0, 1, "\u{00a0}\u{0302}", 1),
            (1, 1, "X", 1),
        ],
        (2, 1),
    )?;
    check(
        size,
        &["\u{0302}", "\u{0301}", "X"],
        &[(0, 0, "\u{00a0}\u{0302}\u{0301}", 1), (1, 0, "X", 1)],
        (2, 0),
    )?;
    check(
        size,
        &["e", "\u{0302}", "X"],
        &[(0, 0, "e\u{0302}", 1), (1, 0, "X", 1)],
        (2, 0),
    )?;
    for (mark, expected) in [
        ("\u{0900}", "\u{00a0}\u{0900}"),
        ("\u{0488}", "\u{00a0}\u{0488}"),
        ("\u{20dd}", "\u{00a0}\u{20dd}"),
    ] {
        check(
            size,
            &[mark, "X"],
            &[(0, 0, expected, 1), (1, 0, "X", 1)],
            (2, 0),
        )?;
    }
    for ignorable in [
        "\u{034f}",
        "\u{17b4}",
        "\u{17b5}",
        "\u{180b}",
        "\u{180c}",
        "\u{180d}",
        "\u{180f}",
        "\u{fe00}",
        "\u{fe0f}",
        "\u{e0100}",
        "\u{e01ef}",
        "\u{200c}",
        "\u{200d}",
        "\u{2068}",
        "\u{180e}",
        "\u{e0020}",
        "\u{102b}",
        "\u{1038}",
    ] {
        check(size, &[ignorable, "X"], &[(0, 0, "X", 1)], (1, 0))?;
    }
    for (suffix, expected) in [
        ("\u{034f}", "\u{00a0}\u{0302}\u{034f}"),
        ("\u{fe0f}", "\u{00a0}\u{0302}\u{fe0f}"),
        ("\u{e0100}", "\u{00a0}\u{0302}\u{e0100}"),
    ] {
        check(
            size,
            &["\u{0302}", suffix, "X"],
            &[(0, 0, expected, 1), (1, 0, "X", 1)],
            (2, 0),
        )?;
    }
    check(
        size,
        &["e", "\u{034f}", "\u{0302}", "X"],
        &[(0, 0, "e\u{034f}\u{0302}", 1), (1, 0, "X", 1)],
        (2, 0),
    )?;
    check(
        size,
        &["\u{1e6e3}", "X"],
        &[(0, 0, "\u{1e6e3}", 1), (1, 0, "X", 1)],
        (2, 0),
    )?;

    let narrow = Size {
        cols: 4,
        rows: 3,
        ..size
    };
    check(
        narrow,
        &["12界", "\u{0302}", "X"],
        &[
            (0, 0, "1", 1),
            (1, 0, "2", 1),
            (2, 0, "界\u{0302}", 2),
            (0, 1, "X", 1),
        ],
        (1, 1),
    )?;
    check(
        narrow,
        &["123界", "\u{0302}", "X"],
        &[
            (0, 0, "1", 1),
            (1, 0, "2", 1),
            (2, 0, "3", 1),
            (0, 1, "界\u{0302}", 2),
            (2, 1, "X", 1),
        ],
        (3, 1),
    )?;
    check(
        narrow,
        &["\x1b[1;4H", "\u{0302}", "X"],
        &[(3, 0, "\u{00a0}\u{0302}", 1), (0, 1, "X", 1)],
        (1, 1),
    )?;
    check(
        narrow,
        &["\x1b[?7l\x1b[1;4H", "\u{0302}", "X"],
        &[(3, 0, "X", 1)],
        (3, 0),
    )?;
    check(
        size,
        &["AB\x1b[H\x1b[4h", "\u{0302}", "X"],
        &[
            (0, 0, "\u{00a0}\u{0302}", 1),
            (1, 0, "X", 1),
            (2, 0, "A", 1),
            (3, 0, "B", 1),
        ],
        (2, 0),
    )?;
    let styled = check(
        size,
        &["\x1b[38;2;12;34;56m", "\u{0302}", "\x1b[0mX"],
        &leading,
        (2, 0),
    )?;
    assert_eq!(styled.rows[0].cells[0].style.fg, Color::Rgb([12, 34, 56]));
    assert_eq!(styled.rows[0].cells[1].style.fg, Color::Default);
    let styled = check(
        size,
        &["\x1b[38;2;12;34;56me", "\x1b[38;2;65;43;21m\u{0302}", "X"],
        &[(0, 0, "e\u{0302}", 1), (1, 0, "X", 1)],
        (2, 0),
    )?;
    assert_eq!(styled.rows[0].cells[0].style.fg, Color::Rgb([12, 34, 56]));

    for (chunks, expected, cursor) in [
        (vec!["\u{034f}\u{0302}X"], "\u{00a0}\u{034f}\u{0302}", 2),
        (vec!["\u{034f}", "\u{0302}X"], "\u{00a0}\u{0302}", 2),
        (vec!["\u{0600}\u{0302}X"], "X", 1),
        (vec!["\u{0600}", "\u{0302}X"], "\u{00a0}\u{0302}", 2),
    ] {
        let mut term = TerminalModel::new(size, false)?;
        for chunk in chunks {
            term.advance(chunk.as_bytes())?;
        }
        let screen = checked(&mut term)?;
        if cursor == 1 {
            expect_cells(&screen, &[(0, 0, expected, 1)], (1, 0));
        } else {
            expect_cells(&screen, &[(0, 0, expected, 1), (1, 0, "X", 1)], (2, 0));
        }
    }

    fn refused(chunks: &[&str]) -> Result<()> {
        let mut term = model();
        let mut error = None;
        for chunk in chunks {
            if let Err(rejected) = term.advance(chunk.as_bytes()) {
                error = Some(rejected);
                break;
            }
        }
        assert!(
            error
                .context("oversized grapheme was accepted")?
                .to_string()
                .contains("grapheme byte limit")
        );
        assert!(term.snapshot().is_err());
        assert!(term.advance(b"X").is_err());
        Ok(())
    }
    let marks31 = "\u{0302}".repeat(31);
    let marks32 = "\u{0302}".repeat(32);
    let original31 = format!("¢{marks31}");
    let original32 = format!("¢{marks32}");
    let fallback31 = format!("\u{00a0}{marks31}");
    assert_eq!(original31.len(), 64);
    assert_eq!(fallback31.len(), 64);
    check(size, &[&original31], &[(0, 0, &original31, 1)], (1, 0))?;
    check(size, &["¢", &marks31], &[(0, 0, &original31, 1)], (1, 0))?;
    check(size, &[&marks31], &[(0, 0, &fallback31, 1)], (1, 0))?;
    refused(&[&original32])?;
    refused(&["¢", &marks32])?;
    refused(&[&marks32])?;
    let mut accumulated = vec!["¢"];
    accumulated.extend(std::iter::repeat_n("\u{0302}", 32));
    refused(&accumulated)?;
    refused(&["\u{0302}"; 32])?;
    refused(&[&format!("\u{034f}{marks31}")])?;
    let mut term = model();
    term.advance("\u{034f}".as_bytes())?;
    term.advance(marks31.as_bytes())?;
    expect_cells(&checked(&mut term)?, &[(0, 0, &fallback31, 1)], (1, 0));
    let mut term = model();
    term.advance("0\x1b[1J\u{0334}".as_bytes())?;
    checked(&mut term)?;
    println!(
        "orphan Mn/Me carriers, DI/Mc boundaries, exact cells/DSR, frame/display installation and three 64-byte limits passed"
    );
    Ok(())
}

fn verify_combining_transcript(arguments: &[String]) -> Result<()> {
    anyhow::ensure!(
        arguments.len() == 2,
        "expected transcript and combining case"
    );
    let mut term = TerminalModel::new(
        Size {
            cols: 80,
            rows: 24,
            pixel_width: 800,
            pixel_height: 480,
        },
        false,
    )?;
    term.advance(&std::fs::read(&arguments[0])?)?;
    let screen = checked(&mut term)?;
    let leading = [(0, 0, "\u{00a0}\u{0302}", 1), (1, 0, "X", 1)];
    match arguments[1].as_str() {
        "leading" | "erase" | "split-utf8" => expect_cells(&screen, &leading, (2, 0)),
        "newline" => expect_cells(
            &screen,
            &[
                (0, 0, "e", 1),
                (0, 1, "\u{00a0}\u{0302}", 1),
                (1, 1, "X", 1),
            ],
            (2, 1),
        ),
        "repeated" => expect_cells(
            &screen,
            &[(0, 0, "\u{00a0}\u{0302}\u{0301}", 1), (1, 0, "X", 1)],
            (2, 0),
        ),
        "normal-base" => expect_cells(&screen, &[(0, 0, "e\u{0302}", 1), (1, 0, "X", 1)], (2, 0)),
        "filtered" => expect_cells(&screen, &[(0, 0, "X", 1)], (1, 0)),
        "tail-di" => expect_cells(
            &screen,
            &[(0, 0, "\u{00a0}\u{0302}\u{fe0f}", 1), (1, 0, "X", 1)],
            (2, 0),
        ),
        "enclosing" => expect_cells(
            &screen,
            &[(0, 0, "\u{00a0}\u{20dd}", 1), (1, 0, "X", 1)],
            (2, 0),
        ),
        _ => anyhow::bail!("unknown combining case"),
    }
    println!(
        "{}",
        serde_json::json!({
            "case": arguments[1], "cells": text_cells(&screen), "cursor": screen.cursor,
            "limits": "CLI transcript roundtrip through terminal core; native font/terminal visuals unverified"
        })
    );
    Ok(())
}

fn verify_scrollback_wrap() -> Result<()> {
    let mut terminal = TerminalModel::new(
        Size {
            cols: 80,
            rows: 12,
            pixel_width: 0,
            pixel_height: 0,
        },
        false,
    )?;
    for index in 0..2048 {
        terminal.advance(format!("{index:04}: cafe").as_bytes())?;
        terminal.advance("\u{0301}\r\n".as_bytes())?;
        if index % 64 == 63 {
            let screen = terminal.snapshot()?;
            assert_eq!(screen.rows.len(), 12);
            assert_eq!((screen.cursor.x, screen.cursor.y), (0, 11));
            let last_line: String = screen.rows[10]
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect();
            assert_eq!(last_line.trim_end(), format!("{index:04}: cafe\u{0301}"));
            assert_eq!(terminal.advance(b"\x1b[6n")?.replies, b"\x1b[12;1R");
        }
    }
    let reply = terminal.advance(b"done\rD\x1b[6n")?.replies;
    assert_eq!(reply, b"\x1b[12;2R");
    let screen = terminal.snapshot()?;
    let last_line: String = screen.rows[11]
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect();
    assert_eq!(last_line.trim_end(), "Done");
    println!(
        "2048 scrolling lines with split combining text, snapshots, continuation and DSR passed"
    );
    Ok(())
}

fn verify_soft_wrap_metadata() -> Result<()> {
    verify_wide_padding_provenance()?;
    #[derive(Debug)]
    struct WrapConfig;
    impl tattoy_wezterm_term::TerminalConfiguration for WrapConfig {
        fn color_palette(&self) -> tattoy_wezterm_term::color::ColorPalette {
            Default::default()
        }
    }
    let core = |cols, rows| {
        tattoy_wezterm_term::Terminal::new(
            tattoy_wezterm_term::TerminalSize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            },
            Arc::new(WrapConfig),
            "wrap-verification",
            "0",
            Box::new(std::io::sink()),
        )
    };
    let mut dangling = core(2, 2);
    dangling.advance_bytes(b"ABCD");
    let last = dangling.screen().phys_row(1);
    dangling
        .screen_mut()
        .line_mut(last)
        .set_last_cell_was_wrapped(true, 100);
    dangling.resize(tattoy_wezterm_term::TerminalSize {
        cols: 4,
        rows: 2,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 96,
    });
    let first = dangling.screen().phys_row(0);
    assert_eq!(
        dangling.screen().lines_in_phys_range(first..first + 1)[0].as_str(),
        "ABCD"
    );
    let mut stale = core(4, 4);
    stale.advance_bytes("abc界Z".as_bytes());
    let mut padding = tattoy_wezterm_term::CellAttributes::default();
    padding.set_wide_wrap_padding(true);
    stale
        .screen_mut()
        .set_cell_grapheme(1, 0, " ", 1, padding.clone(), 100);
    stale.resize(tattoy_wezterm_term::TerminalSize {
        cols: 8,
        rows: 4,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 96,
    });
    let first = stale.screen().phys_row(0);
    assert_eq!(
        stale.screen().lines_in_phys_range(first..first + 1)[0].as_str(),
        "a c界Z"
    );
    let mut repeated = core(4, 4);
    repeated.advance_bytes(b"ABC");
    repeated
        .screen_mut()
        .set_cell_grapheme(2, 0, " ", 1, padding, 100);
    repeated.advance_bytes(b"\x1b[1b");
    assert!(
        !repeated
            .screen_mut()
            .get_cell(3, 0)
            .unwrap()
            .attrs()
            .wide_wrap_padding()
    );
    for cols in [4, 512] {
        let mut core = tattoy_wezterm_term::Terminal::new(
            tattoy_wezterm_term::TerminalSize {
                cols,
                rows: 4,
                pixel_width: 0,
                pixel_height: 0,
                dpi: 96,
            },
            Arc::new(WrapConfig),
            "wrap-verification",
            "0",
            Box::new(std::io::sink()),
        );
        core.advance_bytes(format!(
            "{}\r\x1b[2K\x1b[38;2;11;22;33m{}界",
            "0".repeat(cols),
            "a".repeat(cols - 2)
        ));
        let physical = core.screen().phys_row(0);
        let before = core
            .screen()
            .lines_in_phys_range(physical..physical + 1)
            .remove(0);
        assert!(!before.last_cell_was_wrapped());
        let seqno = before.current_seqno();
        let style = before.visible_cells().last().unwrap().attrs().clone();
        core.advance_bytes(b"Z");
        let mut wrapped = core
            .screen()
            .lines_in_phys_range(physical..physical + 1)
            .remove(0);
        assert!(wrapped.current_seqno() > seqno);
        assert!(wrapped.last_cell_was_wrapped());
        let raw = wrapped.cells_mut_for_attr_changes_only();
        assert!(raw[cols - 2].attrs().wrapped() && raw[cols - 1].attrs().wrapped());
        assert_eq!(raw[cols - 2].attrs().foreground(), style.foreground());
        let mut logical = tattoy_wezterm_term::Line::new(100);
        for x in 0..cols - 2 {
            logical.set_cell_grapheme(x, "a", 1, style.clone(), 100);
        }
        logical.set_cell_grapheme(cols - 2, "界", 2, style.clone(), 100);
        let mut next = tattoy_wezterm_term::Line::new(100);
        next.set_cell_grapheme(0, "Z", 1, style.clone(), 100);
        logical.append_line(next, 101);
        let lines = logical.wrap(cols, 102);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.last_cell_was_wrapped())
                .collect::<Vec<_>>(),
            vec![true, false]
        );
        assert!(lines.iter().all(|line| line.current_seqno() >= 102));
        assert_eq!(lines[0].visible_cells().last().unwrap().str(), "界");
        assert!(
            lines
                .iter()
                .flat_map(|line| line.visible_cells())
                .all(|cell| cell.attrs().foreground() == style.foreground())
        );
    }
    for cols in [4, 512] {
        let size = Size {
            cols,
            rows: 4,
            pixel_width: 0,
            pixel_height: 0,
        };
        for overwritten in [false, true] {
            let mut model = TerminalModel::new(size, false)?;
            if overwritten {
                model.advance(format!("{}\r\x1b[2K", "0".repeat(usize::from(cols))).as_bytes())?;
            }
            let prefix = "a".repeat(usize::from(cols) - 2);
            model.advance(format!("{prefix}界").as_bytes())?;
            let full = model.snapshot()?;
            assert!(
                !full.rows[0].wrap.is_soft(),
                "pending wrap is not an outgoing connection"
            );
            model.advance(b"Z")?;
            let wrapped = model.snapshot()?;
            assert!(
                wrapped.rows[0].wrap.is_soft(),
                "wide leader must carry consumed wrap"
            );
            assert_eq!(wrapped.rows[0].cells.last().unwrap().text, "界");
            assert_eq!(wrapped.rows[1].cells[0].text, "Z");
            model.resize(Size {
                cols: cols / 2,
                ..size
            })?;
            let narrow = model.snapshot()?;
            assert!(narrow.rows[0].wrap.is_soft());
            assert!(narrow.rows[1].wrap.is_soft());
            model.resize(size)?;
            let restored = model.snapshot()?;
            assert_eq!(restored.rows[0].cells, wrapped.rows[0].cells);
            assert_eq!(restored.rows[1].cells[0].text, "Z");
            assert!(restored.rows[0].wrap.is_soft());
            assert!(
                !restored.rows[1].wrap.is_soft(),
                "reflow must clear the old wide spacer flag"
            );
            println!(
                "core soft-wrap cols={cols}, overwritten={overwritten}: pending, wide leader/spacer, narrow/wide reflow passed"
            );
        }
    }
    let mut old_row = serde_json::json!({"cells": [], "stable_id": 0});
    assert!(
        serde_json::from_value::<Row>(old_row.clone()).is_err(),
        "old Row must not silently default wrap kind"
    );
    old_row["wrap"] = "Soft".into();
    assert!(serde_json::from_value::<Row>(old_row)?.wrap.is_soft());
    Ok(())
}

fn verify_wide_padding_provenance() -> Result<()> {
    let size = Size {
        cols: 4,
        rows: 4,
        pixel_width: 0,
        pixel_height: 0,
    };
    let logical = |screen: &Screen| {
        let mut text = String::new();
        for row in &screen.rows {
            for cell in &row.cells {
                if row.wrap != RowWrap::WidePadding || cell.x != screen.size.cols - 1 {
                    text.push_str(&cell.text);
                }
            }
            if !row.wrap.is_soft() {
                break;
            }
        }
        text.trim_end_matches(' ').to_owned()
    };
    for (text, kind) in [
        ("abc界Z", RowWrap::WidePadding),
        ("abc 界Z", RowWrap::Soft),
        ("ab 界Z", RowWrap::WidePadding),
    ] {
        let mut model = TerminalModel::new(size, false)?;
        model.advance(text.as_bytes())?;
        let screen = model.snapshot()?;
        assert_eq!(screen.rows[0].wrap, kind, "{text}: source");
        assert_eq!(logical(&screen), text);
        assert_eq!((screen.cursor.x, screen.cursor.y), (3, 1));
        model.resize(Size { cols: 8, ..size })?;
        let screen = model.snapshot()?;
        assert_eq!(logical(&screen), text, "{text}: wider");
        assert_eq!(
            (screen.cursor.x, screen.cursor.y),
            (
                text.chars()
                    .map(|c| if c == '界' { 2 } else { 1 })
                    .sum::<u16>(),
                0
            )
        );
        model.resize(size)?;
        let screen = model.snapshot()?;
        assert_eq!(screen.rows[0].wrap, kind, "{text}: restored");
        assert_eq!(logical(&screen), text, "{text}: restored");
        assert_eq!((screen.cursor.x, screen.cursor.y), (3, 1));
    }
    for (trace, expected) in [
        (b"\x1b[1;1HX".as_slice(), RowWrap::WidePadding),
        (b"\x1b[1;4H ", RowWrap::Soft),
        (b"\x1b[1;4H\x1b[X", RowWrap::Soft),
        (b"\x1b[1;1H\x1b[@", RowWrap::Hard),
        (b"\x1b[1;1H\x1b[P", RowWrap::Hard),
    ] {
        let mut model = TerminalModel::new(size, false)?;
        model.advance("abc界Z".as_bytes())?;
        model.advance(trace)?;
        assert_eq!(model.snapshot()?.rows[0].wrap, expected, "{trace:?}");
        if trace == b"\x1b[1;4H " {
            assert_eq!(logical(&model.snapshot()?), "abc 界Z");
        }
    }
    let mut split = TerminalModel::new(size, false)?;
    split.advance("\x1b]1337;UnicodeVersion=14\x07abc☺".as_bytes())?;
    split.advance("\u{fe0f}Z".as_bytes())?;
    assert_eq!(split.snapshot()?.rows[0].wrap, RowWrap::WidePadding);
    let mut alternate = TerminalModel::new(size, false)?;
    alternate.advance("\x1b[?1049habc界Z".as_bytes())?;
    assert_eq!(alternate.snapshot()?.rows[0].wrap, RowWrap::WidePadding);
    alternate.resize(Size { cols: 8, ..size })?;
    alternate.resize(size)?;
    assert_ne!(alternate.snapshot()?.rows[0].wrap, RowWrap::WidePadding);
    let mut pending = TerminalModel::new(size, false)?;
    pending.advance(b"ABCD")?;
    pending.resize(Size { cols: 2, ..size })?;
    assert_eq!(
        (pending.snapshot()?.cursor.x, pending.snapshot()?.cursor.y),
        (1, 1)
    );
    pending.advance(b"E")?;
    assert_eq!(pending.snapshot()?.rows[2].cells[0].text, "E");
    let mut sparse = TerminalModel::new(Size { cols: 16, ..size }, false)?;
    sparse.advance(b"A\x1b[1;12H")?;
    sparse.resize(Size { cols: 8, ..size })?;
    assert_eq!(sparse.advance(b"\x1b[6n")?.replies, b"\x1b[2;4R");
    sparse.advance(b"X")?;
    assert_eq!(sparse.advance(b"\x1b[6n")?.replies, b"\x1b[2;5R");
    let screen = sparse.snapshot()?;
    assert_eq!(screen.rows[0].cells[0].text, "A");
    assert_eq!(
        screen.rows[1]
            .cells
            .iter()
            .find(|cell| cell.x == 3)
            .unwrap()
            .text,
        "X"
    );
    for (position, narrow, restored) in [
        (b"\x1b[1;4H".as_slice(), (1, 1), (3, 0)),
        (b"\x1b[2;1H", (0, 2), (0, 1)),
    ] {
        let mut model = TerminalModel::new(size, false)?;
        model.advance("abc界Z".as_bytes())?;
        model.advance(position)?;
        model.resize(Size { cols: 2, ..size })?;
        let screen = model.snapshot()?;
        assert_eq!((screen.cursor.x, screen.cursor.y), narrow);
        model.resize(size)?;
        let screen = model.snapshot()?;
        assert_eq!((screen.cursor.x, screen.cursor.y), restored);
    }
    let mut scrolled = TerminalModel::new(size, false)?;
    scrolled.advance("\x1b[2;1Habc界Z\x1b[4;1H\r\n".as_bytes())?;
    assert_eq!(scrolled.snapshot()?.rows[0].wrap, RowWrap::WidePadding);
    let mut partial = TerminalModel::new(size, false)?;
    partial.advance("abc界Z\x1b[?69h\x1b[2;4s\x1b[S".as_bytes())?;
    assert!(
        partial
            .snapshot()?
            .rows
            .iter()
            .all(|row| row.wrap != RowWrap::WidePadding)
    );
    let mut deletion = TerminalModel::new(
        Size {
            cols: 16,
            rows: 6,
            ..size
        },
        false,
    )?;
    deletion.advance("\x1b[4;1Hhttps://e.test/界Z".as_bytes())?;
    assert_eq!(deletion.snapshot()?.rows[3].wrap, RowWrap::WidePadding);
    deletion.advance(b"\x1b[?69h\x1b[1;4s\x1b[4;1H\x1b[P\x1b[?69l")?;
    let deleted = deletion.snapshot()?;
    assert_eq!(deleted.rows[3].wrap, RowWrap::Hard);
    assert_eq!(
        deleted.rows[3]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .trim_end(),
        "ttp s://e.test/"
    );
    assert_eq!(
        deleted.rows[4]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .trim_end(),
        "界Z"
    );
    let mut insertion = TerminalModel::new(
        Size {
            cols: 16,
            rows: 6,
            ..size
        },
        false,
    )?;
    insertion.advance("\x1b[4;1Hhttps://e.test/界Z".as_bytes())?;
    insertion.advance(b"\x1b[?69h\x1b[1;4s\x1b[4;2H\x1b[@\x1b[?69l")?;
    let inserted = insertion.snapshot()?;
    assert_eq!(inserted.rows[3].wrap, RowWrap::WidePadding);
    assert_eq!(
        inserted.rows[3]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .trim_end(),
        "h tts://e.test/"
    );
    assert_eq!(
        inserted.rows[4]
            .cells
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .trim_end(),
        "界Z"
    );
    println!(
        "wide padding distinguishes typed spaces, edits, split growth, reflow, scroll and cursor affinity"
    );
    Ok(())
}
