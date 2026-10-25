use anyhow::Result;
use base64::Engine;
use mosh_rs::model::*;
use mosh_rs::terminal::TerminalModel;
use std::sync::Arc;

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
