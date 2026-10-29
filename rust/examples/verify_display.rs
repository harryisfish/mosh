use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use mosh_rs::display::{Display, Graphics, PredictionPolicy};
use mosh_rs::input::{
    EscapeAction, EscapeSequence, InputDecoder, InputRejection, LocalEvent, MAX_INPUT_CHUNK,
    PasteAdmission,
};
use mosh_rs::model::*;
use mosh_rs::{state, terminal::TerminalModel, transport};
use tattoy_wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

#[derive(Debug)]
struct Config;
impl TerminalConfiguration for Config {
    fn color_palette(&self) -> tattoy_wezterm_term::color::ColorPalette {
        Default::default()
    }
    fn enable_kitty_graphics(&self) -> bool {
        true
    }
    fn scrollback_size(&self) -> usize {
        0
    }
}

fn terminal() -> Terminal {
    Terminal::new(
        TerminalSize {
            rows: 4,
            cols: 16,
            pixel_width: 160,
            pixel_height: 80,
            dpi: 96,
        },
        Arc::new(Config),
        "mosh-verify",
        "0",
        Box::new(std::io::sink()),
    )
}

fn cell(x: u16, text: &str, width: u8) -> Cell {
    Cell {
        x,
        text: text.into(),
        width,
        style: Style::default(),
        images: Vec::new(),
    }
}

fn frame(revision: u64, base: Option<u64>, resources: Vec<ResourceMeta>) -> Frame {
    Frame {
        epoch: 1,
        revision,
        base,
        echo_through: 0,
        size: Size {
            cols: 16,
            rows: 4,
            pixel_width: 160,
            pixel_height: 80,
        },
        rows: (0..4)
            .map(|y| RowPatch {
                y,
                row: Row {
                    cells: Vec::new(),
                    stable_id: i64::from(y),
                },
            })
            .collect(),
        cursor: Cursor {
            x: 8,
            y: 1,
            shape: 3,
            visible: true,
            blink: false,
        },
        title: "mosh renderer verification".into(),
        directory: Some("file:///tmp".into()),
        markers: vec![ShellMarker {
            row: 0,
            column: 0,
            kind: "Prompt".into(),
            status: None,
        }],
        reverse_video: false,
        prediction: PredictionContext {
            generation: 1,
            eligible: true,
            echo_enabled: true,
            alternate_screen: false,
        },
        resources,
    }
}

fn visible(term: &Terminal) -> Vec<tattoy_wezterm_term::Line> {
    let screen = term.screen();
    screen.lines_in_phys_range(screen.phys_row(0)..screen.phys_row(screen.physical_rows as i64))
}

fn printable(ch: char) -> InputEvent {
    InputEvent::Key(Key {
        code: KeyCode::Char(ch),
        modifiers: 0,
        phase: KeyPhase::Press,
        shifted: None,
        base: None,
        text: None,
    })
}

fn feed(decoder: &mut InputDecoder, bytes: &[u8]) -> Result<Vec<LocalEvent>> {
    let mut events = Vec::new();
    for chunk in bytes.chunks(MAX_INPUT_CHUNK) {
        let batch = decoder.feed(chunk, PasteAdmission::Capture, Instant::now())?;
        assert!(batch.len() <= MAX_INPUT_CHUNK + 1);
        events.extend(batch);
    }
    Ok(events)
}

