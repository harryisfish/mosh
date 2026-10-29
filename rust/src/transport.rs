use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig, VarInt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::diagnostics::{self, Context as DiagnosticContext, Stage};
use crate::model::{
    BootstrapRequest, Frame, Grant, MAX_CELL_IMAGES, MAX_CONTROL, MAX_FRAME, MAX_IMAGE_TILES,
    MAX_RESOURCE, MAX_RESOURCES, PROTOCOL_VERSION, Resource, ResourceMeta,
};

const ALPN: &[u8] = b"mosh-rs/2";
const FRAME_STREAM: u8 = 1;
const RESOURCE_STREAM: u8 = 2;
const STREAM_WINDOW: u32 = 1024 * 1024;
const CONNECTION_WINDOW: u32 = 4 * STREAM_WINDOW;
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ClientIdentity {
    certificate: Vec<u8>,
    key: Vec<u8>,
}

#[derive(Clone)]
pub struct ClientEndpoints {
    ipv4: Option<Endpoint>,
    ipv6: Option<Endpoint>,
}

impl ClientEndpoints {
    pub fn close(&self, error_code: VarInt, reason: &[u8]) {
        for endpoint in self.ipv4.iter().chain(self.ipv6.iter()) {
            endpoint.close(error_code, reason);
        }
    }

    pub async fn wait_idle(&self) {
        tokio::join!(
            async {
                if let Some(endpoint) = &self.ipv4 {
                    endpoint.wait_idle().await;
                }
            },
            async {
                if let Some(endpoint) = &self.ipv6 {
                    endpoint.wait_idle().await;
                }
            }
        );
    }
}

impl ClientIdentity {
    pub fn generate() -> Result<Self> {
        let certified = rcgen::generate_simple_self_signed(vec!["mosh-client.local".into()])?;
        Ok(Self {
            certificate: certified.cert.der().to_vec(),
            key: certified.signing_key.serialize_der(),
        })
    }

    pub fn certificate(&self) -> Vec<u8> {
        self.certificate.clone()
    }

    pub fn endpoint(&self, grant: &Grant) -> Result<ClientEndpoints> {
        validate_grant(grant)?;
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(grant.server_cert.clone()))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_client_auth_cert(
                vec![CertificateDer::from(self.certificate.clone())],
                private_key(&self.key),
            )?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        tls.enable_early_data = false;
        let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
        config.transport_config(Arc::new(transport_config()));
        let ipv4 = Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into());
        let ipv6 = Endpoint::client((Ipv6Addr::UNSPECIFIED, 0).into());
        if let (Err(ipv4), Err(ipv6)) = (&ipv4, &ipv6) {
            bail!("cannot bind client UDP sockets: IPv4: {ipv4}; IPv6: {ipv6}");
        }
        let mut endpoints = ClientEndpoints {
            ipv4: ipv4.ok(),
            ipv6: ipv6.ok(),
        };
        for endpoint in endpoints.ipv4.iter_mut().chain(endpoints.ipv6.iter_mut()) {
            endpoint.set_default_client_config(config.clone());
        }
        Ok(endpoints)
    }
}

fn private_key(bytes: &[u8]) -> PrivateKeyDer<'static> {
    PrivatePkcs8KeyDer::from(bytes.to_vec()).into()
}

fn transport_config() -> TransportConfig {
    let mut config = TransportConfig::default();
    config
        .max_concurrent_bidi_streams(1_u32.into())
        .max_concurrent_uni_streams(3_u32.into())
        .stream_receive_window(STREAM_WINDOW.into())
        .receive_window(CONNECTION_WINDOW.into())
        .send_window(CONNECTION_WINDOW.into())
        .keep_alive_interval(Some(Duration::from_secs(5)))
        .max_idle_timeout(Some(
            Duration::from_secs(20).try_into().expect("valid timeout"),
        ))
        .datagram_receive_buffer_size(None)
        .datagram_send_buffer_size(0);
    config
}

