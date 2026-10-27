use std::io::{Cursor, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use mosh_rs::model::{
    Attach, BootstrapRequest, ClientMessage, Cursor as ScreenCursor, Effect, EffectKind, Frame,
    Grant, InputEvent, InputRecord, Key, KeyCode, KeyPhase, MAX_CONTROL, MAX_EFFECTS, MAX_FRAME,
    Mouse, MouseKind, PROTOCOL_VERSION, PredictionContext, Resource, ResourceMeta, Row, RowPatch,
    ServerMessage, Size, resource_digest,
};
use mosh_rs::transport::{
    ClientEndpoints, ClientIdentity, EncodedFrame, Incoming, bootstrap_local, connect, listen,
    read_request, receive_uni, recv_message, send_message, send_resource, spawn_worker,
    validate_frame, write_grant,
};
use quinn::{Connection, Endpoint, RecvStream, SendStream, VarInt};

async fn rejected_control<T: serde::de::DeserializeOwned>(
    send: &mut SendStream,
    recv: &mut RecvStream,
    body: &[u8],
) -> Result<()> {
    send.write_all(&(body.len() as u32).to_be_bytes()).await?;
    send.write_all(body).await?;
    ensure!(
        recv_message::<T>(recv).await.is_err(),
        "invalid control message was accepted"
    );
    Ok(())
}

async fn verify_control_messages(
    client: &mut (SendStream, RecvStream),
    server: &mut (SendStream, RecvStream),
    session: &str,
) -> Result<()> {
    let key = Key {
        code: KeyCode::Char('x'),
        modifiers: 0,
        phase: KeyPhase::Press,
        shifted: None,
        base: None,
        text: None,
    };
    send_message(
        &mut client.0,
        &ClientMessage::Input {
            epoch: 1,
            record: InputRecord {
                seq: 1,
                event: InputEvent::Key(key.clone()),
            },
        },
    )
    .await?;
    let mut length = [0; 4];
    server.1.read_exact(&mut length).await?;
    ensure!(
        u32::from_be_bytes(length) == 12,
        "single-key control is not compact"
    );
    let mut body = [0; 12];
    server.1.read_exact(&mut body).await?;
    ensure!(
        body == [1, 1, 1, 0, 0, 1, b'x', 0, 0, 0, 0, 0],
        "single-key wire value changed"
    );

    let rich_key = Key {
        code: KeyCode::Char('中'),
        modifiers: 255,
        shifted: Some('文'),
        base: Some('a'),
        text: Some("e\u{301}中文🙂\0".into()),
        ..key
    };
    let mut events: Vec<_> = [KeyPhase::Press, KeyPhase::Repeat, KeyPhase::Release]
        .into_iter()
        .map(|phase| {
            InputEvent::Key(Key {
                phase,
                ..rich_key.clone()
            })
        })
        .collect();
    events.extend([
        InputEvent::Key(Key {
            code: KeyCode::Named("Up".into()),
            ..rich_key
        }),
        InputEvent::Paste("paste 中文🙂\0".into()),
        InputEvent::Focus(true),
        InputEvent::Focus(false),
        InputEvent::QueryResult {
            query: u64::MAX,
            data: Some("clipboard 中文\0".into()),
        },
        InputEvent::QueryResult {
            query: 1,
            data: None,
        },
    ]);
    for kind in [
        MouseKind::Press(2),
        MouseKind::Release(2),
        MouseKind::Move,
        MouseKind::Wheel(-3),
        MouseKind::WheelHorizontal(4),
    ] {
        events.push(InputEvent::Mouse(Mouse {
            x: 511,
            y: 255,
            pixel_x: u16::MAX,
            pixel_y: u16::MAX,
            modifiers: 255,
            kind,
        }));
    }
    let mut messages: Vec<_> = events
        .into_iter()
        .enumerate()
        .map(|(index, event)| ClientMessage::Input {
            epoch: u64::MAX,
            record: InputRecord {
                seq: index as u64 + 128,
                event,
            },
        })
        .collect();
    messages.extend([
        ClientMessage::Attach(Attach {
            version: PROTOCOL_VERSION,
            session: session.into(),
            attempt: 2,
            committed: 10,
            rendered: 20,
            effects: 3,
        }),
        ClientMessage::Rendered {
            epoch: 1,
            revision: u64::MAX,
        },
        ClientMessage::Resync { epoch: 1 },
        ClientMessage::MissingResources {
            epoch: 1,
            digests: vec![[0xff; 32]; 128],
        },
        ClientMessage::EffectsApplied {
            epoch: 1,
            through: u64::MAX,
        },
        ClientMessage::Resize {
            epoch: 1,
            generation: 2,
            after: 128,
            size: Size::default(),
        },
        ClientMessage::Detach { epoch: 1 },
        ClientMessage::Close { epoch: 1 },
        ClientMessage::ExitAck { epoch: 1 },
    ]);
    for expected in messages {
        send_message(&mut client.0, &expected).await?;
        let actual: ClientMessage = recv_message(&mut server.1)
            .await?
            .context("missing client control")?;
        ensure!(
            serde_json::to_value(&actual)? == serde_json::to_value(&expected)?,
            "client control changed in transit"
        );
    }

    let bell = Effect {
        id: 1,
        after: 2,
        kind: EffectKind::Bell,
    };
    let mut effects = vec![bell.clone(); MAX_EFFECTS];
    effects[1].kind = EffectKind::ClipboardWrite {
        selection: "c".into(),
        data: "remote 中文🙂\0".into(),
    };
    effects[2].kind = EffectKind::ClipboardRead {
        query: u64::MAX,
        selection: "p".into(),
    };
    effects[3].kind = EffectKind::Notification {
        title: "title 中".into(),
        body: "body 🙂\0".into(),
    };
    for expected in [
        ServerMessage::Attached {
            epoch: 1,
            committed: 128,
            effect_floor: 2,
        },
        ServerMessage::Committed {
            epoch: 1,
            through: u64::MAX,
        },
        ServerMessage::Effects {
            epoch: 1,
            floor: 2,
            items: effects,
        },
        ServerMessage::Exit {
            epoch: 1,
            code: -9,
            revision: u64::MAX,
            effects: 3,
        },
        ServerMessage::Error {
            message: "error 中文🙂\0".into(),
        },
    ] {
        send_message(&mut server.0, &expected).await?;
        let actual: ServerMessage = recv_message(&mut client.1)
            .await?
            .context("missing server control")?;
        ensure!(
            serde_json::to_value(&actual)? == serde_json::to_value(&expected)?,
            "server control changed in transit"
        );
    }

    let oversized_digests = ClientMessage::MissingResources {
        epoch: 1,
        digests: vec![[0xff; 32]; 129],
    };
    rejected_control::<ClientMessage>(
        &mut client.0,
        &mut server.1,
        &postcard::to_allocvec(&oversized_digests)?,
    )
    .await?;
    ensure!(
        serde_json::from_slice::<ClientMessage>(&serde_json::to_vec(&oversized_digests)?).is_err(),
        "resource count without a size hint was accepted"
    );
    let oversized_effects = ServerMessage::Effects {
        epoch: 1,
        floor: 0,
        items: vec![bell.clone(); MAX_EFFECTS + 1],
    };
    rejected_control::<ServerMessage>(
        &mut server.0,
        &mut client.1,
        &postcard::to_allocvec(&oversized_effects)?,
    )
    .await?;
    ensure!(
        serde_json::from_slice::<ServerMessage>(&serde_json::to_vec(&oversized_effects)?).is_err(),
        "effect count without a size hint was accepted"
    );

    let mut missing = postcard::to_allocvec(&ClientMessage::MissingResources {
        epoch: 1,
        digests: vec![],
    })?;
    ensure!(missing.pop() == Some(0), "empty resource count changed");
    missing.extend(postcard::to_allocvec(&usize::MAX)?);
    missing.extend(vec![0; 32 * 129]);
    rejected_control::<ClientMessage>(&mut client.0, &mut server.1, &missing).await?;
    let mut effects = postcard::to_allocvec(&ServerMessage::Effects {
        epoch: 1,
        floor: 0,
        items: vec![],
    })?;
    ensure!(effects.pop() == Some(0), "empty effect count changed");
    effects.extend(postcard::to_allocvec(&usize::MAX)?);
    for _ in 0..=MAX_EFFECTS {
        effects.extend(postcard::to_allocvec(&bell)?);
    }
    rejected_control::<ServerMessage>(&mut server.0, &mut client.1, &effects).await?;

    let valid = postcard::to_allocvec(&ClientMessage::Close { epoch: u64::MAX })?;
    let mut trailing = valid.clone();
    trailing.push(0);
    rejected_control::<ClientMessage>(&mut client.0, &mut server.1, &trailing).await?;
    rejected_control::<ClientMessage>(&mut client.0, &mut server.1, &valid[..valid.len() - 1])
        .await?;
    rejected_control::<ClientMessage>(&mut client.0, &mut server.1, &[0xff; 11]).await?;
    println!(
        "compact control preserves every message and rich input; bounded collections and malformed payloads are rejected"
    );
    Ok(())
}

fn request(identity: &ClientIdentity) -> BootstrapRequest {
    BootstrapRequest {
        version: PROTOCOL_VERSION,
        client_cert: identity.certificate(),
        size: Size {
            cols: 20,
            rows: 4,
            pixel_width: 200,
            pixel_height: 80,
        },
        command: vec!["/bin/sh".into()],
        bind: Some("127.0.0.1".into()),
        port: None,
        idle_seconds: 60,
        clipboard_read: false,
    }
}

fn frame() -> Frame {
    Frame {
        epoch: 1,
        revision: 1,
        base: None,
        echo_through: 0,
        size: Size {
            cols: 20,
            rows: 4,
            pixel_width: 200,
            pixel_height: 80,
        },
        rows: (0..4)
            .map(|y| RowPatch {
                y,
                row: Row::default(),
            })
            .collect(),
        cursor: ScreenCursor {
            visible: true,
            ..Default::default()
        },
        title: "transport fixture".into(),
        directory: None,
        markers: vec![],
        reverse_video: false,
        prediction: PredictionContext::default(),
        resources: vec![],
    }
}

async fn pair(
    server: &Endpoint,
    client: &ClientEndpoints,
    grant: &mosh_rs::model::Grant,
) -> Result<(Connection, Connection)> {
    let (server, client) = tokio::join!(
        async {
            server
                .accept()
                .await
                .context("server endpoint closed")?
                .await
                .map_err(anyhow::Error::from)
        },
        connect(client, grant)
    );
    Ok((server?, client?))
}

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("bootstrap") => return spawn_worker().await,
        Some("worker") => return worker().await,
        Some("bootstrap-fixture") => {
            return bootstrap_fixture(&std::env::args().nth(2).context("missing fixture mode")?);
        }
        Some("bootstrap-probe") => {
            return bootstrap_probe(&std::env::args().nth(2).context("missing probe mode")?).await;
        }
        _ => (),
    }
    tokio::time::timeout(Duration::from_secs(40), verify()).await??;
    println!(
        "transport verification passed: mutual TLS, protocol v3, bounded bootstrap envelopes, compact bounded control, framing, cancellation, control capacity, image hashing, bounded decompression, detached bootstrap"
    );
    Ok(())
}