fn verify_legacy_input() -> Result<()> {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(30);
    for (prefix, code, modifiers) in [
        (b"\x1b".as_slice(), KeyCode::Named("Escape".into()), 0),
        (b"\x1b[".as_slice(), KeyCode::Char('['), 2),
        (b"\x1bO".as_slice(), KeyCode::Char('O'), 2),
    ] {
        let expected = LocalEvent::Input(InputEvent::Key(Key {
            code,
            modifiers,
            phase: KeyPhase::Press,
            shifted: None,
            base: None,
            text: None,
        }));
        let mut decoder = InputDecoder::new();
        assert!(
            decoder
                .feed(prefix, PasteAdmission::Capture, start)?
                .is_empty()
        );
        assert!(decoder.poll(deadline - Duration::from_nanos(1)).is_empty());
        assert_eq!(decoder.poll(deadline), vec![expected.clone()]);
        assert!(decoder.poll(deadline + Duration::from_secs(1)).is_empty());
        assert_eq!(
            decoder.feed(b"x", PasteAdmission::Capture, deadline)?,
            vec![LocalEvent::Input(printable('x'))]
        );
        let mut decoder = InputDecoder::new();
        assert!(
            decoder
                .feed(prefix, PasteAdmission::Capture, start)?
                .is_empty()
        );
        assert_eq!(
            decoder.feed(b"x", PasteAdmission::Capture, deadline)?,
            vec![expected, LocalEvent::Input(printable('x'))]
        );
    }
    for (sequence, name, legacy) in [
        (b"\x1b[[A".as_slice(), "F1", b"\x1bOP".as_slice()),
        (b"\x1b[[B", "F2", b"\x1bOQ"),
        (b"\x1b[[C", "F3", b"\x1bOR"),
        (b"\x1b[[D", "F4", b"\x1bOS"),
        (b"\x1b[[E", "F5", b"\x1b[15~"),
        (b"\x1b[42~", "F21", b"\x1b[42~"),
        (b"\x1b[43~", "F22", b"\x1b[43~"),
        (b"\x1b[44~", "F23", b"\x1b[44~"),
        (b"\x1b[45~", "F24", b"\x1b[45~"),
        (b"\x1b[E", "KeypadBegin", b"\x1b[E"),
        (b"\x1bOE", "KeypadBegin", b"\x1b[E"),
        (b"\x1bOX", "KeypadEqual", b"="),
    ] {
        let event = InputEvent::Key(Key {
            code: KeyCode::Named(name.into()),
            modifiers: 0,
            phase: KeyPhase::Press,
            shifted: None,
            base: None,
            text: None,
        });
        for split in 0..=sequence.len() {
            let mut decoder = InputDecoder::new();
            let mut events = decoder.feed(&sequence[..split], PasteAdmission::Capture, start)?;
            events.extend(decoder.feed(
                &sequence[split..],
                PasteAdmission::Capture,
                deadline - Duration::from_nanos(1),
            )?);
            assert_eq!(
                events,
                vec![LocalEvent::Input(event.clone())],
                "{sequence:?}, split={split}"
            );
            assert!(decoder.poll(deadline).is_empty());
        }
        let mut terminal = TerminalModel::new(Size::default(), false)?;
        assert_eq!(terminal.encode_input(&event)?, legacy, "{name}");
        if name == "KeypadEqual" {
            terminal.advance(b"\x1b=")?;
            assert_eq!(terminal.encode_input(&event)?, b"\x1bOX");
            terminal.advance(b"\x1b[>31u")?;
            assert_eq!(terminal.encode_input(&event)?, b"\x1b[57415;1:1u");
        }
    }
    for (sequence, event) in [
        ("中".as_bytes(), printable('中')),
        (
            b"\x1b[1;5A".as_slice(),
            InputEvent::Key(Key {
                code: KeyCode::Named("Up".into()),
                modifiers: 4,
                phase: KeyPhase::Press,
                shifted: None,
                base: None,
                text: None,
            }),
        ),
        (
            b"\x1b[97:65:113;7:2;233:769u",
            InputEvent::Key(Key {
                code: KeyCode::Char('a'),
                modifiers: 6,
                phase: KeyPhase::Repeat,
                shifted: Some('A'),
                base: Some('q'),
                text: Some("é\u{301}".into()),
            }),
        ),
    ] {
        for split in 0..=sequence.len() {
            let mut decoder = InputDecoder::new();
            let mut events = decoder.feed(&sequence[..split], PasteAdmission::Capture, start)?;
            events.extend(decoder.feed(
                &sequence[split..],
                PasteAdmission::Capture,
                deadline - Duration::from_nanos(1),
            )?);
            assert_eq!(
                events,
                vec![LocalEvent::Input(event.clone())],
                "{sequence:?}, split={split}"
            );
        }
    }
    for prefix in [b"\x1b[12;".as_slice(), b"\x1b[?1;", b"\x1bO1;", b"\x1b[["] {
        let mut decoder = InputDecoder::new();
        assert!(
            decoder
                .feed(prefix, PasteAdmission::Capture, start)?
                .is_empty()
        );
        assert_eq!(
            decoder.poll(deadline),
            vec![LocalEvent::Rejected(InputRejection::SequenceTimeout)]
        );
        assert!(decoder.poll(deadline + Duration::from_secs(1)).is_empty());
        for _ in 0..32 {
            assert!(
                decoder
                    .feed(&[b'1'; MAX_INPUT_CHUNK], PasteAdmission::Capture, deadline)?
                    .is_empty()
            );
        }
        assert!(
            decoder
                .feed(b"R", PasteAdmission::Capture, deadline)?
                .is_empty()
        );
        assert_eq!(
            decoder.feed(b"z", PasteAdmission::Capture, deadline)?,
            vec![LocalEvent::Input(printable('z'))]
        );
    }
    let mut slow = InputDecoder::new();
    for (bytes, millis) in [(b"\x1b".as_slice(), 0), (b"[", 10), (b"1", 20), (b"2;", 29)] {
        assert!(
            slow.feed(
                bytes,
                PasteAdmission::Capture,
                start + Duration::from_millis(millis)
            )?
            .is_empty()
        );
    }
    assert_eq!(
        slow.feed(b"A", PasteAdmission::Capture, deadline)?,
        vec![LocalEvent::Rejected(InputRejection::SequenceTimeout)]
    );
    for (suffix, expected) in [
        (b"?".as_slice(), EscapeAction::Help),
        (b".", EscapeAction::Detach),
        (b"q", EscapeAction::Close),
        (b"\x1a", EscapeAction::Suspend),
    ] {
        let mut decoder = InputDecoder::new();
        assert!(
            decoder
                .feed(b"\x1b[12;", PasteAdmission::Capture, start)?
                .is_empty()
        );
        assert_eq!(
            decoder.poll(deadline),
            vec![LocalEvent::Rejected(InputRejection::SequenceTimeout)]
        );
        let mut keys = vec![0x1e];
        keys.extend_from_slice(suffix);
        let mut escape = EscapeSequence::new();
        let actions: Vec<_> = decoder
            .feed(&keys, PasteAdmission::Capture, deadline)?
            .into_iter()
            .map(|event| {
                let LocalEvent::Input(event) = event else {
                    panic!("management key was not input")
                };
                escape.feed(event)
            })
            .collect();
        assert_eq!(actions, vec![EscapeAction::Send(Vec::new()), expected]);
    }
    let mut decoder = InputDecoder::new();
    assert!(
        decoder
            .feed(b"\x1b[12;", PasteAdmission::Capture, start)?
            .is_empty()
    );
    assert_eq!(
        decoder.poll(deadline),
        vec![LocalEvent::Rejected(InputRejection::SequenceTimeout)]
    );
    let mut escape = EscapeSequence::new();
    let actions: Vec<_> = decoder
        .feed(b"\x1b[94;5u\x1b[63u", PasteAdmission::Capture, deadline)?
        .into_iter()
        .map(|event| {
            let LocalEvent::Input(event) = event else {
                panic!("Kitty recovery was not input")
            };
            escape.feed(event)
        })
        .collect();
    assert_eq!(
        actions,
        vec![EscapeAction::Send(Vec::new()), EscapeAction::Help]
    );
    let mut maximal = InputDecoder::new();
    let mut partial = b"\x1b[".to_vec();
    partial.extend_from_slice(&vec![b'1'; 4094]);
    for chunk in partial.chunks(MAX_INPUT_CHUNK) {
        assert!(
            maximal
                .feed(chunk, PasteAdmission::Capture, start)?
                .is_empty()
        );
    }
    assert_eq!(
        maximal.poll(deadline),
        vec![LocalEvent::Rejected(InputRejection::SequenceTimeout)]
    );
    let mut limit = InputDecoder::new();
    for chunk in partial.chunks(MAX_INPUT_CHUNK) {
        assert!(
            limit
                .feed(chunk, PasteAdmission::Capture, start)?
                .is_empty()
        );
    }
    assert!(limit.feed(b"1", PasteAdmission::Capture, start).is_err());
    let mut capture = InputDecoder::new();
    assert!(
        capture
            .feed(b"\x1b", PasteAdmission::Capture, start)?
            .is_empty()
    );
    assert_eq!(
        capture
            .feed(&[b'a'; MAX_INPUT_CHUNK], PasteAdmission::Capture, deadline)?
            .len(),
        MAX_INPUT_CHUNK + 1
    );
    let mut utf8 = InputDecoder::new();
    assert!(
        utf8.feed(b"\x1b\xe4", PasteAdmission::Capture, start)?
            .is_empty()
    );
    assert_eq!(
        utf8.poll(deadline),
        vec![LocalEvent::Input(InputEvent::Key(Key {
            code: KeyCode::Named("Escape".into()),
            modifiers: 0,
            phase: KeyPhase::Press,
            shifted: None,
            base: None,
            text: None
        }))]
    );
    assert_eq!(
        utf8.feed(b"\xb8\xad", PasteAdmission::Capture, deadline)?,
        vec![LocalEvent::Input(printable('中'))]
    );
    for (prefix, terminator) in [
        (b"\x1b]777;\x1e?".as_slice(), b"\x1b\\".as_slice()),
        (b"\x1bP\x1e?", b"\x1b\\"),
        (b"\x1b_\x1e?", b"\x1b\\"),
    ] {
        let mut reply = InputDecoder::new();
        assert!(
            reply
                .feed(prefix, PasteAdmission::Capture, start)?
                .is_empty()
        );
        assert!(reply.poll(start + Duration::from_secs(1)).is_empty());
        assert!(
            reply
                .feed(
                    terminator,
                    PasteAdmission::Capture,
                    start + Duration::from_secs(1)
                )?
                .is_empty()
        );
    }
    let mut paste = InputDecoder::new();
    assert!(
        paste
            .feed(b"\x1b[200~\x1e?", PasteAdmission::Capture, start)?
            .is_empty()
    );
    assert!(paste.poll(start + Duration::from_secs(1)).is_empty());
    assert_eq!(
        paste.feed(
            b"\x1b[201~",
            PasteAdmission::Capture,
            start + Duration::from_secs(1)
        )?,
        vec![LocalEvent::Input(InputEvent::Paste("\x1e?".into()))]
    );
    let mut reply = InputDecoder::new();
    assert!(
        reply
            .feed(b"\x1b]52;c;YQ", PasteAdmission::Capture, start)?
            .is_empty()
    );
    assert!(reply.poll(start + Duration::from_secs(1)).is_empty());
    assert_eq!(
        reply.feed(
            b"==\x1b\\",
            PasteAdmission::Capture,
            start + Duration::from_secs(1)
        )?,
        vec![LocalEvent::Clipboard {
            selection: "c".into(),
            data: Some("a".into())
        }]
    );
    let mut x10 = InputDecoder::new();
    assert!(
        x10.feed(b"\x1b[M", PasteAdmission::Capture, start)?
            .is_empty()
    );
    let late = deadline + Duration::from_millis(1);
    let mut mouse = x10.poll(late);
    mouse.extend(x10.feed(&[32, 65, 66], PasteAdmission::Capture, late)?);
    assert_eq!(
        mouse,
        vec![LocalEvent::Input(InputEvent::Mouse(Mouse {
            x: 32,
            y: 33,
            pixel_x: 0,
            pixel_y: 0,
            modifiers: 0,
            kind: MouseKind::Press(0)
        }))]
    );
    assert!(feed(&mut InputDecoder::new(), b"\x1b[24;12R\x1b[?1;2c\x1b[4;1$y")?.is_empty());
    println!(
        "legacy input: absolute deadlines, late tails, Linux F1-F5, F21-F24, keypad, recovery and reply/paste boundaries verified"
    );
    Ok(())
}