pub fn listen(request: &BootstrapRequest) -> Result<(Endpoint, Grant)> {
    validate_request(request)?;
    let certified = rcgen::generate_simple_self_signed(vec!["mosh.local".into()])?;
    let cert = certified.cert.der().to_vec();
    let key = certified.signing_key.serialize_der();
    let mut client_roots = rustls::RootCertStore::empty();
    client_roots.add(CertificateDer::from(request.client_cert.clone()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client_verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(client_roots),
        provider.clone(),
    )
    .build()?;
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![CertificateDer::from(cert.clone())], private_key(&key))?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config
        .max_incoming(32)
        .incoming_buffer_size_total(4 * 1024 * 1024);
    config.transport_config(Arc::new(transport_config()));
    config.migration(true);
    let bind = request
        .bind
        .as_deref()
        .map(str::parse::<IpAddr>)
        .transpose()
        .context("--bind must be an IP address")?
        .unwrap_or(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    let address = SocketAddr::new(bind, request.port.unwrap_or(0));
    let endpoint = Endpoint::server(config.clone(), address).or_else(|error| {
        if request.bind.is_none() {
            Endpoint::server(
                config,
                (Ipv4Addr::UNSPECIFIED, request.port.unwrap_or(0)).into(),
            )
        } else {
            Err(error)
        }
    })?;
    let local = endpoint.local_addr()?;
    let address = if !local.ip().is_unspecified() {
        local.ip().to_string()
    } else {
        std::env::var("SSH_CONNECTION")
            .ok()
            .and_then(|value| value.split_whitespace().nth(2).map(str::to_owned))
            .filter(|value| value.parse::<IpAddr>().is_ok())
            .unwrap_or_else(|| {
                if local.is_ipv6() {
                    "::1".into()
                } else {
                    "127.0.0.1".into()
                }
            })
    };
    let session = blake3::derive_key("mosh-rs session identity v1", &key)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((
        endpoint,
        Grant {
            version: PROTOCOL_VERSION,
            session,
            server_cert: cert,
            port: local.port(),
            address,
        },
    ))
}

pub async fn connect(endpoints: &ClientEndpoints, grant: &Grant) -> Result<Connection> {
    validate_grant(grant)?;
    let resolve = diagnostics::operation(Stage::Resolve, DiagnosticContext::default());
    let addresses: Vec<_> = tokio::net::lookup_host((grant.address.as_str(), grant.port))
        .await
        .context("cannot resolve server UDP address")?
        .collect();
    drop(resolve);
    ensure!(
        !addresses.is_empty(),
        "server address resolved to no IP addresses"
    );
    let mut attempts = tokio::task::JoinSet::new();
    let mut last_error = None;
    for address in addresses {
        let address = if let IpAddr::V6(ipv6) = address.ip()
            && let Some(ipv4) = ipv6.to_ipv4_mapped()
        {
            SocketAddr::new(ipv4.into(), address.port())
        } else {
            address
        };
        let endpoint = if address.is_ipv4() {
            &endpoints.ipv4
        } else {
            &endpoints.ipv6
        };
        let Some(endpoint) = endpoint.clone() else {
            last_error = Some(anyhow::anyhow!("no client UDP socket for {address}"));
            continue;
        };
        attempts.spawn(async move {
            let _operation = diagnostics::operation(Stage::Connect, DiagnosticContext::default());
            let connecting = endpoint
                .connect(address, "mosh.local")
                .with_context(|| format!("cannot start UDP connection to {address}"))?;
            tokio::time::timeout(CONNECT_TIMEOUT, connecting)
                .await
                .with_context(|| format!("UDP connection to {address} timed out"))?
                .with_context(|| format!("UDP handshake with {address} failed"))
        });
    }
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok(Ok(connection)) => return Ok(connection),
            Ok(Err(error)) => last_error = Some(error),
            Err(error) => {
                last_error = Some(anyhow::Error::from(error));
            }
        }
    }
    Err(last_error.expect("at least one address"))
        .context("cannot establish authenticated UDP connection")
}

pub async fn bootstrap_ssh(
    identity: &ClientIdentity,
    target: &str,
    server_path: &str,
    mut request: BootstrapRequest,
    ssh_port: Option<u16>,
) -> Result<Grant> {
    ensure!(
        !target.is_empty() && !target.starts_with('-') && !target.chars().any(char::is_control),
        "invalid SSH target"
    );
    ensure!(
        !server_path.is_empty() && !server_path.contains('\0') && !server_path.contains('\n'),
        "invalid server executable path"
    );
    request.client_cert = identity.certificate();
    validate_request(&request)?;
    let mut command = ssh_command(ssh_port);
    command
        .arg("-T")
        .arg("-o")
        .arg("RequestTTY=no")
        .arg("-o")
        .arg("ClearAllForwardings=yes")
        .arg("--")
        .arg(target)
        .arg(format!(
            "{} bootstrap{}",
            shell_quote(server_path),
            if diagnostics::enabled() {
                " --diagnostics"
            } else {
                ""
            }
        ));
    let mut grant = run_bootstrap(command, &request).await?;
    let use_ssh_hostname = request
        .bind
        .as_deref()
        .map(str::parse::<IpAddr>)
        .transpose()?
        .is_none_or(|ip| ip.is_unspecified());
    let mut query = ssh_command(ssh_port);
    query.arg("-G").arg("--").arg(target);
    if use_ssh_hostname
        && let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(10), query.output()).await
        && output.status.success()
        && output.stdout.len() <= MAX_CONTROL
        && let Some(hostname) = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("hostname "))
    {
        ensure!(
            hostname.len() <= 1024 && !hostname.chars().any(char::is_control),
            "invalid SSH hostname"
        );
        grant.address = hostname.trim_matches(['[', ']']).to_owned();
    }
    validate_grant(&grant)?;
    Ok(grant)
}

