use anyhow::Result;
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
    verify_png_metadata()?;
    verify_projection_regressions()?;
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
    let bytes = postcard::to_io(&frame, CountBytes::default())?.0;
    assert!(bytes <= MAX_FRAME);
    println!("maximum 512x256 full RGB text frame: {bytes} postcard bytes");
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

fn verify_projection_regressions() -> Result<()> {
    use mosh_rs::{
        display::{Display, Graphics, PredictionPolicy},
        state,
    };
    fn checked(model: &mut TerminalModel) -> Result<Screen> {
        let screen = model.snapshot()?;
        assert_eq!(screen.rows.len(), usize::from(screen.size.rows));
        assert!(screen.cursor.x < screen.size.cols && screen.cursor.y < screen.size.rows);
        let dsr = model.advance(b"\x1b[6n")?.replies;
        assert_eq!(
            dsr,
            format!("\x1b[{};{}R", screen.cursor.y + 1, screen.cursor.x + 1).as_bytes()
        );
        let frame = state::frame(1, 1, 0, &screen, None)?;
        let mut display = Display::new(Vec::new(), Graphics::None, PredictionPolicy::Never);
        display.set_epoch(1);
        display.install(frame, &screen.resources)?;
        Ok(screen)
    }
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
        let mut display = Display::new(Vec::new(), graphics, PredictionPolicy::Never);
        display.set_epoch(1);
        display.install(state::frame(1, 1, 0, &screen, None)?, &screen.resources)?;
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