fn verify_input() -> Result<()> {
    verify_legacy_input()?;
    let mut decoder = InputDecoder::new();
    let events = feed(
        &mut decoder,
        b"\x1b[97:65:113;7:2;233:769u\x1b[97;1:3u\x1b[1;5A\x1b[200~hello\n",
    )?;
    assert_eq!(
        events[0],
        LocalEvent::Input(InputEvent::Key(Key {
            code: KeyCode::Char('a'),
            modifiers: 6,
            phase: KeyPhase::Repeat,
            shifted: Some('A'),
            base: Some('q'),
            text: Some("é\u{301}".into())
        }))
    );
    assert!(matches!(
        &events[1],
        LocalEvent::Input(InputEvent::Key(Key {
            phase: KeyPhase::Release,
            ..
        }))
    ));
    assert_eq!(
        events[2],
        LocalEvent::Input(InputEvent::Key(Key {
            code: KeyCode::Named("Up".into()),
            modifiers: 4,
            phase: KeyPhase::Press,
            shifted: None,
            base: None,
            text: None
        }))
    );
    assert_eq!(events.len(), 3);
    assert!(feed(&mut decoder, b"world\x1b[20")?.is_empty());
    assert_eq!(
        feed(&mut decoder, b"1~")?,
        vec![LocalEvent::Input(InputEvent::Paste("hello\nworld".into()))]
    );
    assert!(feed(&mut decoder, &[0xf0, 0x9f])?.is_empty());
    assert_eq!(
        feed(&mut decoder, &[0x98, 0x83])?,
        vec![LocalEvent::Input(printable('😃'))]
    );
    let now = Instant::now();
    assert!(
        decoder
            .feed(b"\x1b", PasteAdmission::Capture, now)?
            .is_empty()
    );
    assert!(decoder.poll(now + Duration::from_millis(29)).is_empty());
    assert_eq!(
        decoder.poll(now + Duration::from_millis(30)),
        vec![LocalEvent::Input(InputEvent::Key(Key {
            code: KeyCode::Named("Escape".into()),
            modifiers: 0,
            phase: KeyPhase::Press,
            shifted: None,
            base: None,
            text: None
        }))]
    );
    let events = feed(
        &mut decoder,
        b"\x1b[<4;3;2M\x1b[<0;3;2m\x1b[I\x1b[O\x1b]52;c;aGVsbG8=\x1b\\",
    )?;
    assert_eq!(
        events[0],
        LocalEvent::Input(InputEvent::Mouse(Mouse {
            x: 2,
            y: 1,
            pixel_x: 0,
            pixel_y: 0,
            modifiers: 1,
            kind: MouseKind::Press(0)
        }))
    );
    assert_eq!(
        events[1],
        LocalEvent::Input(InputEvent::Mouse(Mouse {
            x: 2,
            y: 1,
            pixel_x: 0,
            pixel_y: 0,
            modifiers: 0,
            kind: MouseKind::Release(0)
        }))
    );
    assert_eq!(events[2], LocalEvent::Input(InputEvent::Focus(true)));
    assert_eq!(events[3], LocalEvent::Input(InputEvent::Focus(false)));
    assert_eq!(
        events[4],
        LocalEvent::Clipboard {
            selection: "c".into(),
            data: Some("hello".into())
        }
    );
    let i = Key {
        code: KeyCode::Char('i'),
        modifiers: 0,
        phase: KeyPhase::Press,
        shifted: None,
        base: None,
        text: Some("i".into()),
    };
    let escape = Key {
        code: KeyCode::Named("Escape".into()),
        text: None,
        ..i.clone()
    };
    let expected = vec![
        LocalEvent::Input(InputEvent::Key(i.clone())),
        LocalEvent::Input(InputEvent::Key(Key {
            phase: KeyPhase::Release,
            text: None,
            ..i
        })),
        LocalEvent::Input(InputEvent::Key(escape.clone())),
        LocalEvent::Input(InputEvent::Key(Key {
            phase: KeyPhase::Release,
            ..escape
        })),
    ];
    let mut ghostty = InputDecoder::new();
    assert!(feed(&mut ghostty, b"\x1b[105;")?.is_empty());
    let mut events = feed(&mut ghostty, b";105u\x1b[105;1:")?;
    events.extend(feed(&mut ghostty, b"3u\x1b[27u\x1b[27;1:3u")?);
    assert_eq!(events, expected);
    let mut legacy = TerminalModel::new(Size::default(), false)?;
    let mut kitty = TerminalModel::new(Size::default(), false)?;
    kitty.advance(b"\x1b[>31u")?;
    let mut legacy_bytes = Vec::new();
    let mut kitty_bytes = Vec::new();
    for event in &events {
        let LocalEvent::Input(event) = event else {
            unreachable!();
        };
        legacy_bytes.extend(legacy.encode_input(event)?);
        kitty_bytes.extend(kitty.encode_input(event)?);
    }
    assert_eq!(legacy_bytes, b"i\x1b");
    assert_eq!(
        kitty_bytes,
        b"\x1b[105;1:1;105u\x1b[105;1:3u\x1b[27;1:1u\x1b[27;1:3u"
    );
    assert_eq!(feed(&mut InputDecoder::new(), &kitty_bytes)?, expected);
    for (sequence, name, modifiers, phase) in [
        ("\x1bOA", "Up", 0, KeyPhase::Press),
        ("\x1b[1;1:1A", "Up", 0, KeyPhase::Press),
        ("\x1b[1;1:2A", "Up", 0, KeyPhase::Repeat),
        ("\x1b[1;1:3A", "Up", 0, KeyPhase::Release),
        ("\x1b[1;5:3D", "Left", 4, KeyPhase::Release),
        ("\x1b[3;2:3~", "Delete", 1, KeyPhase::Release),
        ("\x1b[15;1:2~", "F5", 0, KeyPhase::Repeat),
        ("\x1b[1;1:3R", "F3", 0, KeyPhase::Release),
        ("\x1b[1;1:3Z", "Tab", 1, KeyPhase::Release),
    ] {
        let mut decoder = InputDecoder::new();
        let split = sequence.len() - 2;
        assert!(feed(&mut decoder, &sequence.as_bytes()[..split])?.is_empty());
        assert_eq!(
            feed(&mut decoder, &sequence.as_bytes()[split..])?,
            vec![LocalEvent::Input(InputEvent::Key(Key {
                code: KeyCode::Named(name.into()),
                modifiers,
                phase,
                shifted: None,
                base: None,
                text: None,
            }))],
            "{sequence:?}"
        );
    }
    assert!(feed(&mut InputDecoder::new(), b"\x1b[4;1$y")?.is_empty());
    for invalid in [
        b"\x1b[97;0u".as_slice(),
        b"\x1b[97;257u".as_slice(),
        b"\x1b[97;1:4u".as_slice(),
        b"\x1b[1;257A".as_slice(),
        b"\x1b[1;1:4A".as_slice(),
    ] {
        assert!(feed(&mut InputDecoder::new(), invalid).is_err());
    }
    let mut functional = InputDecoder::new();
    let events = feed(&mut functional, b"\x1b[57399;1u\x1b[57428;1u\x1b[1;5R")?;
    assert!(
        matches!(&events[0], LocalEvent::Input(InputEvent::Key(Key { code: KeyCode::Named(name), .. })) if name == "Keypad0")
    );
    assert!(
        matches!(&events[1], LocalEvent::Input(InputEvent::Key(Key { code: KeyCode::Named(name), .. })) if name == "MediaPlay")
    );
    assert!(
        matches!(&events[2], LocalEvent::Input(InputEvent::Key(Key { code: KeyCode::Named(name), modifiers: 4, .. })) if name == "F3")
    );
    let events = feed(
        &mut functional,
        b"\x1b[<66;2;1M\x1b[<67;2;1M\x1b[<35;2;1M\x1b[<128;2;1M",
    )?;
    assert!(matches!(
        &events[0],
        LocalEvent::Input(InputEvent::Mouse(Mouse {
            kind: MouseKind::WheelHorizontal(1),
            ..
        }))
    ));
    assert!(matches!(
        &events[1],
        LocalEvent::Input(InputEvent::Mouse(Mouse {
            kind: MouseKind::WheelHorizontal(-1),
            ..
        }))
    ));
    assert!(matches!(
        &events[2],
        LocalEvent::Input(InputEvent::Mouse(Mouse {
            kind: MouseKind::Move,
            ..
        }))
    ));
    assert_eq!(
        events.len(),
        3,
        "unsupported extra mouse buttons must not become left clicks"
    );
    assert!(
        InputDecoder::new()
            .feed(
                &[b'a'; MAX_INPUT_CHUNK + 1],
                PasteAdmission::Capture,
                Instant::now()
            )
            .is_err()
    );
    let mut maximal = InputDecoder::new();
    assert!(feed(&mut maximal, b"\x1b[200~")?.is_empty());
    assert!(feed(&mut maximal, &vec![b'a'; MAX_PENDING_INPUT / 2])?.is_empty());
    let mut tail = b"\x1b[201~".to_vec();
    tail.extend_from_slice(&[b'z'; MAX_INPUT_CHUNK - 6]);
    let events = maximal.feed(&tail, PasteAdmission::Capture, Instant::now())?;
    assert_eq!(events.len(), MAX_INPUT_CHUNK - 5);
    assert_eq!(
        events[0],
        LocalEvent::Input(InputEvent::Paste("a".repeat(MAX_PENDING_INPUT / 2)))
    );
    assert_eq!(events[1], LocalEvent::Input(printable('z')));
    for split in 0..=6 {
        let mut bounded = InputDecoder::new();
        assert!(feed(&mut bounded, b"\x1b[200~")?.is_empty());
        assert_eq!(
            feed(&mut bounded, &vec![b'a'; MAX_PENDING_INPUT / 2 + 1])?,
            vec![LocalEvent::Rejected(InputRejection::PasteTooLarge)]
        );
        assert!(
            feed(&mut bounded, b"\x1e.\x1eq\x1e?\x1e\x1a")?.is_empty(),
            "discarded paste must not execute management keys"
        );
        assert!(feed(&mut bounded, &b"\x1b[201~"[..split])?.is_empty());
        assert!(feed(&mut bounded, &b"\x1b[201~"[split..])?.is_empty());
        let mut escape = EscapeSequence::new();
        let mut local = Vec::new();
        for event in feed(&mut bounded, b"\x1e?\x1e.")? {
            if let LocalEvent::Input(event) = event {
                local.push(escape.feed(event));
            }
        }
        assert_eq!(
            local,
            vec![
                EscapeAction::Send(Vec::new()),
                EscapeAction::Help,
                EscapeAction::Send(Vec::new()),
                EscapeAction::Detach
            ]
        );
    }
    let mut interrupted = InputDecoder::new();
    assert!(feed(&mut interrupted, b"\x1b[200~held")?.is_empty());
    assert!(interrupted.discard_active_paste());
    assert!(!interrupted.discard_active_paste());
    assert!(feed(&mut interrupted, b"tail\x1e.\x1b[201~")?.is_empty());
    assert!(
        interrupted
            .feed(
                b"\x1b[200~\x1eq\x1b[201~",
                PasteAdmission::Discard,
                Instant::now()
            )?
            .is_empty()
    );
    assert_eq!(
        feed(&mut interrupted, b"\x1b[200~\xff\x1b[201~")?,
        vec![LocalEvent::Rejected(InputRejection::InvalidPaste)]
    );
    assert_eq!(
        feed(&mut interrupted, b"\x1b[200~\x1e.\x1b[201~")?,
        vec![LocalEvent::Input(InputEvent::Paste("\x1e.".into()))]
    );
    let mut fragmented = InputDecoder::new();
    assert!(feed(&mut fragmented, &[0xf0, 0x9f])?.is_empty());
    let mut completing = vec![0x98, 0x83];
    completing.extend_from_slice(&[b'a'; MAX_INPUT_CHUNK - 2]);
    assert_eq!(
        fragmented
            .feed(&completing, PasteAdmission::Capture, Instant::now())?
            .len(),
        MAX_INPUT_CHUNK - 1
    );
    println!(
        "input: kitty phases/alternates/text, legacy keys, fragmented UTF-8/paste, mouse/focus, clipboard and bounds verified"
    );
    Ok(())
}

