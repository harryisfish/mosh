use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use mosh_rs::model::{
    Attach, BootstrapRequest, ClientMessage, Cursor as ScreenCursor, Frame, MAX_CONTROL, MAX_FRAME,
    PROTOCOL_VERSION, PredictionContext, Resource, ResourceMeta, Row, RowPatch, ServerMessage,
    Size, resource_digest,
};
use mosh_rs::transport::{
    ClientIdentity, Incoming, bootstrap_local, connect, listen, read_request, receive_uni,
    recv_message, send_frame, send_message, send_resource, spawn_worker, validate_frame,
    write_grant,
};
use quinn::{Connection, Endpoint, VarInt};

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
        idle_seconds: 30,
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
    client: &Endpoint,
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
        _ => (),
    }
    tokio::time::timeout(Duration::from_secs(40), verify()).await??;
    println!(
        "transport verification passed: mutual TLS, certificate rejection, framing, cancellation, control capacity, image hashing, bounded decompression, detached bootstrap"
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
    let identity = ClientIdentity::generate()?;
    let request = request(&identity);
    let (server, grant) = listen(&request)?;
    let client = identity.endpoint(&grant)?;
    let (server_connection, client_connection) = pair(&server, &client, &grant).await?;
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
        async move { send_frame(&connection, &frame()).await }
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