pub async fn bootstrap_local(
    identity: &ClientIdentity,
    server_path: &Path,
    mut request: BootstrapRequest,
) -> Result<Grant> {
    request.client_cert = identity.certificate();
    validate_request(&request)?;
    let mut command = Command::new(server_path);
    command.arg("bootstrap");
    if diagnostics::enabled() {
        command.arg("--diagnostics");
    }
    run_bootstrap(command, &request).await
}

fn ssh_command(port: Option<u16>) -> Command {
    let mut command = Command::new("ssh");
    if let Some(port) = port {
        command.arg("-p").arg(port.to_string());
    }
    command
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn run_bootstrap(mut command: Command, request: &BootstrapRequest) -> Result<Grant> {
    let _operation = diagnostics::operation(Stage::Bootstrap, DiagnosticContext::default());
    let bytes = json_line(request)?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().context("cannot start session bootstrap")?;
    tokio::time::timeout(BOOTSTRAP_TIMEOUT, async {
        let mut stdin = child.stdin.take().context("bootstrap stdin missing")?;
        stdin.write_all(&bytes).await?;
        stdin.shutdown().await?;
        drop(stdin);
        let stdout = child.stdout.take().context("bootstrap stdout missing")?;
        let grant: Grant = read_async_line(stdout)
            .await
            .context("server did not return a valid bootstrap grant")?;
        let status = child.wait().await?;
        ensure!(status.success(), "session bootstrap exited with {status}");
        validate_grant(&grant)?;
        Ok(grant)
    })
    .await
    .context("SSH bootstrap timed out")?
}

pub fn read_request<R: Read>(reader: R) -> Result<BootstrapRequest> {
    let request: BootstrapRequest = read_line(reader)?;
    validate_request(&request)?;
    Ok(request)
}

pub fn write_grant<W: Write>(mut writer: W, grant: &Grant) -> Result<()> {
    validate_grant(grant)?;
    writer.write_all(&json_line(grant)?)?;
    writer.flush()?;
    Ok(())
}

pub async fn spawn_worker(diagnostics_enabled: bool) -> Result<()> {
    let request = read_request(std::io::stdin().lock())?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = std::env::temp_dir().join(format!("mosh-rs-{}-{stamp}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let log_path = directory.join("server.log");
    let log = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&log_path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log);
    let diagnostic_path = directory.join("diagnostics.jsonl");
    if diagnostics_enabled {
        command.arg("--diagnostics").arg(&diagnostic_path);
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("cannot start detached session worker")?;
    let grant = tokio::time::timeout(Duration::from_secs(30), async {
        let mut stdin = child.stdin.take().context("worker stdin missing")?;
        stdin.write_all(&json_line(&request)?).await?;
        stdin.shutdown().await?;
        drop(stdin);
        read_async_line::<_, Grant>(child.stdout.take().context("worker stdout missing")?).await
    })
    .await;
    match grant {
        Ok(Ok(grant)) => {
            if diagnostics_enabled {
                eprintln!("mosh-rs-server: diagnostics: {}", diagnostic_path.display());
            }
            let delivered = write_grant(std::io::stdout().lock(), &grant);
            if delivered.is_err() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            delivered
        }
        result => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            match result {
                Ok(Err(error)) => Err(error)
                    .with_context(|| format!("worker startup failed; log: {}", log_path.display())),
                Err(error) => Err(error).with_context(|| {
                    format!("worker startup timed out; log: {}", log_path.display())
                }),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

fn json_line<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() < MAX_CONTROL, "bootstrap message exceeds limit");
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_line<R: Read, T: DeserializeOwned>(reader: R) -> Result<T> {
    let mut reader = std::io::BufReader::new(reader).take(MAX_CONTROL as u64 + 1);
    let mut bytes = Vec::new();
    std::io::BufRead::read_until(&mut reader, b'\n', &mut bytes)?;
    ensure!(
        bytes.len() <= MAX_CONTROL && bytes.last() == Some(&b'\n'),
        "bootstrap message is incomplete or exceeds limit"
    );
    bytes.pop();
    Ok(serde_json::from_slice(&bytes)?)
}

async fn read_async_line<R: AsyncRead + Unpin, T: DeserializeOwned>(reader: R) -> Result<T> {
    let mut reader = BufReader::new(reader);
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        ensure!(!available.is_empty(), "bootstrap stream ended before grant");
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.unwrap_or(available.len());
        ensure!(
            bytes.len() + count < MAX_CONTROL,
            "bootstrap message exceeds limit"
        );
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count + usize::from(end.is_some()));
        if end.is_some() {
            return Ok(serde_json::from_slice(&bytes)?);
        }
    }
}

fn validate_request(request: &BootstrapRequest) -> Result<()> {
    ensure!(
        request.version == PROTOCOL_VERSION,
        "unsupported protocol version"
    );
    request.size.validate()?;
    ensure!(
        !request.client_cert.is_empty() && request.client_cert.len() <= 16384,
        "invalid client certificate size"
    );
    ensure!(
        request.command.len() <= 128
            && request.command.iter().map(String::len).sum::<usize>() <= 128 * 1024,
        "command exceeds limit"
    );
    ensure!(
        request.command.iter().all(|part| !part.contains('\0')),
        "command contains NUL"
    );
    ensure!(
        (60..=604800).contains(&request.idle_seconds),
        "idle timeout must be between 60 and 604800 seconds"
    );
    ensure!(request.port != Some(0), "server port must be nonzero");
    Ok(())
}

fn validate_grant(grant: &Grant) -> Result<()> {
    ensure!(
        grant.version == PROTOCOL_VERSION,
        "unsupported server protocol version"
    );
    ensure!(
        grant.session.len() == 64 && grant.session.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid session identity"
    );
    ensure!(
        !grant.server_cert.is_empty() && grant.server_cert.len() <= 16384,
        "invalid server certificate size"
    );
    ensure!(
        grant.port > 0
            && !grant.address.is_empty()
            && grant.address.len() <= 1024
            && !grant.address.chars().any(char::is_control),
        "invalid UDP endpoint"
    );
    Ok(())
}

pub async fn send_message<T: Serialize>(send: &mut SendStream, message: &T) -> Result<()> {
    let bytes = postcard::to_allocvec(message)?;
    ensure!(bytes.len() <= MAX_CONTROL, "control message exceeds limit");
    let _operation = diagnostics::operation(
        Stage::ControlWrite,
        DiagnosticContext {
            bytes: bytes.len() as u64,
            ..Default::default()
        },
    );
    send.set_priority(10)?;
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

pub async fn recv_message<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<Option<T>> {
    let mut length = [0_u8; 4];
    let first = recv.read(&mut length[..1]).await?;
    if first.is_none() {
        return Ok(None);
    }
    let _operation =
        diagnostics::operation(Stage::ControlPayloadRead, DiagnosticContext::default());
    recv.read_exact(&mut length[1..]).await?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(length <= MAX_CONTROL, "control message exceeds limit");
    let mut bytes = vec![0; length];
    recv.read_exact(&mut bytes).await?;
    let (message, remaining) = postcard::take_from_bytes(&bytes)?;
    ensure!(remaining.is_empty(), "trailing bytes in control message");
    Ok(Some(message))
}

pub enum Incoming {
    Frame(Frame),
    Resource(Resource),
}

const SUPERSEDED: VarInt = VarInt::from_u32(0x10);

struct CancellableStream(Option<SendStream>);

impl CancellableStream {
    fn stream(&mut self) -> &mut SendStream {
        self.0.as_mut().expect("active display stream")
    }

    async fn complete(mut self) -> Result<()> {
        self.stream().finish()?;
        let stopped = self.stream().stopped().await?;
        ensure!(stopped.is_none(), "display stream stopped by receiver");
        self.0.take();
        Ok(())
    }
}

impl Drop for CancellableStream {
    fn drop(&mut self) {
        if let Some(stream) = &mut self.0 {
            let _ = stream.reset(SUPERSEDED);
        }
    }
}

pub struct EncodedFrame {
    compressed: Vec<u8>,
    epoch: u64,
    revision: u64,
}

impl EncodedFrame {
    pub fn new(frame: &Frame) -> Result<Self> {
        let _operation = diagnostics::operation(
            Stage::FrameEncode,
            DiagnosticContext {
                epoch: frame.epoch,
                revision: frame.revision,
                ..Default::default()
            },
        );
        validate_frame(frame)?;
        let bytes = postcard::to_allocvec(frame)?;
        ensure!(bytes.len() <= MAX_FRAME, "display frame exceeds limit");
        let compressed = zstd::bulk::compress(&bytes, 1)?;
        ensure!(
            compressed.len() <= MAX_FRAME,
            "compressed frame exceeds limit"
        );
        Ok(Self {
            compressed,
            epoch: frame.epoch,
            revision: frame.revision,
        })
    }

    pub fn compressed_len(&self) -> usize {
        self.compressed.len()
    }

    pub async fn send(self, connection: &Connection) -> Result<()> {
        let _operation = diagnostics::operation(
            Stage::FrameSend,
            DiagnosticContext {
                bytes: self.compressed.len() as u64,
                epoch: self.epoch,
                revision: self.revision,
                ..Default::default()
            },
        );
        let mut stream = CancellableStream(Some(connection.open_uni().await?));
        stream.stream().set_priority(0)?;
        stream.stream().write_all(&[FRAME_STREAM]).await?;
        stream.stream().write_all(&self.compressed).await?;
        stream.complete().await
    }
}

pub async fn send_resource(connection: &Connection, resource: &Resource) -> Result<()> {
    let _operation = diagnostics::operation(
        Stage::ResourceSend,
        DiagnosticContext {
            bytes: resource.rgba.len() as u64,
            ..Default::default()
        },
    );
    resource.validate()?;
    let metadata = serde_json::to_vec(&resource.meta)?;
    ensure!(metadata.len() <= 4096, "image metadata exceeds limit");
    let mut stream = CancellableStream(Some(connection.open_uni().await?));
    stream.stream().set_priority(-10)?;
    stream.stream().write_all(&[RESOURCE_STREAM]).await?;
    stream
        .stream()
        .write_all(&(metadata.len() as u32).to_be_bytes())
        .await?;
    stream.stream().write_all(&metadata).await?;
    stream.stream().write_all(&resource.rgba).await?;
    stream.complete().await
}

pub async fn receive_uni(connection: &Connection) -> Result<Incoming> {
    loop {
        match receive_one_uni(connection).await {
            Err(error) if is_superseded(&error) => continue,
            result => return result,
        }
    }
}

fn is_superseded(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        if let Some(quinn::ReadExactError::ReadError(quinn::ReadError::Reset(code))) =
            cause.downcast_ref::<quinn::ReadExactError>()
        {
            return *code == SUPERSEDED;
        }
        if let Some(quinn::ReadToEndError::Read(quinn::ReadError::Reset(code))) =
            cause.downcast_ref::<quinn::ReadToEndError>()
        {
            return *code == SUPERSEDED;
        }
        false
    })
}

async fn receive_one_uni(connection: &Connection) -> Result<Incoming> {
    let mut stream = connection.accept_uni().await?;
    let payload = diagnostics::operation(Stage::DisplayPayloadRead, DiagnosticContext::default());
    let mut kind = [0_u8; 1];
    stream.read_exact(&mut kind).await?;
    match kind[0] {
        FRAME_STREAM => {
            let bytes = stream.read_to_end(MAX_FRAME).await?;
            drop(payload);
            let _operation = diagnostics::operation(
                Stage::FrameDecode,
                DiagnosticContext {
                    bytes: bytes.len() as u64,
                    ..Default::default()
                },
            );
            let mut decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(bytes))?;
            decoder.window_log_max(23)?;
            let mut decoded = Vec::new();
            Read::take(decoder, MAX_FRAME as u64 + 1).read_to_end(&mut decoded)?;
            ensure!(
                decoded.len() <= MAX_FRAME,
                "decoded display frame exceeds limit"
            );
            let frame = postcard::from_bytes(&decoded)?;
            validate_frame(&frame)?;
            Ok(Incoming::Frame(frame))
        }
        RESOURCE_STREAM => {
            let mut length = [0_u8; 4];
            stream.read_exact(&mut length).await?;
            let length = u32::from_be_bytes(length) as usize;
            ensure!(length <= 4096, "image metadata exceeds limit");
            let mut bytes = vec![0; length];
            stream.read_exact(&mut bytes).await?;
            let meta: ResourceMeta = serde_json::from_slice(&bytes)?;
            validate_resource_meta(&meta)?;
            let rgba = stream.read_to_end(meta.bytes as usize).await?;
            let resource = Resource {
                meta,
                rgba: rgba.into(),
            };
            resource.validate()?;
            Ok(Incoming::Resource(resource))
        }
        _ => bail!("unknown display stream type"),
    }
}

fn validate_resource_meta(meta: &ResourceMeta) -> Result<()> {
    ensure!(
        (1..=16384).contains(&meta.width) && (1..=16384).contains(&meta.height),
        "image dimensions exceed limit"
    );
    let bytes = u64::from(meta.width) * u64::from(meta.height) * 4;
    ensure!(
        meta.width > 0
            && meta.height > 0
            && bytes <= MAX_RESOURCE as u64
            && bytes == u64::from(meta.bytes),
        "image dimensions or length exceed limit"
    );
    Ok(())
}

fn safe_text(text: &str, limit: usize) -> bool {
    text.len() <= limit && !text.chars().any(char::is_control)
}

pub fn validate_frame(frame: &Frame) -> Result<()> {
    frame.size.validate()?;
    ensure!(
        frame.epoch > 0
            && frame.revision > 0
            && frame.base.is_none_or(|base| base < frame.revision),
        "invalid display revision"
    );
    ensure!(
        frame.cursor.x < frame.size.cols && frame.cursor.y < frame.size.rows,
        "cursor exceeds window"
    );
    ensure!(
        frame.rows.len() <= frame.size.rows as usize
            && frame.resources.len() <= 4096
            && frame.markers.len() <= 4096,
        "display metadata exceeds limit"
    );
    ensure!(
        safe_text(&frame.title, 4096)
            && frame
                .directory
                .as_ref()
                .is_none_or(|value| safe_text(value, 16384)),
        "unsafe terminal metadata"
    );
    let mut resource_ids = BTreeSet::new();
    let mut total_resource_bytes = 0_u64;
    for resource in &frame.resources {
        validate_resource_meta(resource)?;
        ensure!(
            resource_ids.insert(resource.digest),
            "duplicate image resource"
        );
        total_resource_bytes += u64::from(resource.bytes);
    }
    ensure!(
        total_resource_bytes <= MAX_RESOURCES as u64,
        "frame image bytes exceed session limit"
    );
    let mut rows = BTreeSet::new();
    let mut image_tiles = 0usize;
    for patch in &frame.rows {
        ensure!(
            patch.y < frame.size.rows && rows.insert(patch.y),
            "invalid or duplicate display row"
        );
        ensure!(
            patch.row.cells.len() <= frame.size.cols as usize,
            "display row exceeds limit"
        );
        let mut end = 0_u16;
        for cell in &patch.row.cells {
            ensure!(
                (1..=2).contains(&cell.width)
                    && cell.x >= end
                    && cell.x < frame.size.cols
                    && cell.x + u16::from(cell.width) <= frame.size.cols,
                "invalid cell position or width"
            );
            ensure!(
                safe_text(&cell.text, 4096) && cell.images.len() <= MAX_CELL_IMAGES,
                "cell content exceeds limit"
            );
            image_tiles += cell.images.len();
            ensure!(image_tiles <= MAX_IMAGE_TILES, "too many image placements");
            if let Some(link) = &cell.style.hyperlink {
                ensure!(
                    safe_text(&link.params, 4096) && safe_text(&link.url, 16384),
                    "unsafe hyperlink"
                );
            }
            for tile in &cell.images {
                ensure!(
                    resource_ids.contains(&tile.resource),
                    "image placement has no resource"
                );
                let meta = frame
                    .resources
                    .iter()
                    .find(|meta| meta.digest == tile.resource)
                    .expect("resource exists");
                ensure!(
                    tile.source[0]
                        .checked_add(tile.source[2])
                        .is_some_and(|right| right <= meta.width)
                        && tile.source[1]
                            .checked_add(tile.source[3])
                            .is_some_and(|bottom| bottom <= meta.height),
                    "image crop exceeds resource"
                );
            }
            end = cell.x + u16::from(cell.width);
        }
    }
    for marker in &frame.markers {
        ensure!(safe_text(&marker.kind, 32), "invalid shell marker");
    }
    if frame.base.is_none() {
        ensure!(
            frame.rows.len() == frame.size.rows as usize,
            "incomplete display checkpoint"
        );
    }
    Ok(())
}