fn verify_render(graphics: Graphics) -> Result<()> {
    let rgba = vec![255, 0, 0, 255];
    let digest = resource_digest(1, 1, &rgba);
    let image = Resource {
        meta: ResourceMeta {
            digest,
            width: 1,
            height: 1,
            bytes: 4,
        },
        rgba: rgba.into(),
    };
    let mut resources = BTreeMap::new();
    resources.insert(digest, image.clone());
    let mut first = frame(1, None, vec![image.meta.clone()]);
    let mut red = cell(0, "R", 1);
    red.style.fg = Color::Rgb([12, 34, 56]);
    red.style.underline = 3;
    red.style.underline_color = Color::Rgb([200, 100, 50]);
    let mut link = cell(1, "L", 1);
    link.style.hyperlink = Some(Hyperlink {
        params: "id=verify".into(),
        url: "https://example.com/path".into(),
    });
    first.rows[0].row.cells = vec![
        red,
        link,
        cell(2, "e\u{301}", 1),
        cell(3, "中", 2),
        cell(5, "😃", 2),
    ];
    let mut tile = cell(0, " ", 1);
    tile.images.push(ImageTile {
        resource: digest,
        source: [0, 0, 1, 1],
        padding: [0; 4],
        z: 1,
        placement: 9,
    });
    first.rows[2].row.cells.push(tile);
    let mut display = Display::new(Vec::new(), graphics, PredictionPolicy::Adaptive);
    display.set_epoch(1)?;
    assert!(display.install(&first, &resources)?);
    assert_eq!(display.screen().unwrap().rows[0], first.rows[0].row);
    display.predict(1, &printable('X'))?;
    let mut second = frame(2, Some(1), vec![image.meta.clone()]);
    second.echo_through = 1;
    second.rows = vec![RowPatch {
        y: 1,
        row: Row {
            cells: vec![cell(8, "Y", 1)],
            stable_id: 1,
        },
    }];
    assert!(display.install(&second, &resources)?);
    assert!(!display.install(&first, &resources)?);
    let effect = Effect {
        id: 1,
        after: 2,
        kind: EffectKind::ClipboardRead {
            query: 7,
            selection: "c".into(),
        },
    };
    assert_eq!(
        display.apply_effect(&effect, false)?,
        Some(InputEvent::QueryResult {
            query: 7,
            data: None
        })
    );
    assert_eq!(display.apply_effect(&effect, false)?, None);
    let write = Effect {
        id: 2,
        after: 2,
        kind: EffectKind::ClipboardWrite {
            selection: "c".into(),
            data: "hello".into(),
        },
    };
    display.apply_effect(&write, false)?;
    let mut invalid = second;
    invalid.revision = 3;
    invalid.base = Some(2);
    invalid.rows[0].row.cells[0].text = "\x1b[2J".into();
    assert!(display.install(&invalid, &resources).is_err());
    let (cached, output) = display.into_cached();
    assert!(
        output
            .windows(b"\x1b]52;c;aGVsbG8=\x1b\\".len())
            .any(|bytes| bytes == b"\x1b]52;c;aGVsbG8=\x1b\\")
    );
    let mut display = cached.bind(Vec::new());
    display.redraw_cached(Size {
        cols: 4,
        rows: 2,
        ..first.size
    })?;
    assert_eq!(display.screen().unwrap().size, first.size);
    let (cached, _) = display.into_cached();
    let mut display = cached.bind(Vec::new());
    display.redraw_cached(first.size)?;
    assert_eq!(display.revision(), 2);
    assert_eq!(display.effects_applied(), 2);
    assert_eq!(display.apply_effect(&write, false)?, None);
    let redraw = display.into_inner();
    assert!(!redraw.windows(5).any(|bytes| bytes == b"\x1b]52;"));
    assert!(!redraw.windows(6).any(|bytes| bytes == b"\x1b]133;"));
    assert!(redraw.windows(4).any(|bytes| bytes == b"\x1b]2;"));
    assert!(redraw.windows(4).any(|bytes| bytes == b"\x1b]7;"));
    let mut term = terminal();
    term.advance_bytes(b"\x1b[?7l");
    term.advance_bytes(&redraw);
    let lines = visible(&term);
    let cells: Vec<_> = lines[0].visible_cells().collect();
    assert_eq!(cells[0].str(), "R");
    match cells[0].attrs().foreground() {
        tattoy_wezterm_term::color::ColorAttribute::TrueColorWithDefaultFallback(color) => {
            assert_eq!(color.to_srgb_u8(), (12, 34, 56, 255));
        }
        other => panic!("unexpected foreground {other:?}"),
    }
    assert_eq!(
        cells[0].attrs().underline(),
        tattoy_wezterm_term::Underline::Curly
    );
    assert_eq!(
        cells[1].attrs().hyperlink().unwrap().uri(),
        "https://example.com/path"
    );
    assert_eq!(
        cells[1]
            .attrs()
            .hyperlink()
            .unwrap()
            .params()
            .get("id")
            .map(String::as_str),
        Some("verify")
    );
    assert_eq!(cells[2].str(), "e\u{301}");
    assert_eq!(cells[3].str(), "中");
    assert_eq!(
        lines[1]
            .visible_cells()
            .find(|cell| cell.cell_index() == 8)
            .unwrap()
            .str(),
        "Y"
    );
    if graphics != Graphics::None {
        let images = lines[2]
            .visible_cells()
            .next()
            .unwrap()
            .attrs()
            .images()
            .unwrap();
        assert_eq!(images[0].image_data().data().dimensions()?, (10, 20));
        let decoded = images[0].image_data().data().clone().decode();
        match decoded {
            tattoy_wezterm_term::image::ImageDataType::Rgba8 { data, .. } => {
                assert!(
                    data.as_chunks::<4>()
                        .0
                        .iter()
                        .all(|pixel| *pixel == [255, 0, 0, 255])
                );
            }
            other => panic!("expected static RGBA image, got {other:?}"),
        }
    }
    println!(
        "renderer {graphics:?}: graphemes, true color, underline, hyperlink parameters, effects and prediction rollback verified through terminal core"
    );
    Ok(())
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn verify_prediction_modes() -> Result<()> {
    let capture = Capture::default();
    let mut display = Display::new(capture.clone(), Graphics::None, PredictionPolicy::Adaptive);
    display.set_epoch(1)?;
    let mut current = frame(1, None, Vec::new());
    current.prediction.echo_enabled = false;
    current.prediction.alternate_screen = true;
    assert!(display.install(&current, &BTreeMap::new())?);
    for (index, text) in ['a', 'b', 'c'].into_iter().enumerate() {
        let before = capture.0.lock().unwrap().len();
        display.predict(index as u64 + 1, &printable(text))?;
        assert_eq!(
            capture.0.lock().unwrap().len(),
            before,
            "unlearned no-echo input must not be predicted"
        );
        current.base = Some(current.revision);
        current.revision += 1;
        current.echo_through = index as u64 + 1;
        current.cursor.x += 1;
        current.rows = vec![RowPatch {
            y: 1,
            row: Row {
                cells: ['a', 'b', 'c'][..=index]
                    .iter()
                    .enumerate()
                    .map(|(x, ch)| cell(x as u16 + 8, &ch.to_string(), 1))
                    .collect(),
                stable_id: 1,
            },
        }];
        assert!(display.install(&current, &BTreeMap::new())?);
    }
    let before = capture.0.lock().unwrap().len();
    display.predict(4, &printable('d'))?;
    assert!(
        capture.0.lock().unwrap().len() > before,
        "learned alternate-screen echo may be predicted"
    );
    current.base = Some(current.revision);
    current.revision += 1;
    current.echo_through = 4;
    current.prediction.generation += 1;
    assert!(display.install(&current, &BTreeMap::new())?);
    let mut term = terminal();
    term.advance_bytes(&*capture.0.lock().unwrap());
    let lines = visible(&term);
    assert_eq!(
        lines[1]
            .visible_cells()
            .find(|cell| cell.cell_index() == 11)
            .unwrap()
            .str(),
        " "
    );
    let before = capture.0.lock().unwrap().len();
    display.predict(5, &printable('S'))?;
    assert_eq!(
        capture.0.lock().unwrap().len(),
        before,
        "mode change must discard learned prediction eligibility"
    );
    println!(
        "prediction: no-echo suppression, learned TUI echo, and mode-change rollback verified"
    );
    Ok(())
}

fn learned_prediction() -> Result<(Display<Capture>, Capture, Frame)> {
    let capture = Capture::default();
    let mut display = Display::new(capture.clone(), Graphics::None, PredictionPolicy::Adaptive);
    display.set_epoch(1)?;
    let mut current = frame(1, None, Vec::new());
    current.prediction.echo_enabled = false;
    assert!(display.install(&current, &BTreeMap::new())?);
    for (index, glyph) in ['a', 'b', 'c'].into_iter().enumerate() {
        display.predict(index as u64 + 1, &printable(glyph))?;
        current.base = Some(current.revision);
        current.revision += 1;
        current.echo_through = index as u64 + 1;
        current.rows[1]
            .row
            .cells
            .push(cell(8 + index as u16, &glyph.to_string(), 1));
        current.cursor.x += 1;
        assert!(display.install(&current, &BTreeMap::new())?);
    }
    let before = capture.0.lock().unwrap().len();
    display.predict(4, &printable('d'))?;
    assert!(
        capture.0.lock().unwrap().len() > before,
        "three real echoes must enable prediction"
    );
    Ok((display, capture, current))
}

fn verify_cached_redraw() -> Result<()> {
    let (mut display, capture, current) = learned_prediction()?;
    let canonical = display.screen().unwrap().clone();
    let before = capture.0.lock().unwrap().len();
    display.redraw_cached(current.size)?;
    let redraw = capture.0.lock().unwrap()[before..].to_vec();
    assert_eq!(
        redraw
            .windows(4)
            .filter(|bytes| *bytes == b"\x1b[2J")
            .count(),
        1
    );
    assert_eq!(display.screen().unwrap().rows, canonical.rows);
    assert_eq!(display.screen().unwrap().size, canonical.size);
    assert_eq!(display.screen().unwrap().cursor, canonical.cursor);
    assert_eq!(display.epoch(), 1);
    assert_eq!(display.revision(), current.revision);
    assert_eq!(display.installed_echo_through(), Some(3));
    let mut term = terminal();
    term.advance_bytes(&*capture.0.lock().unwrap());
    assert_eq!(visible(&term)[1].get_cell(11).unwrap().str(), " ");
    assert_eq!((term.cursor_pos().x, term.cursor_pos().y), (11, 1));
    let before = capture.0.lock().unwrap().len();
    display.predict(5, &printable('S'))?;
    assert_eq!(capture.0.lock().unwrap().len(), before);
    println!(
        "direct cached redraw cleared active prediction and confidence; full redraw bytes={}",
        redraw.len()
    );

    for graphics in [Graphics::Kitty, Graphics::Sixel] {
        let capture = Capture::default();
        let mut display = Display::new(capture.clone(), graphics, PredictionPolicy::Adaptive);
        display.set_epoch(1)?;
        let rgba = vec![255, 0, 0, 255];
        let digest = resource_digest(1, 1, &rgba);
        let resource = Resource {
            meta: ResourceMeta {
                digest,
                width: 1,
                height: 1,
                bytes: 4,
            },
            rgba: rgba.into(),
        };
        let mut current = frame(1, None, vec![resource.meta.clone()]);
        let mut tile = cell(0, " ", 1);
        tile.images.push(ImageTile {
            resource: digest,
            source: [0, 0, 1, 1],
            padding: [0; 4],
            z: 1,
            placement: 1,
        });
        current.rows[2].row.cells.push(tile);
        let resources = BTreeMap::from([(digest, resource)]);
        assert!(display.install(&current, &resources)?);
        for (id, kind) in [
            EffectKind::Bell,
            EffectKind::ClipboardWrite {
                selection: "c".into(),
                data: "redraw".into(),
            },
            EffectKind::Notification {
                title: "redraw".into(),
                body: "once".into(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            display.apply_effect(
                &Effect {
                    id: id as u64 + 1,
                    after: 1,
                    kind,
                },
                false,
            )?;
        }
        display.predict(1, &printable('X'))?;
        let canonical = display.screen().unwrap().clone();
        let before = capture.0.lock().unwrap().len();
        display.redraw_cached(current.size)?;
        let redraw = capture.0.lock().unwrap()[before..].to_vec();
        for forbidden in [b"\x07".as_slice(), b"\x1b]52;", b"\x1b]777;", b"\x1b]133;"] {
            assert!(
                !redraw
                    .windows(forbidden.len())
                    .any(|bytes| bytes == forbidden)
            );
        }
        assert!(redraw.windows(4).any(|bytes| bytes == b"\x1b]2;"));
        assert!(redraw.windows(4).any(|bytes| bytes == b"\x1b]7;"));
        assert_eq!(display.screen().unwrap().rows, canonical.rows);
        assert_eq!(display.screen().unwrap().size, canonical.size);
        assert_eq!(display.screen().unwrap().cursor, canonical.cursor);
        assert_eq!(display.effects_applied(), 3);
        assert_eq!(display.installed_echo_through(), Some(0));
        let mut term = terminal();
        term.advance_bytes(&redraw);
        let lines = visible(&term);
        assert_eq!(
            lines[1]
                .get_cell(8)
                .map(|cell| cell.str().to_owned())
                .unwrap_or_else(|| " ".into()),
            " "
        );
        let image = &lines[2].get_cell(0).unwrap().attrs().images().unwrap()[0];
        assert_eq!(image.image_data().data().dimensions()?, (10, 20));
        match image.image_data().data().clone().decode() {
            tattoy_wezterm_term::image::ImageDataType::Rgba8 { data, .. } => assert!(
                data.as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| *pixel == [255, 0, 0, 255])
            ),
            other => panic!("unexpected cached image {other:?}"),
        }
        assert_eq!((term.cursor_pos().x, term.cursor_pos().y), (8, 1));
        println!(
            "direct cached {graphics:?} redraw restored image and cursor without effects; bytes={}",
            redraw.len()
        );
    }

    let capture = Capture::default();
    let mut display = Display::new(capture.clone(), Graphics::None, PredictionPolicy::Never);
    display.write_status("Connecting.")?;
    let before = capture.0.lock().unwrap().len();
    display.redraw_cached(Size::default())?;
    let redraw = capture.0.lock().unwrap()[before..].to_vec();
    assert!(!redraw.windows(4).any(|bytes| bytes == b"\x1b[2J"));
    assert!(display.screen().is_none());
    assert_eq!(display.revision(), 0);
    let mut term = terminal();
    term.advance_bytes(&redraw);
    assert_eq!(visible(&term)[0].get_cell(0).unwrap().str(), "C");
    Ok(())
}

fn verify_redraw_transcript(arguments: &[String]) -> Result<()> {
    anyhow::ensure!(
        arguments.len() == 2 || arguments.len() == 5,
        "expected transcript, fixed|broken, optional columns rows resize-offset"
    );
    let bytes = std::fs::read(&arguments[0])?;
    let fixed = match arguments[1].as_str() {
        "fixed" => true,
        "broken" => false,
        _ => anyhow::bail!("expected fixed or broken transcript"),
    };
    let size = TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 800,
        pixel_height: 480,
        dpi: 96,
    };
    let mut term = Terminal::new(
        size,
        Arc::new(Config),
        "mosh-redraw-replay",
        "0",
        Box::new(io::sink()),
    );
    if arguments.len() == 5 {
        let cols = arguments[2].parse()?;
        let rows = arguments[3].parse()?;
        let offset: usize = arguments[4].parse()?;
        anyhow::ensure!(offset <= bytes.len(), "resize offset outside transcript");
        term.advance_bytes(&bytes[..offset]);
        term.resize(TerminalSize {
            cols,
            rows,
            pixel_width: cols * 10,
            pixel_height: rows * 20,
            ..size
        });
        term.advance_bytes(&bytes[offset..]);
    } else {
        term.advance_bytes(&bytes);
    }
    let lines = visible(&term);
    let expected = if fixed { "REDRAW_BASE" } else { "BROKEN!!!!!" };
    for (x, glyph) in expected.chars().enumerate() {
        let cell = lines[3].get_cell(x).unwrap();
        assert_eq!(cell.str(), glyph.to_string());
        let expected_fg = if fixed {
            (11, 22, 33, 255)
        } else {
            (99, 88, 77, 255)
        };
        match cell.attrs().foreground() {
            tattoy_wezterm_term::color::ColorAttribute::TrueColorWithDefaultFallback(color) => {
                assert_eq!(color.to_srgb_u8(), expected_fg)
            }
            other => panic!("unexpected redraw foreground {other:?}"),
        }
        if fixed {
            match cell.attrs().background() {
                tattoy_wezterm_term::color::ColorAttribute::TrueColorWithDefaultFallback(color) => {
                    assert_eq!(color.to_srgb_u8(), (44, 55, 66, 255))
                }
                other => panic!("unexpected redraw background {other:?}"),
            }
        }
    }
    let image_cell = lines[5].get_cell(2).unwrap();
    let images = image_cell.attrs().images();
    if fixed {
        let image = &images.unwrap()[0];
        assert_eq!(image.image_data().data().dimensions()?, (10, 20));
        match image.image_data().data().clone().decode() {
            tattoy_wezterm_term::image::ImageDataType::Rgba8 { data, .. } => assert!(
                data.as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| *pixel == [255, 0, 0, 255])
            ),
            other => panic!("unexpected replay image {other:?}"),
        }
    } else {
        assert!(images.is_none_or(|images| images.is_empty()));
    }
    assert_eq!((term.cursor_pos().x, term.cursor_pos().y), (5, 8));
    println!(
        "redraw transcript: exact cells, style, cursor and image fixed={fixed}; native visual unverified"
    );
    Ok(())
}

