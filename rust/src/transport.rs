use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, RecvStream, SendStream, TransportConfig, VarInt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::diagnostics::{self, Context as DiagnosticContext, Stage};
use crate::model::{
    BootstrapRequest, Frame, Grant, MAX_CELL_IMAGES, MAX_CONTROL, MAX_FRAME, MAX_IMAGE_TILES,
    MAX_RESOURCE, MAX_RESOURCES, PROTOCOL_VERSION, Resource, ResourceMeta,
    validate_locale_candidates,
};

const ALPN: &[u8] = b"mosh-rs/4";
const FRAME_STREAM: u8 = 1;
const RESOURCE_STREAM: u8 = 2;
const STREAM_WINDOW: u32 = 1024 * 1024;
const CONNECTION_WINDOW: u32 = 4 * STREAM_WINDOW;
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const GRANT_BEGIN: &[u8] = b"\x1eMOSH-RS-GRANT\n";
const GRANT_END: &[u8] = b"\x1eMOSH-RS-GRANT-END\n";
const MAX_BOOTSTRAP_DIAGNOSTICS: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum GrantReadEnd {
    Frame,
    Eof,
}

enum GrantPhase {
    Seeking,
    Body,
    End(Grant),
    Finished(Grant),
}

enum SshDestination {
    Hostname(String),
    Bind(IpAddr),
}

impl SshDestination {
    async fn resolve(bind: Option<&str>, target: &str, ssh_port: Option<u16>) -> Result<Self> {
        let bind = bind
            .map(str::parse::<IpAddr>)
            .transpose()
            .context("--bind must be an IP address")?
            .map(normalize_ip);
        if let Some(bind) = bind.filter(|ip| !ip.is_unspecified()) {
            return Ok(Self::Bind(bind));
        }
        Ok(Self::Hostname(ssh_hostname(target, ssh_port).await?))
    }

    fn authorize(self, mut grant: Grant) -> Result<Grant> {
        match self {
            Self::Hostname(hostname) => grant.address = hostname,
            Self::Bind(bind) => {
                let advertised = grant
                    .address
                    .parse::<IpAddr>()
                    .map(normalize_ip)
                    .context("server UDP address does not match --bind")?;
                ensure!(
                    advertised == bind,
                    "server UDP address does not match --bind"
                );
                grant.address = bind.to_string();
            }
        }
        validate_grant(&grant)?;
        Ok(grant)
    }
}

pub struct ClientIdentity {
    certificate: Vec<u8>,
    key: Vec<u8>,
}

pub struct WorkerTempDirGuard {
    path: Option<PathBuf>,
}

impl WorkerTempDirGuard {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for WorkerTempDirGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
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
    for mut address in addresses {
        address.set_ip(normalize_ip(address.ip()));
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
    let destination = SshDestination::resolve(request.bind.as_deref(), target, ssh_port).await?;
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
    let grant = run_bootstrap(command, &request).await?;
    destination.authorize(grant)
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

fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ipv6) => ipv6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    }
}