const GRANT_CANARY: &str = "grant-private-canary.invalid";
const LITERAL_ARGV: &str = "literal ' quotes \" $HOME $(printf unsafe) ; &\nsecond line";

fn bootstrap_fixture(mode: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    if mode == "concurrent" {
        stdout.write_all(&vec![b'b'; 48 * 1024])?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    let request = read_request(std::io::stdin().lock())?;
    ensure!(
        request.command.first().map(String::as_str) == Some(LITERAL_ARGV)
            && request.command.get(1).is_some_and(|arg| {
                arg.len() == if mode == "concurrent" { 120 * 1024 } else { 16 }
                    && arg.bytes().all(|byte| byte == b'x')
            }),
        "bootstrap changed structured argv"
    );
    let grant = Grant {
        version: PROTOCOL_VERSION,
        session: "a1".repeat(32),
        server_cert: vec![1, 2, 3],
        port: 12345,
        address: GRANT_CANARY.into(),
    };
    let mut framed = Vec::new();
    write_grant(&mut framed, &grant)?;
    match mode {
        "banner" | "fragmented" | "unterminated-banner" => {
            eprintln!("fixture stderr diagnostic");
            stdout.write_all(b"{\"diagnostic\":\"ordinary JSON\"}\nleading bootstrap banner")?;
            if mode != "unterminated-banner" {
                stdout.write_all(b"\n")?;
            }
            if mode == "fragmented" {
                for byte in &framed {
                    stdout.write_all(&[*byte])?;
                    stdout.flush()?;
                    std::thread::sleep(Duration::from_millis(1));
                }
            } else {
                stdout.write_all(&framed)?;
            }
            stdout.write_all(&vec![b't'; 40 * 1024])?;
            stdout.write_all(b"\ntrailing bootstrap banner\n")?;
        }
        "concurrent" => stdout.write_all(&framed)?,
        "diagnostic-limit" => {
            stdout.write_all(&vec![b'b'; 64 * 1024 - 1])?;
            stdout.write_all(b"\n")?;
            stdout.write_all(&framed)?;
        }
        "eof" => (),
        "missing" => stdout.write_all(b"banner without a grant\n")?,
        "oversize-banner" => stdout.write_all(&vec![b'b'; 64 * 1024 + 64])?,
        "trailing-oversize" => {
            stdout.write_all(&framed)?;
            stdout.write_all(&vec![b'b'; 64 * 1024 + 64])?;
        }
        "duplicate" => {
            stdout.write_all(&framed)?;
            stdout.write_all(&framed)?;
        }
        "unframed" => {
            serde_json::to_writer(&mut stdout, &grant)?;
            stdout.write_all(b"\n")?;
        }
        "malformed-unframed" => {
            writeln!(stdout, "{{\"session\":\"{GRANT_CANARY}\",broken}}")?;
        }
        "invalid-json" => {
            writeln!(
                stdout,
                "\x1eMOSH-RS-GRANT\n{{\"address\":\"{GRANT_CANARY}\",broken}}\n\x1eMOSH-RS-GRANT-END"
            )?;
        }
        "invalid-grant" => {
            let mut old = grant;
            old.version = 2;
            stdout.write_all(b"\x1eMOSH-RS-GRANT\n")?;
            serde_json::to_writer(&mut stdout, &old)?;
            stdout.write_all(b"\n\x1eMOSH-RS-GRANT-END\n")?;
        }
        "truncated-body" => {
            stdout.write_all(b"\x1eMOSH-RS-GRANT\n{\"version\":")?;
        }
        "missing-end" => {
            stdout.write_all(b"\x1eMOSH-RS-GRANT\n")?;
            serde_json::to_writer(&mut stdout, &grant)?;
            stdout.write_all(b"\n")?;
        }
        "wrong-end" => {
            stdout.write_all(b"\x1eMOSH-RS-GRANT\n")?;
            serde_json::to_writer(&mut stdout, &grant)?;
            stdout.write_all(b"\nunexpected\n")?;
        }
        "unexpected-end" => stdout.write_all(b"\x1eMOSH-RS-GRANT-END\n")?,
        "oversize-body" => {
            stdout.write_all(b"\x1eMOSH-RS-GRANT\n")?;
            stdout.write_all(&vec![b'x'; MAX_CONTROL + 1])?;
        }
        "nonzero" => {
            stdout.write_all(&framed)?;
            stdout.flush()?;
            std::process::exit(23);
        }
        "timeout" => std::thread::sleep(Duration::from_secs(130)),
        _ => anyhow::bail!("unknown bootstrap fixture mode"),
    }
    stdout.flush()?;
    Ok(())
}

async fn bootstrap_probe(mode: &str) -> Result<()> {
    let expected_error = match mode {
        "banner" | "fragmented" | "unterminated-banner" | "concurrent" | "diagnostic-limit" => None,
        "eof" | "missing" => Some("without a grant frame"),
        "oversize-banner" | "trailing-oversize" | "oversize-body" => {
            Some("output line exceeds limit")
        }
        "duplicate" => Some("duplicate bootstrap grant frame"),
        "unframed" | "malformed-unframed" => Some("unframed bootstrap grant"),
        "invalid-json" => Some("invalid bootstrap grant JSON"),
        "invalid-grant" => Some("unsupported server protocol version"),
        "truncated-body" => Some("grant body is incomplete"),
        "missing-end" => Some("grant frame is incomplete"),
        "wrong-end" => Some("invalid bootstrap grant frame end"),
        "unexpected-end" => Some("unexpected bootstrap grant frame end"),
        "nonzero" => Some("session bootstrap exited with"),
        "timeout" => Some("session bootstrap timed out"),
        _ => anyhow::bail!("unknown bootstrap probe mode"),
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let wrapper = std::env::temp_dir().join(format!(
        "mosh-rs-bootstrap-probe-{}-{stamp}",
        std::process::id()
    ));
    let executable = std::env::current_exe()?;
    let quoted_executable = executable.to_string_lossy().replace('\'', "'\\''");
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o700)
        .open(&wrapper)?;
    writeln!(
        file,
        "#!/bin/sh\nexec '{quoted_executable}' bootstrap-fixture '{mode}' \"$@\""
    )?;
    drop(file);
    let identity = ClientIdentity::generate()?;
    let mut request = request(&identity);
    request.command = vec![
        LITERAL_ARGV.into(),
        "x".repeat(if mode == "concurrent" { 120 * 1024 } else { 16 }),
    ];
    let result = bootstrap_local(&identity, &wrapper, request).await;
    std::fs::remove_file(&wrapper)?;
    if let Some(expected) = expected_error {
        let Err(error) = result else {
            anyhow::bail!("invalid bootstrap fixture was accepted");
        };
        ensure!(
            format!("{error:#}").contains(expected),
            "unexpected bootstrap failure: {error:#}"
        );
        eprintln!("bootstrap {mode} rejected: {error:#}");
    } else {
        let grant = result?;
        ensure!(
            grant.version == PROTOCOL_VERSION && grant.address == GRANT_CANARY,
            "bootstrap fixture grant changed"
        );
    }
    Ok(())
}

async fn verify_bootstrap_framing() -> Result<()> {
    for mode in [
        "banner",
        "fragmented",
        "unterminated-banner",
        "concurrent",
        "diagnostic-limit",
        "eof",
        "missing",
        "oversize-banner",
        "trailing-oversize",
        "duplicate",
        "unframed",
        "malformed-unframed",
        "invalid-json",
        "invalid-grant",
        "truncated-body",
        "missing-end",
        "wrong-end",
        "unexpected-end",
        "oversize-body",
        "nonzero",
    ] {
        let output = tokio::process::Command::new(std::env::current_exe()?)
            .args(["bootstrap-probe", mode])
            .output()
            .await?;
        ensure!(
            !output
                .stdout
                .windows(GRANT_CANARY.len())
                .any(|part| part == GRANT_CANARY.as_bytes())
                && !output
                    .stderr
                    .windows(GRANT_CANARY.len())
                    .any(|part| part == GRANT_CANARY.as_bytes())
                && !output
                    .stderr
                    .windows(32)
                    .any(|part| part == b"a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1"),
            "bootstrap diagnostics leaked a grant"
        );
        ensure!(
            output.status.success(),
            "bootstrap {mode} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if matches!(mode, "banner" | "fragmented" | "unterminated-banner") {
            for expected in [
                b"leading bootstrap banner".as_slice(),
                b"ordinary JSON".as_slice(),
                b"trailing bootstrap banner".as_slice(),
                b"fixture stderr diagnostic".as_slice(),
            ] {
                ensure!(
                    output
                        .stderr
                        .windows(expected.len())
                        .any(|part| part == expected),
                    "bootstrap lost a diagnostic"
                );
            }
        }
    }
    println!(
        "bootstrap grant envelopes preserve banners and stderr, handle fragmented output and concurrent stdin, reject malformed/oversized/duplicate/old grants, and hide credentials"
    );
    Ok(())
}

async fn worker() -> Result<()> {
    let request = read_request(std::io::stdin().lock())?;
    let (endpoint, grant) = listen(&request)?;
    write_grant(std::io::stdout().lock(), &grant)?;
    let connection = tokio::time::timeout(Duration::from_secs(15), async {
        endpoint
            .accept()
            .await
            .context("worker endpoint closed")?
            .await
            .map_err(anyhow::Error::from)
    })
    .await??;
    let (mut send, mut recv) = connection.accept_bi().await?;
    let input: String = recv_message(&mut recv)
        .await?
        .context("missing worker request")?;
    send_message(&mut send, &input).await?;
    send.finish()?;
    send.stopped().await?;
    endpoint.close(0_u32.into(), b"fixture complete");
    endpoint.wait_idle().await;
    Ok(())
}

async fn verify() -> Result<()> {
    verify_bootstrap_framing().await?;
    let identity = ClientIdentity::generate()?;
    let request = request(&identity);
    let mut legacy_request = request.clone();
    legacy_request.version = 2;
    let mut legacy_json = serde_json::to_vec(&legacy_request)?;
    legacy_json.push(b'\n');
    ensure!(
        read_request(Cursor::new(legacy_json)).is_err(),
        "v2 bootstrap request was accepted"
    );
    ensure!(
        listen(&legacy_request).is_err(),
        "v2 listener request was accepted"
    );
    let (server, grant) = listen(&request)?;
    let client = identity.endpoint(&grant)?;
    let mut legacy_grant = grant.clone();
    legacy_grant.version = 2;
    ensure!(
        identity.endpoint(&legacy_grant).is_err(),
        "v2 server grant was accepted"
    );
    ensure!(
        connect(&client, &legacy_grant).await.is_err(),
        "v2 connection grant was accepted"
    );
    let mut hostname_grant = grant.clone();
    hostname_grant.address = "localhost".into();
    let (server_connection, client_connection) = tokio::time::timeout(
        Duration::from_secs(2),
        pair(&server, &client, &hostname_grant),
    )
    .await
    .context("hostname fallback waited for an unreachable address")??;
    ensure!(
        client_connection.remote_address().is_ipv4(),
        "IPv4 candidate did not use a native IPv4 socket"
    );
    let handshake = client_connection
        .handshake_data()
        .context("missing TLS handshake data")?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .map_err(|_| anyhow::anyhow!("unexpected TLS handshake data"))?;
    ensure!(
        handshake.protocol.as_deref() == Some(&b"mosh-rs/3"[..]),
        "QUIC negotiated an unexpected protocol"
    );
    println!("hostname candidates reach an IPv4-only server without waiting for IPv6 timeout");
    let mut mapped_grant = grant.clone();
    mapped_grant.address = "::ffff:127.0.0.1".into();
    let (mapped_server, mapped_client) = pair(&server, &client, &mapped_grant).await?;
    ensure!(
        mapped_client.remote_address().is_ipv4(),
        "IPv4-mapped candidate did not use a native IPv4 socket"
    );
    mapped_client.close(0_u32.into(), b"mapped IPv4 fixture complete");
    mapped_server.closed().await;
    println!("IPv4-mapped candidates use the native IPv4 socket");
    let mut ipv6_request = request.clone();
    ipv6_request.bind = Some("::1".into());
    let (ipv6_server, mut ipv6_grant) = listen(&ipv6_request)?;
    ipv6_grant.address = "localhost".into();
    let ipv6_client = identity.endpoint(&ipv6_grant)?;
    let (ipv6_server_connection, ipv6_client_connection) = tokio::time::timeout(
        Duration::from_secs(2),
        pair(&ipv6_server, &ipv6_client, &ipv6_grant),
    )
    .await
    .context("hostname fallback did not reach an IPv6-only server")??;
    ensure!(
        ipv6_client_connection.remote_address().ip().is_ipv6(),
        "IPv6 candidate did not use a native IPv6 socket"
    );
    ipv6_client_connection.close(0_u32.into(), b"IPv6 fixture complete");
    ipv6_server_connection.closed().await;
    ipv6_server.close(0_u32.into(), b"IPv6 fixture complete");
    ipv6_client.close(0_u32.into(), b"IPv6 fixture complete");
    ipv6_client.wait_idle().await;
    println!("hostname candidates also reach an IPv6-only server through a native IPv6 socket");
    let mut client_streams = client_connection.open_bi().await?;
    let attach = ClientMessage::Attach(Attach {
        version: PROTOCOL_VERSION,
        session: grant.session.clone(),
        attempt: 1,
        committed: 0,
        rendered: 0,
        effects: 0,
    });
    send_message(&mut client_streams.0, &attach).await?;
    let mut server_streams = server_connection.accept_bi().await?;
    let received: ClientMessage = recv_message(&mut server_streams.1)
        .await?
        .context("missing attach")?;
    ensure!(
        matches!(received, ClientMessage::Attach(Attach { attempt: 1, .. })),
        "control frame changed"
    );
    send_message(
        &mut server_streams.0,
        &ServerMessage::Attached {
            epoch: 1,
            committed: 0,
            effect_floor: 0,
        },
    )
    .await?;
    let received: ServerMessage = recv_message(&mut client_streams.1)
        .await?
        .context("missing attached")?;
    ensure!(
        matches!(received, ServerMessage::Attached { epoch: 1, .. }),
        "server control frame changed"
    );
    verify_control_messages(&mut client_streams, &mut server_streams, &grant.session).await?;

    let rgba: Arc<[u8]> = vec![123; 4 * 1024 * 1024].into();
    let large = Resource {
        meta: ResourceMeta {
            digest: resource_digest(1024, 1024, &rgba),
            width: 1024,
            height: 1024,
            bytes: rgba.len() as u32,
        },
        rgba,
    };
    let sender = tokio::spawn({
        let connection = server_connection.clone();
        async move { send_resource(&connection, &large).await }
    });
    let held = client_connection.accept_uni().await?;
    tokio::time::timeout(
        Duration::from_secs(2),
        send_message(
            &mut server_streams.0,
            &ServerMessage::Committed {
                epoch: 1,
                through: 7,
            },
        ),
    )
    .await??;
    let committed: ServerMessage =
        tokio::time::timeout(Duration::from_secs(2), recv_message(&mut client_streams.1))
            .await??
            .context("missing commit under bulk backpressure")?;
    ensure!(
        matches!(committed, ServerMessage::Committed { through: 7, .. }),
        "bulk transfer blocked input confirmation"
    );
    drop(held);
    ensure!(
        sender.await?.is_err(),
        "stopped bulk stream unexpectedly succeeded"
    );

    let rgba: Arc<[u8]> = vec![255, 0, 0, 255, 0, 255, 0, 255].into();
    let resource = Resource {
        meta: ResourceMeta {
            digest: resource_digest(2, 1, &rgba),
            width: 2,
            height: 1,
            bytes: 8,
        },
        rgba,
    };
    let resource_sender = tokio::spawn({
        let connection = server_connection.clone();
        let resource = resource.clone();
        async move { send_resource(&connection, &resource).await }
    });
    let incoming = receive_uni(&client_connection).await?;
    let Incoming::Resource(received) = incoming else {
        anyhow::bail!("expected resource stream");
    };
    ensure!(
        received.meta == resource.meta && received.rgba == resource.rgba,
        "resource changed in transit"
    );
    resource_sender.await??;

    let mut cancelled = server_connection.open_uni().await?;
    cancelled.write_all(&[1]).await?;
    cancelled.write_all(b"incomplete frame").await?;
    cancelled.reset(VarInt::from_u32(0x10))?;
    let frame_sender = tokio::spawn({
        let connection = server_connection.clone();
        async move { EncodedFrame::new(&frame())?.send(&connection).await }
    });
    let incoming = receive_uni(&client_connection).await?;
    ensure!(
        matches!(incoming, Incoming::Frame(Frame { revision: 1, .. })),
        "superseded stream prevented next frame"
    );
    frame_sender.await??;

    let rgba: Arc<[u8]> = vec![42; 4 * 1024 * 1024].into();
    let resource = Resource {
        meta: ResourceMeta {
            digest: resource_digest(1024, 1024, &rgba),
            width: 1024,
            height: 1024,
            bytes: rgba.len() as u32,
        },
        rgba,
    };
    let cancelled_sender = tokio::spawn({
        let connection = server_connection.clone();
        async move { send_resource(&connection, &resource).await }
    });
    let mut held = client_connection.accept_uni().await?;
    cancelled_sender.abort();
    ensure!(
        cancelled_sender.await.unwrap_err().is_cancelled(),
        "bulk sender was not cancelled"
    );
    let reset = held.read_to_end(MAX_FRAME).await.unwrap_err();
    ensure!(
        matches!(reset, quinn::ReadToEndError::Read(quinn::ReadError::Reset(code)) if code == VarInt::from_u32(0x10)),
        "cancelled sender did not reset its stream"
    );

    let bomb = zstd::bulk::compress(&vec![b'x'; MAX_FRAME + 1], 1)?;
    let mut stream = server_connection.open_uni().await?;
    stream.write_all(&[1]).await?;
    stream.write_all(&bomb).await?;
    stream.finish()?;
    let error = match receive_uni(&client_connection).await {
        Ok(_) => anyhow::bail!("oversized decompression was accepted"),
        Err(error) => error,
    };
    ensure!(
        error.to_string().contains("exceeds limit"),
        "unexpected decompression failure: {error}"
    );

    ensure!(
        read_request(Cursor::new(vec![b'x'; MAX_CONTROL + 1])).is_err(),
        "oversized bootstrap accepted"
    );
    ensure!(
        send_message(&mut client_streams.0, &"x".repeat(MAX_CONTROL + 1))
            .await
            .is_err(),
        "oversized outgoing control accepted"
    );
    client_streams.0.write_all(&u32::MAX.to_be_bytes()).await?;
    ensure!(
        recv_message::<ClientMessage>(&mut server_streams.1)
            .await
            .is_err(),
        "oversized incoming control accepted"
    );
    client_streams.0.write_all(&4_u32.to_be_bytes()).await?;
    client_streams.0.write_all(&[1]).await?;
    client_streams.0.finish()?;
    ensure!(
        recv_message::<ClientMessage>(&mut server_streams.1)
            .await
            .is_err(),
        "truncated control body was accepted"
    );
    let mut invalid = frame();
    invalid.rows[0].y = 65535;
    ensure!(
        validate_frame(&invalid).is_err(),
        "invalid display row accepted"
    );

    client_connection.close(0_u32.into(), b"fixture complete");
    server_connection.closed().await;

    let wrong_identity = ClientIdentity::generate()?;
    let wrong_client = wrong_identity.endpoint(&grant)?;
    let (server_result, client_result) = tokio::join!(
        async {
            server
                .accept()
                .await
                .context("server closed")?
                .await
                .map_err(anyhow::Error::from)
        },
        connect(&wrong_client, &grant)
    );
    ensure!(
        server_result.is_err(),
        "server accepted another client certificate"
    );
    if let Ok(connection) = client_result {
        tokio::time::timeout(Duration::from_secs(2), connection.closed()).await?;
    }

    let (_other_server, other_grant) = listen(&request)?;
    let mut wrong_grant = grant.clone();
    wrong_grant.server_cert = other_grant.server_cert;
    let wrong_trust = identity.endpoint(&wrong_grant)?;
    let (server_result, client_result) = tokio::join!(
        async {
            server
                .accept()
                .await
                .context("server closed")?
                .await
                .map_err(anyhow::Error::from)
        },
        connect(&wrong_trust, &wrong_grant)
    );
    ensure!(
        client_result.is_err() && server_result.is_err(),
        "client accepted an untrusted server certificate"
    );

    let grant = bootstrap_local(&identity, &std::env::current_exe()?, request.clone()).await?;
    let detached_client = identity.endpoint(&grant)?;
    let connection = connect(&detached_client, &grant).await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    send_message(&mut send, &"detached worker survived bootstrap exit").await?;
    let reply: String = recv_message(&mut recv)
        .await?
        .context("detached worker did not reply")?;
    ensure!(
        reply == "detached worker survived bootstrap exit",
        "detached worker changed message"
    );
    connection.closed().await;
    server.close(0_u32.into(), b"fixture complete");
    client.close(0_u32.into(), b"fixture complete");
    wrong_client.close(0_u32.into(), b"fixture complete");
    wrong_trust.close(0_u32.into(), b"fixture complete");
    Ok(())
}