fn verify_prediction_boundaries() -> Result<()> {
    let capture = Capture::default();
    let mut display = Display::new(capture.clone(), Graphics::None, PredictionPolicy::Adaptive);
    display.set_epoch(1)?;
    let mut current = frame(1, None, Vec::new());
    current.prediction.echo_enabled = false;
    current.rows[1].row.cells.push(cell(8, "a", 1));
    assert!(display.install(&current, &BTreeMap::new())?);
    for seq in 1..=4 {
        let before = capture.0.lock().unwrap().len();
        display.predict(seq, &printable('a'))?;
        assert_eq!(
            capture.0.lock().unwrap().len(),
            before,
            "pre-existing glyphs must not teach echo"
        );
        current.base = Some(current.revision);
        current.revision += 1;
        current.echo_through = seq;
        current.rows[1].row.cells[0].style.flags ^= 1;
        assert!(display.install(&current, &BTreeMap::new())?);
    }
    display.predict(5, &printable('a'))?;
    display.predict(6, &printable('b'))?;
    current.base = Some(current.revision);
    current.revision += 1;
    current.echo_through = 6;
    current.rows[1].row.cells.push(cell(9, "b", 1));
    current.cursor.x = 10;
    assert!(display.install(&current, &BTreeMap::new())?);
    for (seq, glyph) in [(7, 'c'), (8, 'd')] {
        let before = capture.0.lock().unwrap().len();
        display.predict(seq, &printable(glyph))?;
        assert_eq!(capture.0.lock().unwrap().len(), before);
        current.base = Some(current.revision);
        current.revision += 1;
        current.echo_through = seq;
        current.rows[1]
            .row
            .cells
            .push(cell(current.cursor.x, &glyph.to_string(), 1));
        current.cursor.x += 1;
        assert!(display.install(&current, &BTreeMap::new())?);
    }
    let before = capture.0.lock().unwrap().len();
    display.predict(9, &printable('e'))?;
    assert!(
        capture.0.lock().unwrap().len() > before,
        "a pre-existing match must preserve later learnable tokens"
    );

    let mut ctrl = printable('x');
    if let InputEvent::Key(key) = &mut ctrl {
        key.modifiers = 4;
    }
    let controls = ["Enter", "Escape", "Up", "Down"].map(|name| {
        InputEvent::Key(Key {
            code: KeyCode::Named(name.into()),
            modifiers: 0,
            phase: KeyPhase::Press,
            shifted: None,
            base: None,
            text: None,
        })
    });
    for control in
        controls
            .into_iter()
            .chain([ctrl, printable('\n'), InputEvent::Paste("secret".into())])
    {
        let (mut display, capture, _) = learned_prediction()?;
        display.predict(5, &control)?;
        let mut term = terminal();
        term.advance_bytes(&*capture.0.lock().unwrap());
        assert_eq!(
            visible(&term)[1]
                .visible_cells()
                .find(|c| c.cell_index() == 11)
                .unwrap()
                .str(),
            " ",
            "control must remove visible overlay"
        );
        let before = capture.0.lock().unwrap().len();
        display.predict(6, &printable('S'))?;
        assert_eq!(
            capture.0.lock().unwrap().len(),
            before,
            "control must discard learned no-echo confidence: {control:?}"
        );
    }
    let (mut display, capture, mut current) = learned_prediction()?;
    display.set_epoch(2)?;
    let mut term = terminal();
    term.advance_bytes(&*capture.0.lock().unwrap());
    assert_eq!(
        visible(&term)[1]
            .visible_cells()
            .find(|c| c.cell_index() == 11)
            .unwrap()
            .str(),
        " ",
        "session epoch must roll back visible guesses immediately"
    );
    current.epoch = 2;
    current.base = None;
    current.revision += 1;
    current.echo_through = 4;
    assert!(display.install(&current, &BTreeMap::new())?);
    let before = capture.0.lock().unwrap().len();
    display.predict(5, &printable('S'))?;
    assert_eq!(
        capture.0.lock().unwrap().len(),
        before,
        "new session epoch cannot reuse old learning"
    );

    let capture = Capture::default();
    let mut display = Display::new(capture.clone(), Graphics::None, PredictionPolicy::Never);
    display.set_epoch(1)?;
    assert!(display.install(&frame(1, None, Vec::new()), &BTreeMap::new())?);
    let before = capture.0.lock().unwrap().len();
    display.predict(1, &printable('S'))?;
    assert_eq!(
        capture.0.lock().unwrap().len(),
        before,
        "never policy must suppress even PTY echo predictions"
    );
    println!(
        "prediction: pre-existing glyphs, real glyph transitions, control rollback, session epoch and never policy verified"
    );
    Ok(())
}