async fn ssh_hostname(target: &str, ssh_port: Option<u16>) -> Result<String> {
    let mut child = ssh_command(ssh_port)
        .arg("-G")
        .arg("--")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("cannot query SSH configuration")?;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let mut stdout = child
            .stdout
            .take()
            .context("SSH configuration stdout missing")?
            .take((MAX_CONTROL + 1) as u64);
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() <= MAX_CONTROL,
            "SSH configuration output exceeds limit"
        );
        let status = child.wait().await?;
        ensure!(
            status.success(),
            "SSH configuration query exited with {status}"
        );
        let output = std::str::from_utf8(&bytes).context("SSH configuration is not UTF-8")?;
        let mut hostname = None;
        for line in output.lines() {
            let mut fields = line.splitn(2, char::is_whitespace);
            if fields.next() == Some("hostname") {
                ensure!(
                    hostname.is_none(),
                    "SSH configuration contains duplicate hostname"
                );
                hostname = Some(fields.next().unwrap_or(""));
            }
        }
        let hostname = hostname.context("SSH configuration is missing hostname")?;
        ensure!(
            !hostname.is_empty()
                && hostname.len() <= 1024
                && !hostname
                    .chars()
                    .any(|ch| ch.is_whitespace() || ch.is_control()),
            "invalid SSH hostname"
        );
        let hostname = if hostname.starts_with('[') && hostname.ends_with(']') {
            let inner = &hostname[1..hostname.len() - 1];
            inner.parse::<Ipv6Addr>().context("invalid SSH hostname")?;
            inner
        } else {
            hostname
        };
        ensure!(!hostname.contains(['[', ']']), "invalid SSH hostname");
        Ok(hostname
            .parse::<IpAddr>()
            .map(normalize_ip)
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| hostname.to_owned()))
    })
    .await;
    match result {
        Ok(Ok(hostname)) => Ok(hostname),
        result => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            match result {
                Ok(Err(error)) => Err(error),
                Err(_) => bail!("SSH configuration query timed out"),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
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
    let result = tokio::time::timeout(BOOTSTRAP_TIMEOUT, async {
        let mut stdin = child.stdin.take().context("bootstrap stdin missing")?;
        let stdout = child.stdout.take().context("bootstrap stdout missing")?;
        let ((), grant) = tokio::try_join!(
            async {
                stdin.write_all(&bytes).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            },
            read_grant(stdout, GrantReadEnd::Eof),
        )?;
        let status = child.wait().await?;
        ensure!(status.success(), "session bootstrap exited with {status}");
        Ok(grant)
    })
    .await;
    match result {
        Ok(Ok(grant)) => Ok(grant),
        result => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            match result {
                Ok(Err(error)) => Err(error),
                Err(_) => bail!("session bootstrap timed out"),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}

pub fn read_request<R: Read>(reader: R) -> Result<BootstrapRequest> {
    let request: BootstrapRequest = read_line(reader)?;
    validate_request(&request)?;
    Ok(request)
}

pub fn write_grant<W: Write>(mut writer: W, grant: &Grant) -> Result<()> {
    validate_grant(grant)?;
    let body = json_line(grant)?;
    writer.write_all(GRANT_BEGIN)?;
    writer.write_all(&body)?;
    writer.write_all(GRANT_END)?;
    writer.flush()?;
    Ok(())
}

fn worker_log_diagnostics(log: &mut std::fs::File) -> String {
    const LIMIT: usize = 8 * 1024;
    let mut bytes = Vec::new();
    if std::io::Read::take(log, (LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return "unavailable".into();
    }
    let clipped = String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT)]);
    let diagnostics = clipped.trim_end();
    if diagnostics.is_empty() {
        "empty".into()
    } else if bytes.len() > LIMIT {
        format!("{diagnostics}\n[truncated]")
    } else {
        diagnostics.into()
    }
}

pub async fn spawn_worker(diagnostics_enabled: bool) -> Result<()> {
    let request = read_request(std::io::stdin().lock())?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let directory = std::env::temp_dir().join(format!("mosh-rs-{}-{stamp}", std::process::id()));
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    let mut cleanup = WorkerTempDirGuard::new(directory.clone());
    let log_path = directory.join("server.log");
    let log = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&log_path)?;
    let mut log_reader = std::fs::File::open(&log_path)?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("worker")
        .arg("--cleanup-dir")
        .arg(&directory)
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
        let bytes = json_line(&request)?;
        let stdout = child.stdout.take().context("worker stdout missing")?;
        let ((), grant) = tokio::try_join!(
            async {
                stdin.write_all(&bytes).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            },
            read_grant(stdout, GrantReadEnd::Frame),
        )?;
        Ok::<_, anyhow::Error>(grant)
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
            } else {
                cleanup.disarm();
            }
            delivered
        }
        result => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let diagnostics = worker_log_diagnostics(&mut log_reader);
            match result {
                Ok(Err(error)) => Err(error).with_context(|| {
                    format!("worker startup failed; log (captured): {diagnostics}")
                }),
                Err(error) => Err(error).with_context(|| {
                    format!("worker startup timed out; log (captured): {diagnostics}")
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

async fn read_grant<R: AsyncRead + Unpin>(reader: R, end: GrantReadEnd) -> Result<Grant> {
    let mut reader = BufReader::new(reader);
    let mut phase = GrantPhase::Seeking;
    let mut diagnostic_bytes = 0;
    loop {
        let limit = match phase {
            GrantPhase::Body => MAX_CONTROL,
            GrantPhase::End(_) => GRANT_END.len(),
            _ => MAX_BOOTSTRAP_DIAGNOSTICS - diagnostic_bytes + GRANT_BEGIN.len(),
        };
        let Some(line) = read_bootstrap_line(&mut reader, limit).await? else {
            return match phase {
                GrantPhase::Finished(grant) => Ok(grant),
                GrantPhase::Seeking => bail!("bootstrap stream ended without a grant frame"),
                _ => bail!("bootstrap grant frame is incomplete"),
            };
        };
        phase = match phase {
            GrantPhase::Seeking if line.ends_with(GRANT_BEGIN) => {
                write_bootstrap_diagnostic(
                    &line[..line.len() - GRANT_BEGIN.len()],
                    &mut diagnostic_bytes,
                )?;
                GrantPhase::Body
            }
            GrantPhase::Seeking | GrantPhase::Finished(_) if line.ends_with(GRANT_END) => {
                bail!("unexpected bootstrap grant frame end")
            }
            GrantPhase::Finished(_) if line.ends_with(GRANT_BEGIN) => {
                bail!("duplicate bootstrap grant frame")
            }
            GrantPhase::Body => {
                ensure!(
                    line.last() == Some(&b'\n'),
                    "bootstrap grant body is incomplete"
                );
                let grant = serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("invalid bootstrap grant JSON"))?;
                validate_grant(&grant)?;
                GrantPhase::End(grant)
            }
            GrantPhase::End(grant) => {
                ensure!(line == GRANT_END, "invalid bootstrap grant frame end");
                if matches!(end, GrantReadEnd::Frame) {
                    return Ok(grant);
                }
                GrantPhase::Finished(grant)
            }
            phase => {
                write_bootstrap_diagnostic(&line, &mut diagnostic_bytes)?;
                phase
            }
        };
    }
}

fn write_bootstrap_diagnostic(bytes: &[u8], total: &mut usize) -> Result<()> {
    ensure!(
        !is_unframed_grant(bytes),
        "unframed bootstrap grant; update both Rust endpoints"
    );
    *total += bytes.len();
    ensure!(
        *total <= MAX_BOOTSTRAP_DIAGNOSTICS,
        "bootstrap diagnostics exceed limit"
    );
    std::io::stderr().write_all(bytes)?;
    Ok(())
}

fn is_unframed_grant(line: &[u8]) -> bool {
    let sensitive_fields = [b"\"session\"".as_slice(), b"\"server_cert\"".as_slice()];
    if sensitive_fields
        .iter()
        .any(|key| line.windows(key.len()).any(|part| part == *key))
    {
        return true;
    }
    if let Ok(serde_json::Value::Object(fields)) = serde_json::from_slice(line) {
        return fields.contains_key("session") || fields.contains_key("server_cert");
    }
    line.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{')
}

async fn read_bootstrap_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok((!bytes.is_empty()).then_some(bytes));
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(available.len(), |index| index + 1);
        ensure!(
            bytes.len() + count <= limit,
            "bootstrap output line exceeds limit"
        );
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(bytes));
        }
    }
}

fn validate_request(request: &BootstrapRequest) -> Result<()> {
    ensure!(
        request.version == PROTOCOL_VERSION,
        "unsupported protocol version"
    );
    request.size.validate()?;
    validate_locale_candidates(&request.locale_candidates)?;
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