fn verify_prediction_epoch_context() -> Result<()> {
    for policy in [PredictionPolicy::Adaptive, PredictionPolicy::Conservative] {
        let capture = Capture::default();
        let mut display = Display::new(capture.clone(), Graphics::None, policy);
        display.set_epoch(1)?;
        let mut current = frame(1, None, Vec::new());
        assert!(display.install(&current, &BTreeMap::new())?);
        display.predict(1, &printable('P'))?;
        display.set_epoch(2)?;
        let before = capture.0.lock().unwrap().len();
        display.predict(2, &printable('S'))?;
        assert_eq!(
            capture.0.lock().unwrap().len(),
            before,
            "cached ECHO must not expose input before the new epoch's first frame: {policy:?}"
        );
        current.epoch = 2;
        current.revision += 1;
        current.prediction.generation += 1;
        current.echo_through = 2;
        assert!(display.install(&current, &BTreeMap::new())?);
        let before = capture.0.lock().unwrap().len();
        display.predict(3, &printable('N'))?;
        assert!(
            capture.0.lock().unwrap()[before..]
                .windows(b"\x1b[0;4mN".len())
                .any(|bytes| bytes == b"\x1b[0;4mN"),
            "a confirmed ECHO frame must restore prediction: {policy:?}"
        );

        let capture = Capture::default();
        let mut display = Display::new(capture.clone(), Graphics::None, policy);
        display.set_epoch(1)?;
        let mut current = frame(1, None, Vec::new());
        current.prediction.echo_enabled = false;
        assert!(display.install(&current, &BTreeMap::new())?);
        display.set_epoch(2)?;
        for (seq, glyph) in [(1, 'a'), (2, 'b'), (3, 'c')] {
            display.predict(seq, &printable(glyph))?;
        }
        current.epoch = 2;
        current.revision += 1;
        current.prediction.generation += 1;
        current.echo_through = 3;
        current.rows[1].row.cells = ['a', 'b', 'c']
            .into_iter()
            .enumerate()
            .map(|(index, glyph)| cell(8 + index as u16, &glyph.to_string(), 1))
            .collect();
        current.cursor.x = 11;
        assert!(display.install(&current, &BTreeMap::new())?);
        let before = capture.0.lock().unwrap().len();
        display.predict(4, &printable('S'))?;
        assert_eq!(
            capture.0.lock().unwrap().len(),
            before,
            "input before the first frame must not teach a new remote stamp: {policy:?}"
        );
        display.predict(5, &printable('\n'))?;
        for (seq, glyph) in [(6, 'd'), (7, 'e'), (8, 'f')] {
            display.predict(seq, &printable(glyph))?;
            current.base = Some(current.revision);
            current.revision += 1;
            current.echo_through = seq;
            current.rows[1]
                .row
                .cells
                .push(cell(current.cursor.x, &glyph.to_string(), 1));
            current.cursor.x += 1;
            assert!(display.install(&current, &BTreeMap::new())?);
        }
        let before = capture.0.lock().unwrap().len();
        display.predict(9, &printable('N'))?;
        if policy == PredictionPolicy::Adaptive {
            assert!(
                capture.0.lock().unwrap()[before..]
                    .windows(b"\x1b[0;4mN".len())
                    .any(|bytes| bytes == b"\x1b[0;4mN"),
                "the confirmed new epoch must still learn real noecho replies"
            );
        } else {
            assert_eq!(capture.0.lock().unwrap().len(), before);
        }
        current.base = Some(current.revision);
        current.revision += 1;
        current.prediction.echo_enabled = true;
        assert!(display.install(&current, &BTreeMap::new())?);
        let before = capture.0.lock().unwrap().len();
        display.predict(10, &printable('T'))?;
        assert!(
            capture.0.lock().unwrap()[before..]
                .windows(b"\x1b[0;4mT".len())
                .any(|bytes| bytes == b"\x1b[0;4mT"),
            "prediction must resume after a confirmed ECHO transition: {policy:?}"
        );
    }
    println!(
        "prediction: attachment context gates cached ECHO and learning without disabling confirmed prediction"
    );
    Ok(())
}

fn verify_lifecycle() -> Result<()> {
    let rgba = vec![0, 0, 255, 255];
    let digest = resource_digest(1, 1, &rgba);
    let resource = Resource {
        meta: ResourceMeta {
            digest,
            width: 1,
            height: 1,
            bytes: 4,
        },
        rgba: rgba.into(),
    };
    let resources = BTreeMap::from([(digest, resource.clone())]);
    let mut initial = frame(1, None, vec![resource.meta]);
    let mut tile = cell(0, " ", 1);
    tile.images.push(ImageTile {
        resource: digest,
        source: [0, 0, 1, 1],
        padding: [0; 4],
        z: 1,
        placement: 1,
    });
    initial.rows[0].row.cells.push(tile);
    let mut display = Display::new(Vec::new(), Graphics::Kitty, PredictionPolicy::Never);
    display.set_epoch(1)?;
    assert!(display.install(&initial, &resources)?);
    display.write_status("reconnecting")?;
    display.write_status("")?;
    let mut removed = frame(2, Some(1), Vec::new());
    removed.rows = vec![RowPatch {
        y: 0,
        row: Row::default(),
    }];
    removed.directory = None;
    assert!(display.install(&removed, &BTreeMap::new())?);
    assert!(!display.install(&initial, &resources)?);
    initial.revision = 100;
    initial.epoch = 99;
    assert!(!display.install(&initial, &resources)?);
    let mut mismatch = frame(3, Some(1), Vec::new());
    mismatch.rows.clear();
    assert!(!display.install(&mismatch, &BTreeMap::new())?);
    assert!(display.needs_checkpoint());
    display.set_effect_floor(4);
    display.set_epoch(2)?;
    assert_eq!(display.effects_applied(), 4);
    assert_eq!(display.epoch(), 2);
    let mut checkpoint = frame(4, None, Vec::new());
    checkpoint.epoch = 2;
    checkpoint.directory = None;
    assert!(display.install(&checkpoint, &BTreeMap::new())?);
    assert!(!display.needs_checkpoint());
    assert_eq!(display.revision(), 4);
    let mut term = terminal();
    term.advance_bytes(display.into_inner());
    assert!(visible(&term).iter().all(|line| {
        line.visible_cells()
            .all(|cell| cell.attrs().images().is_none_or(|images| images.is_empty()))
    }));
    assert!(term.get_current_dir().is_none());
    println!(
        "lifecycle: image deletion, stale frames, epoch fencing, checkpoint recovery, effect floor and directory reset verified"
    );
    Ok(())
}

fn escape_actions(bytes: &[u8]) -> Result<(Vec<InputEvent>, Vec<EscapeAction>)> {
    let mut decoder = InputDecoder::new();
    let events: Vec<_> = feed(&mut decoder, bytes)?
        .into_iter()
        .map(|event| {
            let LocalEvent::Input(event) = event else {
                panic!("expected semantic input")
            };
            event
        })
        .collect();
    let mut escape = EscapeSequence::new();
    let actions = events
        .iter()
        .cloned()
        .map(|event| escape.feed(event))
        .collect();
    Ok((events, actions))
}

fn sent_events(actions: Vec<EscapeAction>) -> Vec<InputEvent> {
    actions
        .into_iter()
        .flat_map(|action| match action {
            EscapeAction::Send(events) => events,
            other => panic!("unexpected management action {other:?}"),
        })
        .collect()
}

fn verify_escape_sequences() -> Result<()> {
    let (_, actions) = escape_actions(b"\x1e.")?;
    assert_eq!(
        actions,
        vec![EscapeAction::Send(Vec::new()), EscapeAction::Detach]
    );
    let (_, actions) = escape_actions(b"\x1eq")?;
    assert_eq!(actions[1], EscapeAction::Close);
    let (_, actions) = escape_actions(b"\x1e?")?;
    assert_eq!(actions[1], EscapeAction::Help);
    let (_, actions) = escape_actions(b"\x1e\x1a")?;
    assert_eq!(actions[1], EscapeAction::Suspend);
    let (events, actions) = escape_actions(b"\x1a")?;
    assert_eq!(sent_events(actions), events);
    let (_, actions) = escape_actions(b"\x1b[54;6u\x1b[122;6u")?;
    assert_eq!(actions[1], EscapeAction::Suspend);
    let (_, actions) =
        escape_actions(b"\x1b[94;5u\x1b[94;5:3u\x1b[122;5u\x1b[122;5:2u\x1b[122;5:3u")?;
    assert_eq!(
        actions,
        vec![
            EscapeAction::Send(Vec::new()),
            EscapeAction::Send(Vec::new()),
            EscapeAction::Suspend,
            EscapeAction::Send(Vec::new()),
            EscapeAction::Send(Vec::new()),
        ]
    );
    let (events, actions) = escape_actions(b"\x1ex")?;
    assert_eq!(
        sent_events(actions),
        events,
        "unknown legacy suffix must retain the prefix"
    );
    let (events, actions) = escape_actions(b"\x1e^")?;
    assert_eq!(sent_events(actions), vec![events[0].clone()]);
    let (_, actions) = escape_actions(
        b"\x1b[54;6:1u\x1b[54;6:2u\x1b[54;6:3u\x1b[57442;1:3u\x1b[46;1:1u\x1b[46;1:3u",
    )?;
    assert_eq!(actions[0], EscapeAction::Send(Vec::new()));
    assert_eq!(actions[1], EscapeAction::Send(Vec::new()));
    assert_eq!(actions[2], EscapeAction::Send(Vec::new()));
    assert!(
        matches!(&actions[3], EscapeAction::Send(events) if matches!(&events[0], InputEvent::Key(Key { code: KeyCode::Named(name), phase: KeyPhase::Release, .. }) if name == "LeftControl"))
    );
    assert_eq!(
        actions[4],
        EscapeAction::Detach,
        "prefix release must not cancel the escape"
    );
    assert_eq!(
        actions[5],
        EscapeAction::Send(Vec::new()),
        "consumed suffix release must remain local"
    );
    let (events, actions) =
        escape_actions(b"\x1b[54;6:1u\x1b[54;6:2u\x1b[54;6:3u\x1b[120;1:1u\x1b[120;1:3u")?;
    assert_eq!(
        sent_events(actions),
        events,
        "unknown kitty suffix must replay all prefix phases in order"
    );
    let (events, actions) = escape_actions(b"\x1b[54;6:1u\x1b[54;6:3u\x1b[54;2:1;94u\x1b[54;2:3u")?;
    assert_eq!(
        sent_events(actions),
        events[..2],
        "literal prefix consumes only the second key stroke"
    );
    let (events, actions) = escape_actions(b"\x1b[54;6:1u\x1b[54;6:3u\x1b[113;5:1u")?;
    assert_eq!(
        sent_events(actions),
        events,
        "modified q must remain remote input"
    );
    println!(
        "escape: legacy and kitty commands, prefix phases, modifier releases, literal prefix and unknown-suffix replay verified"
    );
    Ok(())
}

fn main() -> Result<()> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--redraw-transcript")
    {
        return verify_redraw_transcript(&arguments[1..]);
    }
    anyhow::ensure!(arguments.is_empty(), "unknown verify_display arguments");
    let mut display = Display::new(Vec::new(), Graphics::None, PredictionPolicy::Never);
    display.write_status("Connect failed")?;
    let mut term = terminal();
    term.advance_bytes(display.into_inner());
    let status: String = visible(&term)[0]
        .visible_cells()
        .map(|cell| cell.str().to_owned())
        .collect();
    assert!(status.starts_with("Connect failed"), "{status:?}");
    println!("connection errors render before the first remote screen");
    verify_input()?;
    verify_legacy_roundtrip()?;
    verify_image_layers()?;
    verify_render(Graphics::None)?;
    verify_render(Graphics::Kitty)?;
    verify_render(Graphics::Sixel)?;
    verify_prediction_modes()?;
    verify_cached_redraw()?;
    verify_prediction_boundaries()?;
    verify_prediction_epoch_context()?;
    verify_lifecycle()?;
    verify_escape_sequences()?;
    Ok(())
}

fn verify_legacy_roundtrip() -> Result<()> {
    for (mode, mouse_bytes) in [
        (b"\x1b[?1000h".as_slice(), b"\x1b[M !!\x1b[M#!!".as_slice()),
        (
            b"\x1b[?1000h\x1b[?1006h".as_slice(),
            b"\x1b[<0;1;1M\x1b[<0;1;1m".as_slice(),
        ),
    ] {
        let mut model = TerminalModel::new(Size::default(), false)?;
        model.advance(mode)?;
        let mut decoder = InputDecoder::new();
        let mut encoded = Vec::new();
        for event in feed(&mut decoder, b"\n\x08\x1b[M !!\x1b[M#!!")? {
            if let LocalEvent::Input(event) = event {
                encoded.extend(model.encode_input(&event)?);
            }
        }
        assert_eq!(encoded, [b"\n\x08".as_slice(), mouse_bytes].concat());
        assert!(model.encode_input(&InputEvent::Focus(false))?.is_empty());
    }
    println!(
        "legacy Ctrl-J/Ctrl-H and X10 release survive semantic input and both remote mouse encodings"
    );
    Ok(())
}

fn verify_image_layers() -> Result<()> {
    let mut model = TerminalModel::new(Size::default(), false)?;
    for id in 1..=65 {
        model.advance(
            format!("\x1b[H\x1b_Ga=T,f=24,s=1,v=1,i={id},q=2,C=1,z={id};/wAA\x1b\\").as_bytes(),
        )?;
    }
    let screen = model.snapshot()?;
    let frame = state::frame(1, 1, 0, &screen, None)?;
    transport::validate_frame(&frame)?;
    for graphics in [Graphics::None, Graphics::Kitty] {
        let mut display = Display::new(Vec::new(), graphics, PredictionPolicy::Never);
        display.set_epoch(1)?;
        assert!(display.install(&frame, &screen.resources)?);
        assert_eq!(
            display
                .screen()
                .unwrap()
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .map(|cell| cell.images.len())
                .max(),
            Some(65)
        );
    }
    println!(
        "65 static image layers in one cell survive core, frame validation, and display installation"
    );
    Ok(())
}
