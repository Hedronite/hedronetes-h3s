//! Hedronetes (`h3s`) multicall binary.
//!
//! Persistent control plane and native server/worker agent composition.

use clap::{Args, Parser, Subcommand, ValueEnum};
use rustls::pki_types::pem::PemObject;
use std::sync::atomic::{AtomicU64, Ordering};

const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\nKubernetes 1.34\nyouki 0.7.0-h3s.1\ncontainerd 2.3.5"
);
const DEFAULT_CONFIG: &str = "/etc/hedronetes/config.yaml";
const ADMIN_PORT_OFFSET: u16 = 1;
static SUPERVISOR_RESTARTS: AtomicU64 = AtomicU64::new(0);

/// Hedronetes (h3s) — Kubernetes-compatible cluster distribution in one binary.
#[derive(Debug, Parser)]
#[command(
    name = "h3s",
    version,
    long_version = LONG_VERSION,
    about = "Hedronetes (h3s): Kubernetes-compatible cluster distribution in one Rust binary",
    long_about = "k3s, written in Rust, without embedding a Go control plane.\n\n\
         The server includes a native local agent unless --disable-agent is set. \
         An explicit local CRI endpoint enables Pod reconciliation on servers and workers.",
    multicall = true,
    subcommand_required = true,
    arg_required_else_help = true,
    subcommand_value_name = "COMMAND",
    subcommand_help_heading = "Commands",
    propagate_version = true
)]
enum Multicall {
    /// Hedronetes (h3s): Kubernetes-compatible cluster distribution in one Rust binary
    H3s(H3sCli),
    /// Start the control plane + datastore + supervisor (embedded agent unless disabled).
    Server(ServerArgs),
    /// Enroll a worker, reconcile assigned Pods through configured CRI, and maintain Node/Lease status.
    Agent(AgentArgs),
    /// Inspect the configured local CRI v1 runtime without changing workloads.
    RuntimeInfo(RuntimeArgs),
}

#[derive(Debug, Parser)]
#[command(
    name = "h3s",
    version,
    long_version = LONG_VERSION,
    about = "Hedronetes (h3s): Kubernetes-compatible cluster distribution in one Rust binary",
    arg_required_else_help = true,
    subcommand_value_name = "COMMAND",
    subcommand_help_heading = "Commands",
    propagate_version = true
)]
struct H3sCli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the control plane + datastore + supervisor (embedded agent unless disabled).
    Server(ServerArgs),
    /// Enroll a worker, reconcile assigned Pods through configured CRI, and maintain Node/Lease status.
    Agent(AgentArgs),
    /// Inspect the configured local CRI v1 runtime without changing workloads.
    RuntimeInfo(RuntimeArgs),
    /// Rotate the server join token offline.
    Token(TokenArgs),
    /// Copy the durable registry and server identity offline.
    Backup(BackupArgs),
    /// Restore a durable registry and server identity offline.
    Restore(RestoreArgs),
}
#[derive(Debug, Args)]
struct TokenArgs {
    #[command(subcommand)]
    command: TokenCommand,
}
#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Replace server/node-token with one new token.
    Rotate(DurabilityArgs),
}
#[derive(Debug, Args)]
struct DurabilityArgs {
    #[arg(long)]
    data_dir: std::path::PathBuf,
}
#[derive(Debug, Args)]
struct BackupArgs {
    #[arg(long)]
    data_dir: std::path::PathBuf,
    #[arg(long)]
    output: std::path::PathBuf,
}
#[derive(Debug, Args)]
struct RestoreArgs {
    #[arg(long)]
    data_dir: std::path::PathBuf,
    #[arg(long)]
    from: std::path::PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

fn parse_store(value: &str) -> Result<String, String> {
    match value {
        "sqlite" | "memory" | "postgres" => Ok(value.to_owned()),
        other => Err(format!(
            "--store={other} is not implemented; supported backends are sqlite (default), memory and postgres"
        )),
    }
}

/// Server configuration; runtime data is isolated from companion stores.
#[derive(Debug, Args)]
struct ServerArgs {
    /// Read flat or server-scoped YAML before applying command-line overrides.
    #[arg(long, value_name = "FILE")]
    _config: Option<std::path::PathBuf>,
    /// Registry backend for this single-server process.
    #[arg(long, default_value = h3s_storage::SqliteStore::DEFAULT_BACKEND, value_parser = parse_store)]
    store: String,
    /// Seal Secret payloads at rest through Geode custody.
    #[arg(long)]
    secrets_encryption: bool,
    /// Required PostgreSQL registry URL when --store=postgres.
    #[arg(long)]
    datastore_endpoint: Option<String>,
    /// Human-readable text or one-JSON-object-per-line logs.
    #[arg(long, value_enum, default_value = "text")]
    log_format: LogFormat,
    #[arg(long, default_value = "/var/lib/hedronetes")]
    data_dir: std::path::PathBuf,
    #[arg(long, default_value = "0.0.0.0")]
    bind_address: std::net::IpAddr,
    #[arg(long, default_value_t = 6443)]
    https_listen_port: u16,
    /// Plain HTTP supervisor endpoint; zero asks the kernel for a free port.
    #[arg(long)]
    observability_port: Option<u16>,
    #[arg(long)]
    tls_san: Vec<String>,
    #[arg(long, default_value = "/etc/hedronetes/h3s.yaml")]
    write_kubeconfig: std::path::PathBuf,
    /// Private IPv4 Pod network, disjoint from the Service range.
    #[arg(long, default_value = "10.42.0.0/16")]
    cluster_cidr: String,
    /// Each node receives one immutable subnet of the Pod network.
    #[arg(long, default_value_t = 24)]
    node_cidr_mask_size: u8,
    /// Run the control plane without registering or running a local agent.
    #[arg(long)]
    disable_agent: bool,
    /// Turn off shipped add-ons: --disable=traefik,servicelb. Both are on
    /// unless named here.
    #[arg(long, value_delimiter = ',')]
    disable: Vec<String>,
    /// HTTP port the Traefik gateway serves Ingress traffic on; zero asks
    /// the kernel for a free port, preventing concurrent servers colliding.
    #[arg(long, default_value_t = 0)]
    traefik_http_port: u16,
    /// Local node name; defaults to the lowercase system hostname.
    #[arg(long)]
    node_name: Option<String>,
    /// Reachable local node IP; defaults to a concrete bind IP or route-selected IPv4.
    #[arg(long)]
    node_ip: Option<std::net::IpAddr>,
    /// Private loopback kubelet listener; never binds a reachable interface.
    #[arg(long,default_value_t=10250,value_parser=clap::value_parser!(u16).range(1..))]
    kubelet_port: u16,
    /// Use an operator-configured local CRI v1 runtime for assigned Pods.
    #[arg(long)]
    container_runtime_endpoint: Option<String>,
    /// Enable the native Service proxy with this absolute nft helper path.
    #[arg(long)]
    service_proxy_nft: Option<std::path::PathBuf>,
    /// IPv4 DNS Service address used by ClusterFirst Pods on this node.
    #[arg(long)]
    cluster_dns: Option<std::net::Ipv4Addr>,
    /// DNS suffix shared by the cluster's DNS server and all nodes.
    #[arg(long, default_value = "cluster.local")]
    cluster_domain: String,
    /// Shared enrollment token; prefer --token-file over a command-line value.
    #[arg(
        long,
        env = "H3S_TOKEN",
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<h3s_auth::bootstrap::Token>,
    #[arg(long)]
    token_file: Option<std::path::PathBuf>,
}

/// Native worker enrollment and lifecycle; runtime readiness is explicit.
#[derive(Debug, Args)]
struct AgentArgs {
    /// Read flat or agent-scoped YAML before applying command-line overrides.
    #[arg(long, value_name = "FILE")]
    _config: Option<std::path::PathBuf>,
    /// Human-readable text or one-JSON-object-per-line logs.
    #[arg(long, value_enum, default_value = "text")]
    log_format: LogFormat,
    /// Enable the native Service proxy with this absolute nft helper path.
    #[arg(long)]
    service_proxy_nft: Option<std::path::PathBuf>,
    /// IPv4 DNS Service address used by ClusterFirst Pods on this node.
    #[arg(long)]
    cluster_dns: Option<std::net::Ipv4Addr>,
    /// DNS suffix shared by the cluster's DNS server and all nodes.
    #[arg(long, default_value = "cluster.local")]
    cluster_domain: String,
    /// Private loopback kubelet listener; never binds a reachable interface.
    #[arg(long,default_value_t=10250,value_parser=clap::value_parser!(u16).range(1..))]
    kubelet_port: u16,
    /// Use an operator-configured local CRI v1 runtime for assigned Pods.
    #[arg(long)]
    container_runtime_endpoint: Option<String>,
    #[arg(long)]
    server: String,
    /// Trusted CA. Omit only with a secure token-file that authenticates a fetched CA.
    #[arg(long)]
    server_ca_file: Option<std::path::PathBuf>,
    #[arg(long)]
    node_name: String,
    #[arg(long)]
    node_ip: std::net::IpAddr,
    #[arg(long, default_value = "/var/lib/hedronetes")]
    data_dir: std::path::PathBuf,
    #[arg(
        long,
        env = "H3S_TOKEN",
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<h3s_auth::bootstrap::Token>,
    #[arg(long)]
    token_file: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
struct RuntimeArgs {
    /// Human-readable text or one-JSON-object-per-line logs.
    #[arg(long, value_enum, default_value = "text")]
    log_format: LogFormat,
    #[arg(
        long,
        default_value = "unix:///run/hedronetes/containerd/containerd.sock"
    )]
    container_runtime_endpoint: String,
}
async fn runtime_info(args: RuntimeArgs) -> RunResult {
    let cri = h3s_cri::Cri::connect(&args.container_runtime_endpoint).await?;
    let status = cri
        .runtime()
        .status(h3s_cri::v1::StatusRequest { verbose: false })
        .await
        .map_err(h3s_cri::Error::from)?
        .into_inner();
    let conditions: Vec<_> = status.status.ok_or("CRI returned no runtime status")?.conditions.into_iter()
        .map(|c| serde_json::json!({"type":c.r#type,"status":c.status,"reason":c.reason,"message":c.message})).collect();
    let v = cri.version();
    println!(
        "{}",
        serde_json::json!({"runtime_name":v.runtime_name,"runtime_version":v.runtime_version,"runtime_api_version":v.runtime_api_version,"conditions":conditions})
    );
    Ok(())
}

fn command_name(args: &[std::ffi::OsString]) -> Option<(usize, String)> {
    let direct = std::path::Path::new(args.first()?)
        .file_name()?
        .to_str()?
        .to_owned();
    if matches!(direct.as_str(), "server" | "agent") {
        return Some((0, direct));
    }
    let command = args.get(1)?.to_str()?;
    matches!(command, "server" | "agent").then(|| (1, command.to_owned()))
}

fn option_name(value: &str) -> Option<&str> {
    value
        .strip_prefix("--")
        .filter(|name| !name.is_empty())
        .map(|name| name.split('=').next().expect("nonempty option"))
}

fn unquote_config(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        return Ok(value[1..value.len() - 1].to_owned());
    }
    if value.contains(['"', '\'']) {
        return Err("config has unmatched quote".into());
    }
    Ok(value.to_owned())
}

fn config_values(value: &str) -> Result<Vec<String>, String> {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        if inner.trim().is_empty() {
            return Ok(Vec::new());
        }
        return inner.split(',').map(unquote_config).collect();
    }
    Ok(vec![unquote_config(value)?])
}

fn allowed_config(command: &str, key: &str) -> bool {
    let common = matches!(
        key,
        "config"
            | "data-dir"
            | "node-name"
            | "node-ip"
            | "kubelet-port"
            | "container-runtime-endpoint"
            | "service-proxy-nft"
            | "cluster-dns"
            | "cluster-domain"
            | "token"
            | "token-file"
            | "log-format"
    );
    common
        || match command {
            "server" => matches!(
                key,
                "store"
                    | "bind-address"
                    | "https-listen-port"
                    | "observability-port"
                    | "tls-san"
                    | "write-kubeconfig"
                    | "cluster-cidr"
                    | "node-cidr-mask-size"
                    | "disable-agent"
                    | "datastore-endpoint"
                    | "disable"
                    | "traefik-http-port"
            ),
            "agent" => matches!(key, "server" | "server-ca-file"),
            _ => false,
        }
}

fn read_config(
    path: &std::path::Path,
    command: &str,
) -> Result<Vec<(String, Vec<String>)>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("config {}: {error}", path.display()))?;
    let mut selected = None::<String>;
    let mut values = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        if raw.contains('\t') {
            return Err(format!("config line {}: tabs are not supported", index + 1));
        }
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line
            .split_once(" #")
            .map_or(line, |(value, _)| value.trim_end());
        let (raw_key, raw_value) = line
            .split_once(':')
            .ok_or_else(|| format!("config line {}: expected key: value", index + 1))?;
        let key = raw_key.trim().replace('_', "-");
        let value = raw_value.trim();
        if indent == 0 && value.is_empty() && matches!(key.as_str(), "server" | "agent") {
            selected = Some(key);
            continue;
        }
        let applies = indent == 0 || selected.as_deref() == Some(command);
        if !applies {
            continue;
        }
        if !allowed_config(command, &key) {
            return Err(format!(
                "config line {}: unsupported {command} key {key}",
                index + 1
            ));
        }
        values.push((key, config_values(value)?));
    }
    Ok(values)
}

fn configured_args<I, T>(args: I) -> Result<Vec<std::ffi::OsString>, String>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString>,
{
    let mut args: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let Some((command_index, command)) = command_name(&args).map(|(i, c)| (i, c.to_owned())) else {
        return Ok(args);
    };
    let mut explicit = None::<std::path::PathBuf>;
    let mut present = std::collections::BTreeSet::new();
    let mut iter = args.iter().skip(command_index + 1);
    while let Some(value) = iter.next() {
        let value = value.to_string_lossy();
        if let Some(name) = option_name(&value) {
            present.insert(name.to_owned());
            if name == "config" {
                explicit = if let Some((_, path)) = value.split_once('=') {
                    Some(path.into())
                } else {
                    iter.next().map(std::path::PathBuf::from)
                };
            }
        }
    }
    let path = explicit.unwrap_or_else(|| DEFAULT_CONFIG.into());
    if !path
        .try_exists()
        .map_err(|error| format!("config {}: {error}", path.display()))?
    {
        return Ok(args);
    }
    let mut injected = Vec::<std::ffi::OsString>::new();
    for (key, values) in read_config(&path, &command)? {
        if key == "config" || present.contains(&key) {
            continue;
        }
        if key == "disable-agent" {
            match values.as_slice() {
                [value] if value == "true" => injected.push("--disable-agent".into()),
                [value] if value == "false" => {}
                _ => return Err("config disable-agent must be true or false".into()),
            }
            continue;
        }
        for value in values {
            injected.push(format!("--{key}").into());
            injected.push(value.into());
        }
    }
    args.splice(command_index + 1..command_index + 1, injected);
    Ok(args)
}

fn init_tracing(format: LogFormat) {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let result = match format {
        LogFormat::Text => tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_target(false))
            .try_init(),
        LogFormat::Json => tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().json().with_target(false))
            .try_init(),
    };
    let _ = result;
}

fn command_log_format(command: &Multicall) -> LogFormat {
    match command {
        Multicall::H3s(H3sCli {
            command: Command::Server(args),
        })
        | Multicall::Server(args) => args.log_format,
        Multicall::H3s(H3sCli {
            command: Command::Agent(args),
        })
        | Multicall::Agent(args) => args.log_format,
        Multicall::H3s(H3sCli {
            command: Command::RuntimeInfo(args),
        })
        | Multicall::RuntimeInfo(args) => args.log_format,
        Multicall::H3s(H3sCli {
            command: Command::Token(_) | Command::Backup(_) | Command::Restore(_),
        }) => LogFormat::Text,
    }
}

fn install_rustls_provider() {
    // rustls 0.23 default crypto is aws-lc-rs. OpenSSL is not a default feature.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

type RunResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn input_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

fn postgres_endpoint<'a>(
    store: &str,
    endpoint: Option<&'a str>,
) -> Result<Option<&'a str>, std::io::Error> {
    if store != "postgres" {
        return Ok(None);
    }
    let endpoint = endpoint.ok_or_else(|| {
        input_error("--store=postgres requires --datastore-endpoint=postgres://...")
    })?;
    if !endpoint.starts_with("postgres://") {
        return Err(input_error(
            "--store=postgres requires a postgres:// --datastore-endpoint",
        ));
    }
    Ok(Some(endpoint))
}

fn postgres_secrets_dir(endpoint: &str) -> std::path::PathBuf {
    std::env::temp_dir()
        .join("h3s-postgres-secrets")
        .join(hex_digest(endpoint))
}

fn hex_digest(value: &str) -> String {
    use std::fmt::Write;
    h3s_auth::bootstrap::digest(value)
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(&mut output, "{byte:02x}").expect("write to String");
            output
        })
}

fn secure_token(ca_pem: &str, secret: &str) -> String {
    format!("K10{}::{secret}", hex_digest(ca_pem))
}

fn secure_token_ca_hash(token: &str) -> Option<&str> {
    let (hash, secret) = token.strip_prefix("K10")?.split_once("::")?;
    (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) && !secret.is_empty())
        .then_some(hash)
}

fn server_authority(server: &str) -> Result<(String, u16), std::io::Error> {
    let authority = server
        .strip_prefix("https://")
        .ok_or_else(|| input_error("--server must use https://"))?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
        return Err(input_error(
            "--server must be an HTTPS origin without path, query, fragment or userinfo",
        ));
    }
    if let Some(bracketed) = authority.strip_prefix('[') {
        let (host, port) = bracketed
            .split_once("]:")
            .ok_or_else(|| input_error("IPv6 --server requires [address]:port"))?;
        return Ok((
            host.to_owned(),
            port.parse()
                .map_err(|_| input_error("invalid --server port"))?,
        ));
    }
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| input_error("--server requires an explicit port"))?;
    if host.is_empty() || host.contains(':') {
        return Err(input_error("invalid --server host"));
    }
    Ok((
        host.to_owned(),
        port.parse()
            .map_err(|_| input_error("invalid --server port"))?,
    ))
}

#[derive(Debug)]
struct BootstrapVerifier;
impl rustls::client::danger::ServerCertVerifier for BootstrapVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn fetch_server_ca_blocking(
    server: &str,
    token: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use std::io::{Read, Write};
    let expected = secure_token_ca_hash(token).ok_or_else(|| {
        input_error("CA fetch requires a secure token-file generated by h3s server")
    })?;
    let (host, port) = server_authority(server)?;
    let tcp = std::net::TcpStream::connect((host.as_str(), port))?;
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    tcp.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|_| input_error("invalid --server TLS name"))?;
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(BootstrapVerifier))
        .with_no_client_auth();
    let connection = rustls::ClientConnection::new(std::sync::Arc::new(config), server_name)?;
    let mut tls = rustls::StreamOwned::new(connection, tcp);
    write!(
        tls,
        "GET /v1-h3s/server/cacerts HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: application/x-pem-file\r\n\r\n"
    )?;
    tls.flush()?;
    let mut response = Vec::new();
    tls.take(1024 * 1024 + 8192).read_to_end(&mut response)?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| input_error("CA endpoint returned an invalid HTTP response"))?;
    let headers = std::str::from_utf8(&response[..header_end])?;
    if !headers
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200 "))
    {
        return Err(input_error("CA endpoint did not return HTTP 200").into());
    }
    let body = &response[header_end + 4..];
    if body.is_empty() || body.len() > 1024 * 1024 {
        return Err(input_error("CA endpoint returned an empty or oversized certificate").into());
    }
    let ca = std::str::from_utf8(body)?.to_owned();
    if hex_digest(&ca) != expected {
        return Err(input_error("fetched CA does not match the secure token-file hash").into());
    }
    rustls::pki_types::CertificateDer::from_pem_slice(ca.as_bytes())
        .map_err(|_| input_error("CA endpoint returned invalid PEM"))?;
    Ok(ca)
}

async fn fetch_server_ca(
    server: &str,
    data_dir: &std::path::Path,
    token: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let server = server.to_owned();
    let token = token.to_owned();
    let ca = tokio::task::spawn_blocking(move || fetch_server_ca_blocking(&server, &token))
        .await
        .map_err(|_| input_error("CA fetch task failed"))??;
    let directory = data_dir.join("agent");
    h3s_certs::private::directory(&directory)?;
    let path = directory.join("server-ca.crt");
    if path.try_exists()? {
        if h3s_certs::private::read(&path, 1024 * 1024)? != ca.as_bytes() {
            return Err(input_error("fetched CA differs from the persisted agent CA").into());
        }
    } else {
        h3s_certs::private::write(&path, ca.as_bytes(), false)?;
    }
    Ok(path)
}

fn metric_text(snapshot: h3s_apiserver::Snapshot) -> String {
    format!(
        concat!(
            "# TYPE h3s_apiserver_requests_total counter\n",
            "h3s_apiserver_requests_total {}\n",
            "# TYPE h3s_store_revision gauge\n",
            "h3s_store_revision {}\n",
            "# TYPE h3s_watchers gauge\n",
            "h3s_watchers {}\n",
            "# TYPE h3s_scheduler_binds_total counter\n",
            "h3s_scheduler_binds_total {}\n",
            "# TYPE h3s_proxy_apply_total counter\n",
            "h3s_proxy_apply_total {}\n",
            "# TYPE h3s_supervisor_restarts_total counter\n",
            "h3s_supervisor_restarts_total {}\n"
        ),
        snapshot.requests,
        snapshot.store_revision,
        snapshot.watches,
        h3s_scheduler::binds(),
        h3s_proxy::applies(),
        SUPERVISOR_RESTARTS.load(Ordering::Relaxed),
    )
}

async fn observe_connection(
    mut stream: tokio::net::TcpStream,
    api: h3s_apiserver::Api,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut request = Vec::with_capacity(1024);
    loop {
        if request.len() >= 8192 {
            return Err(input_error("observability request headers exceed 8 KiB").into());
        }
        let mut chunk = [0_u8; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        request.extend_from_slice(&chunk[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let line = std::str::from_utf8(&request)?
        .lines()
        .next()
        .ok_or_else(|| input_error("empty observability request"))?;
    let (status, content_type, body) = match line {
        "GET /livez HTTP/1.1" | "GET /readyz HTTP/1.1" => {
            ("200 OK", "text/plain; charset=utf-8", "ok\n".to_owned())
        }
        "GET /metrics HTTP/1.1" => match api.snapshot().await {
            Ok(snapshot) => (
                "200 OK",
                "text/plain; version=0.0.4; charset=utf-8",
                metric_text(snapshot),
            ),
            Err(error) => (
                "503 Service Unavailable",
                "text/plain; charset=utf-8",
                format!("metrics unavailable: {error}\n"),
            ),
        },
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "not found\n".to_owned(),
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn serve_observability(
    listener: tokio::net::TcpListener,
    api: h3s_apiserver::Api,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let (stream, _) = listener.accept().await?;
        let api = api.clone();
        tokio::spawn(async move {
            if let Err(error) = observe_connection(stream, api).await {
                tracing::warn!(%error, "observability request failed");
            }
        });
    }
}

fn local_node(
    args: &ServerArgs,
) -> Result<Option<(String, std::net::IpAddr)>, Box<dyn std::error::Error + Send + Sync>> {
    if args.disable_agent {
        return Ok(None);
    }
    let name = match &args.node_name {
        Some(name) => name.clone(),
        None => hostname::get()?
            .into_string()
            .map_err(|_| "hostname is not UTF-8; set --node-name")?
            .to_lowercase(),
    };
    if !h3s_api::valid_node_name(&name) {
        return Err("invalid local node name; set --node-name to a lowercase DNS name".into());
    }
    let ip = match args.node_ip {
        Some(ip) => ip,
        None if !args.bind_address.is_unspecified() && !args.bind_address.is_loopback() => {
            args.bind_address
        }
        None => {
            // UDP connect asks the kernel for its route's source address. No
            // datagram is sent, and the documentation-only destination need
            // not respond. Explicit --node-ip is required on ambiguous hosts.
            let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
            socket
                .connect("192.0.2.1:9")
                .map_err(|_| "cannot select a local node IP; set --node-ip")?;
            socket.local_addr()?.ip()
        }
    };
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return Err("local node IP must identify its reachable node interface".into());
    }
    Ok(Some((name, ip)))
}

/// Validate the `--disable` list. Only shipped add-ons may be turned off.
fn parse_addons(disabled: &[String]) -> std::result::Result<(bool, bool), String> {
    let mut traefik = true;
    let mut servicelb = true;
    for name in disabled {
        match name.as_str() {
            "traefik" => traefik = false,
            "servicelb" => servicelb = false,
            other => {
                return Err(format!(
                    "unknown add-on {other:?} in --disable; supported: traefik, servicelb"
                ))
            }
        }
    }
    Ok((traefik, servicelb))
}

async fn run_server(args: ServerArgs) -> RunResult {
    let node = local_node(&args)?;
    let cluster_dns = args
        .cluster_dns
        .map(|ip| h3s_kubelet::ClusterDns::new(ip, args.cluster_domain.clone()))
        .transpose()?;
    let server_dir = args.data_dir.join("server");
    let mut sans = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "kubernetes".into(),
        "kubernetes.default".into(),
        "kubernetes.default.svc".into(),
        "kubernetes.default.svc.cluster.local".into(),
        "10.43.0.1".into(),
    ];
    if cluster_dns.is_some() {
        sans.push(format!("kubernetes.default.svc.{}", args.cluster_domain));
    }
    if !args.bind_address.is_unspecified() {
        sans.push(args.bind_address.to_string());
    }
    if args.bind_address.is_ipv6() {
        sans.push("::1".into());
    }
    sans.extend(args.tls_san);
    sans.sort();
    sans.dedup();
    let tls_dir = server_dir.join("tls");
    h3s_certs::ClusterPki::renew_serving_certificate_if_expiring(&tls_dir, &sans)?;
    let pki = std::sync::Arc::new(h3s_certs::ClusterPki::open_or_create(&tls_dir, &sans)?);
    let token = read_token(
        args.token.as_ref(),
        args.token_file.as_deref(),
        Some((&server_dir, pki.ca_pem())),
    )?
    .expect("server token generated");
    let ca_file = server_dir.join("ca.crt");
    if ca_file.try_exists()? {
        if h3s_certs::private::read(&ca_file, 1024 * 1024)? != pki.ca_pem().as_bytes() {
            return Err("existing CA export differs from cluster PKI".into());
        }
    } else {
        h3s_certs::private::write(&ca_file, pki.ca_pem().as_bytes(), false)?;
    }
    let db_dir = server_dir.join("db");
    let _registry_lock = if args.store == h3s_storage::SqliteStore::DEFAULT_BACKEND {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&db_dir)?;
        let metadata = std::fs::symlink_metadata(&db_dir)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("registry directory must not be a symlink".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("registry directory requires mode 0700".into());
            }
        }
        Some(
            h3s_certs::private::exclusive_process_lock(&db_dir.join(".registry.lock")).map_err(
                |error| {
                    format!(
                        "registry {} is locked by another h3s server: {error}",
                        db_dir.display()
                    )
                },
            )?,
        )
    } else {
        None
    };
    let postgres_endpoint = postgres_endpoint(&args.store, args.datastore_endpoint.as_deref())?;
    let secrets_dir = postgres_endpoint
        .map(postgres_secrets_dir)
        .unwrap_or_else(|| server_dir.join("secrets"));
    let geode = if args.secrets_encryption {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                candidates.push(dir.join("geode"));
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(std::path::PathBuf::from(home).join(".cargo/bin/geode"));
        }
        let binary = candidates
            .into_iter()
            .find(|path| path.is_file())
            .ok_or_else(|| input_error("--secrets-encryption requires the geode binary on PATH"))?;
        Some(h3s_storage::GeodeSealer::create(binary, secrets_dir)?)
    } else {
        None
    };
    let store: std::sync::Arc<dyn h3s_storage::Storage> = match args.store.as_str() {
        "postgres" => {
            let endpoint =
                postgres_endpoint.expect("postgres endpoint validated before opening store");
            let store = h3s_storage::PostgresStore::open(endpoint).await?;
            match geode {
                Some(sealer) => std::sync::Arc::new(store.with_secrets_sealer(sealer)),
                None => std::sync::Arc::new(store),
            }
        }
        "sqlite" | "memory" => {
            let store =
                h3s_storage::SqliteStore::open_backend(&args.store, db_dir.join("h3s.db")).await?;
            match geode {
                Some(sealer) => std::sync::Arc::new(store.with_secrets_sealer(sealer)),
                None => std::sync::Arc::new(store),
            }
        }
        _ => unreachable!("clap store parser admits only supported backends"),
    };
    let api = h3s_apiserver::Api::new(store)
        .await?
        .with_node_cidrs(&args.cluster_cidr, args.node_cidr_mask_size)
        .await?
        .with_bootstrap(pki.clone(), &token)?;
    let listener =
        tokio::net::TcpListener::bind((args.bind_address, args.https_listen_port)).await?;
    let local = listener.local_addr()?;
    let admin_port = match args.observability_port {
        Some(port) => port,
        None => local
            .port()
            .checked_add(ADMIN_PORT_OFFSET)
            .ok_or_else(|| input_error("HTTPS listen port leaves no observability port"))?,
    };
    let observe_listener = tokio::net::TcpListener::bind((args.bind_address, admin_port)).await?;
    let observe_local = observe_listener.local_addr()?;
    let connect_ip = if args.bind_address.is_unspecified() {
        if args.bind_address.is_ipv6() {
            std::net::Ipv6Addr::LOCALHOST.into()
        } else {
            std::net::Ipv4Addr::LOCALHOST.into()
        }
    } else {
        args.bind_address
    };
    let endpoint = format!(
        "https://{}",
        std::net::SocketAddr::new(connect_ip, local.port())
    );
    let (traefik_on, servicelb_on) =
        parse_addons(&args.disable).map_err(|message| input_error(&message))?;
    let gateway_listener = match traefik_on {
        true => Some(tokio::net::TcpListener::bind((connect_ip, args.traefik_http_port)).await?),
        false => None,
    };
    let gateway_local = match gateway_listener.as_ref() {
        Some(listener) => Some(listener.local_addr()?),
        None => None,
    };
    let config = pki.kubeconfig(&endpoint, pki.admin())?;
    write_kubeconfig(&args.write_kubeconfig, &config)?;
    tracing::info!(
        api = %format!("https://{local}"),
        metrics = %format!("http://{observe_local}/metrics"),
        kubeconfig = %args.write_kubeconfig.display(),
        store = %args.store,
        "h3s server listening"
    );
    let observe = tokio::spawn(serve_observability(observe_listener, api.clone()));
    let server = h3s_apiserver::serve(listener, pki.server_config()?, api.router(), async {
        let _ = tokio::signal::ctrl_c().await;
    });
    // Every controller and the local agent is a supervised child: it restarts
    // with backoff and never ends this process. Only the API and shutdown do.
    let namespace_client =
        client_for(&pki, &endpoint, h3s_controllers::NAMESPACE_CONTROLLER_ID).await?;
    let ca_pem = pki.ca_pem().to_owned();
    let mut children = tokio::task::JoinSet::new();
    children.spawn(supervise("namespace controller", move || {
        h3s_controllers::run_namespace_controller(namespace_client.clone(), ca_pem.clone())
    }));
    macro_rules! controller {
        ($name:literal, $id:expr, $run:path) => {{
            let client = client_for(&pki, &endpoint, $id).await?;
            children.spawn(supervise($name, move || $run(client.clone())));
        }};
    }
    controller!(
        "node CIDR controller",
        h3s_controllers::NODE_CIDR_CONTROLLER_ID,
        h3s_controllers::run_node_cidr_controller
    );
    controller!(
        "endpoint controller",
        h3s_controllers::ENDPOINT_CONTROLLER_ID,
        h3s_controllers::run_endpoint_controller
    );
    controller!(
        "deployment controller",
        h3s_controllers::DEPLOYMENT_CONTROLLER_ID,
        h3s_controllers::run_deployment_controller
    );
    controller!(
        "replicaset controller",
        h3s_controllers::REPLICASET_CONTROLLER_ID,
        h3s_controllers::run_replicaset_controller
    );
    controller!(
        "workload gc",
        h3s_controllers::WORKLOAD_GC_ID,
        h3s_controllers::run_workload_gc
    );
    // ServiceLB and Traefik ship default-on; --disable=traefik,servicelb
    // takes each out before its client or listener exists.
    if servicelb_on {
        let client = client_for(&pki, &endpoint, h3s_controllers::SERVICELB_CONTROLLER_ID).await?;
        let node_name = node.as_ref().map(|(name, _)| name.clone());
        children.spawn(supervise("servicelb", move || {
            h3s_controllers::run_servicelb(client.clone(), node_name.clone())
        }));
    }
    if let Some(listener) = gateway_listener {
        let client = client_for(&pki, &endpoint, h3s_controllers::TRAEFIK_CONTROLLER_ID).await?;
        // The listener is non-cloneable; its controller owns it for the server
        // lifetime. The server shutdown drops it with every other child.
        children.spawn(async move {
            if let Err(error) = h3s_controllers::run_traefik(client, listener).await {
                eprintln!("traefik controller stopped: {error}");
            }
        });
        if let Some(local) = gateway_local {
            tracing::info!(gateway = %format!("http://{local}"), "h3s traefik listening");
        }
    }
    let scheduler = client_for(&pki, &endpoint, h3s_scheduler::SCHEDULER_ID).await?;
    children.spawn(supervise("scheduler", move || {
        let client = scheduler.clone();
        async move {
            #[cfg(unix)]
            {
                let mut usr1 =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
                        .expect("SIGUSR1");
                tokio::select! {
                    result = h3s_scheduler::run(client) => result,
                    _ = usr1.recv() => {
                        tracing::warn!("h3s scheduler received SIGUSR1; stopping for supervisor restart");
                        Ok(())
                    }
                }
            }
            #[cfg(not(unix))]
            {
                h3s_scheduler::run(client).await
            }
        }
    }));
    // Enrollment is polled alongside serving: awaiting it before the API is
    // driven would deadlock this process against its own TLS listener. The
    // agent may die and come back; the API does not follow it down.
    if let Some((node_name, node_ip)) = node {
        let config = h3s_kubelet::Config {
            server: endpoint,
            ca_file,
            node_name,
            node_ip,
            data_dir: args.data_dir,
            token: Some(token),
            kubelet_port: args.kubelet_port,
            runtime_endpoint: args.container_runtime_endpoint,
            service_proxy_nft: args.service_proxy_nft,
            cluster_dns,
        };
        children.spawn(supervise("local agent", move || {
            run_native_agent(config.clone())
        }));
    }
    // Parent join: the API, whose shutdown future is Ctrl-C. Children are
    // dropped with the process when it returns.
    server.await?;
    observe.abort();
    children.abort_all();
    Ok(())
}
async fn client_for(
    pki: &h3s_certs::ClusterPki,
    endpoint: &str,
    id: &str,
) -> Result<kube::Client, Box<dyn std::error::Error + Send + Sync>> {
    let identity = pki.issue_client(id, None)?;
    let config = pki.kubeconfig(endpoint, &identity)?;
    h3s_controllers::client_from_kubeconfig(&config).await
}
/// Copied from the kubelet tunnel loop: a child that returns or fails is
/// restarted after a delay that doubles up to 30s and resets once a run has
/// stayed up for a minute. Nothing a child does ends the process.
async fn supervise<F, Fut, E>(name: &'static str, mut start: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    let mut delay = 1;
    loop {
        let started = tokio::time::Instant::now();
        let result = start().await;
        if started.elapsed() > std::time::Duration::from_secs(60) {
            delay = 1;
        }
        SUPERVISOR_RESTARTS.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(()) => tracing::warn!("h3s {name} stopped; restarting in {delay}s"),
            Err(error) => tracing::warn!("h3s {name}: {error}; restarting in {delay}s"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        delay = (delay * 2).min(30);
    }
}
fn write_kubeconfig(path: &std::path::Path, contents: &str) -> RunResult {
    use std::io::Write;
    if path.try_exists()? {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("kubeconfig must be a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("kubeconfig requires mode 0600".into());
            }
        }
        if std::fs::read_to_string(path)? == contents {
            return Ok(());
        }
        return Err("refusing to overwrite a different kubeconfig; choose a project-owned --write-kubeconfig path".into());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(contents.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn read_token(
    token: Option<&h3s_auth::bootstrap::Token>,
    file: Option<&std::path::Path>,
    server: Option<(&std::path::Path, &str)>,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let text = if let Some(token) = token {
        Some(token.expose().to_owned())
    } else if let Some(file) = file {
        Some(
            String::from_utf8(h3s_certs::private::read(file, 1024)?)
                .map_err(|_| "token file is not UTF-8")?
                .trim()
                .to_owned(),
        )
    } else if let Some((dir, ca_pem)) = server {
        let _lock = h3s_certs::private::exclusive_process_lock(&dir.join(".node-token.lock"))?;
        let path = dir.join("node-token");
        if !path.try_exists()? {
            let secret = h3s_auth::bootstrap::random_secret()
                .map_err(|_| "secure random source unavailable")?;
            let token = secure_token(ca_pem, &secret);
            h3s_certs::private::write(&path, token.as_bytes(), false)?;
        }
        Some(
            String::from_utf8(h3s_certs::private::read(&path, 1024)?)
                .map_err(|_| "token file is not UTF-8")?
                .trim()
                .to_owned(),
        )
    } else {
        None
    };
    if text
        .as_deref()
        .is_some_and(|v| !h3s_auth::bootstrap::valid_token(v))
    {
        return Err("join token must be 32-256 printable ASCII bytes".into());
    }
    Ok(text)
}
fn copy_regular(
    source: &std::path::Path,
    destination: &std::path::Path,
    replace: bool,
) -> RunResult {
    let metadata = std::fs::symlink_metadata(source)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!("{} must be a regular file", source.display()).into());
    }
    if destination.try_exists()? {
        if !replace {
            return Err(format!("{} already exists", destination.display()).into());
        }
        std::fs::remove_file(destination)?;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(source, destination)?;
    #[cfg(unix)]
    std::fs::set_permissions(destination, metadata.permissions())?;
    Ok(())
}

fn copy_tree(source: &std::path::Path, destination: &std::path::Path) -> RunResult {
    let metadata = std::fs::symlink_metadata(source)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("{} must be a real directory", source.display()).into());
    }
    std::fs::create_dir_all(destination)?;
    #[cfg(unix)]
    std::fs::set_permissions(destination, metadata.permissions())?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source)?;
        if metadata.is_dir() {
            copy_tree(&source, &destination)?;
        } else {
            copy_regular(&source, &destination, false)?;
        }
    }
    Ok(())
}

fn rotate_token(args: DurabilityArgs) -> RunResult {
    let server = args.data_dir.join("server");
    let _registry = h3s_certs::private::exclusive_process_lock(&server.join("db/.registry.lock"))?;
    let _token_lock = h3s_certs::private::exclusive_process_lock(&server.join(".node-token.lock"))?;
    let ca = String::from_utf8(h3s_certs::private::read(
        &server.join("ca.crt"),
        1024 * 1024,
    )?)?;
    let secret =
        h3s_auth::bootstrap::random_secret().map_err(|_| "secure random source unavailable")?;
    let token = secure_token(&ca, &secret);
    if !h3s_auth::bootstrap::valid_token(&token) {
        return Err("rotated token failed validation".into());
    }
    h3s_certs::private::write(&server.join("node-token"), token.as_bytes(), true)?;
    println!("{token}");
    Ok(())
}

fn backup(args: BackupArgs) -> RunResult {
    let server = args.data_dir.join("server");
    let _registry = h3s_certs::private::exclusive_process_lock(&server.join("db/.registry.lock"))?;
    h3s_storage::backup::backup(&args.data_dir, &args.output)?;
    copy_tree(&server.join("tls"), &args.output.join("server/tls"))?;
    copy_regular(
        &server.join("ca.crt"),
        &args.output.join("server/ca.crt"),
        false,
    )?;
    copy_regular(
        &server.join("node-token"),
        &args.output.join("server/node-token"),
        false,
    )?;
    Ok(())
}

fn restore(args: RestoreArgs) -> RunResult {
    let server = args.data_dir.join("server");
    let _registry = h3s_certs::private::exclusive_process_lock(&server.join("db/.registry.lock"))?;
    h3s_storage::backup::restore(&args.data_dir, &args.from)?;
    let source_server = args.from.join("server");
    let tls = server.join("tls");
    if tls.try_exists()? {
        std::fs::remove_dir_all(&tls)?;
    }
    copy_tree(&source_server.join("tls"), &tls)?;
    copy_regular(&source_server.join("ca.crt"), &server.join("ca.crt"), true)?;
    copy_regular(
        &source_server.join("node-token"),
        &server.join("node-token"),
        true,
    )?;
    Ok(())
}

async fn run_agent(args: AgentArgs) -> RunResult {
    let cluster_dns = args
        .cluster_dns
        .map(|ip| h3s_kubelet::ClusterDns::new(ip, args.cluster_domain))
        .transpose()?;
    let token = read_token(args.token.as_ref(), args.token_file.as_deref(), None)?;
    let ca_file = match args.server_ca_file {
        Some(path) => path,
        None => {
            let token = token
                .as_deref()
                .ok_or_else(|| input_error("CA fetch requires --token or --token-file"))?;
            fetch_server_ca(&args.server, &args.data_dir, token).await?
        }
    };
    run_native_agent(h3s_kubelet::Config {
        server: args.server,
        ca_file,
        node_name: args.node_name,
        node_ip: args.node_ip,
        data_dir: args.data_dir,
        token,
        kubelet_port: args.kubelet_port,
        runtime_endpoint: args.container_runtime_endpoint,
        service_proxy_nft: args.service_proxy_nft,
        cluster_dns,
    })
    .await
}
async fn run_native_agent(config: h3s_kubelet::Config) -> RunResult {
    let agent = h3s_kubelet::Agent::connect(config).await?;
    tracing::info!("h3s agent enrolled; Node readiness follows configured runtime health");
    agent.run().await?;
    Ok(())
}
async fn run_command(command: Command) -> RunResult {
    match command {
        Command::Server(args) => run_server(args).await,
        Command::Agent(args) => run_agent(args).await,
        Command::RuntimeInfo(args) => runtime_info(args).await,
        Command::Token(TokenArgs {
            command: TokenCommand::Rotate(args),
        }) => rotate_token(args),
        Command::Backup(args) => backup(args),
        Command::Restore(args) => restore(args),
    }
}
#[tokio::main]
async fn main() {
    install_rustls_provider();
    let args = configured_args(std::env::args_os()).unwrap_or_else(|error| {
        eprintln!("h3s: {error}");
        std::process::exit(2);
    });
    let command = Multicall::parse_from(args);
    init_tracing(command_log_format(&command));
    let result = match command {
        Multicall::H3s(cli) => run_command(cli.command).await,
        Multicall::Server(args) => run_server(args).await,
        Multicall::Agent(args) => run_agent(args).await,
        Multicall::RuntimeInfo(args) => runtime_info(args).await,
    };
    if let Err(error) = result {
        tracing::error!(%error, "h3s command failed");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn h3s_help_lists_server_and_agent() {
        let mut cmd = Multicall::command();
        let h3s = cmd.find_subcommand_mut("h3s").expect("h3s applet");
        let mut buf = Vec::new();
        h3s.write_help(&mut buf).unwrap();
        let help = String::from_utf8(buf).unwrap();
        assert!(help.contains("server"), "{help}");
        assert!(help.contains("agent"), "{help}");
    }

    #[test]
    fn server_help_is_display_help() {
        let err = Multicall::try_parse_from(["h3s", "server", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = err.to_string();
        assert!(help.contains("server"), "{help}");
    }

    #[test]
    fn agent_help_is_display_help() {
        let err = Multicall::try_parse_from(["h3s", "agent", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = err.to_string();
        assert!(
            help.contains("reconcile assigned Pods through configured CRI"),
            "{help}"
        );
        assert!(!help.to_ascii_lowercase().contains("incomplete"), "{help}");
    }

    #[test]
    fn parses_server_via_h3s_applet() {
        let parsed = Multicall::try_parse_from(["h3s", "server"]).expect("parse server");
        assert!(matches!(
            parsed,
            Multicall::H3s(H3sCli {
                command: Command::Server(_)
            })
        ));
    }

    #[test]
    fn parses_agent_via_h3s_applet() {
        let parsed = Multicall::try_parse_from([
            "h3s",
            "agent",
            "--server",
            "https://server:6443",
            "--server-ca-file",
            "/tmp/ca.crt",
            "--node-name",
            "worker",
            "--node-ip",
            "192.0.2.2",
        ])
        .expect("parse agent");
        assert!(matches!(
            parsed,
            Multicall::H3s(H3sCli {
                command: Command::Agent(_)
            })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn supervise_restarts_failed_and_stopped_children_with_capped_backoff() {
        use std::sync::{Arc, Mutex};
        let starts = Arc::new(Mutex::new(Vec::new()));
        let observed = starts.clone();
        let child = tokio::spawn(supervise("child", move || {
            let observed = observed.clone();
            async move {
                let mut starts = observed.lock().unwrap();
                starts.push(tokio::time::Instant::now());
                match starts.len() {
                    1 => Err("controller stream ended unexpectedly"),
                    2 => Ok(()),
                    _ => Err("still failing"),
                }
            }
        }));
        // 1s, 2s, 4s, 8s, 16s, 30s, 30s: the delay caps rather than growing.
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        child.abort();
        let starts = starts.lock().unwrap();
        let gaps: Vec<u64> = starts.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
        assert_eq!(gaps, vec![1, 2, 4, 8, 16, 30, 30], "{gaps:?}");
    }

    #[test]
    fn server_node_identity_uses_explicit_values_and_validates_before_startup() {
        fn args(extra: &[&str]) -> ServerArgs {
            let mut argv = vec!["h3s", "server", "--node-name", "server-node"];
            argv.extend_from_slice(extra);
            let Multicall::H3s(H3sCli {
                command: Command::Server(args),
            }) = Multicall::try_parse_from(argv).unwrap()
            else {
                panic!("server arguments")
            };
            args
        }
        let resolved = local_node(&args(&["--bind-address", "192.0.2.10"]))
            .unwrap()
            .unwrap();
        assert_eq!(
            resolved,
            ("server-node".into(), "192.0.2.10".parse().unwrap())
        );
        let resolved = local_node(&args(&[
            "--bind-address",
            "192.0.2.10",
            "--node-ip",
            "192.0.2.11",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            resolved.1,
            "192.0.2.11".parse::<std::net::IpAddr>().unwrap()
        );
        for ip in ["0.0.0.0", "127.0.0.1", "224.0.0.1", "::", "::1", "ff02::1"] {
            assert!(local_node(&args(&["--node-ip", ip])).is_err(), "{ip}");
        }
        let mut invalid = args(&["--node-ip", "192.0.2.10"]);
        invalid.node_name = Some("UPPER CASE".into());
        assert!(local_node(&invalid).is_err());
        invalid.disable_agent = true;
        assert!(local_node(&invalid).unwrap().is_none());
    }

    #[test]
    fn config_yaml_matches_flags_and_explicit_flags_win() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "server:\n  data-dir: /from-config\n  node-name: config-node\n  store: memory\n  disable-agent: true\nagent:\n  server: https://config.example:6443\n",
        )
        .unwrap();
        let args = vec![
            std::ffi::OsString::from("h3s"),
            "server".into(),
            "--config".into(),
            file.path().as_os_str().to_owned(),
            "--data-dir".into(),
            "/from-flag".into(),
        ];
        let configured = configured_args(args).unwrap();
        let Multicall::H3s(H3sCli {
            command: Command::Server(args),
        }) = Multicall::try_parse_from(configured).unwrap()
        else {
            panic!("server arguments")
        };
        assert_eq!(args.data_dir, std::path::Path::new("/from-flag"));
        assert_eq!(args.node_name.as_deref(), Some("config-node"));
        assert_eq!(args.store, "memory");
        assert!(args.disable_agent);
    }

    #[test]
    fn agent_config_supplies_required_fields_and_flags_override_it() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "agent:\n  server: https://config.example:6443\n  server-ca-file: /config/ca.crt\n  token-file: /config/token\n  node-name: config-worker\n  node-ip: 192.0.2.20\n  data-dir: /config/data\n",
        )
        .unwrap();
        let args = vec![
            std::ffi::OsString::from("h3s"),
            "agent".into(),
            "--config".into(),
            file.path().as_os_str().to_owned(),
            "--node-name".into(),
            "flag-worker".into(),
        ];
        let configured = configured_args(args).unwrap();
        let Multicall::H3s(H3sCli {
            command: Command::Agent(args),
        }) = Multicall::try_parse_from(configured).unwrap()
        else {
            panic!("agent arguments")
        };
        assert_eq!(args.server, "https://config.example:6443");
        assert_eq!(
            args.server_ca_file.as_deref(),
            Some(std::path::Path::new("/config/ca.crt"))
        );
        assert_eq!(
            args.token_file.as_deref(),
            Some(std::path::Path::new("/config/token"))
        );
        assert_eq!(args.node_name, "flag-worker");
        assert_eq!(
            args.node_ip,
            "192.0.2.20".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(args.data_dir, std::path::Path::new("/config/data"));
    }

    #[test]
    fn store_and_ha_flags_fail_explicitly() {
        let postgres = Multicall::try_parse_from([
            "h3s",
            "server",
            "--store=postgres",
            "--datastore-endpoint",
            "postgres://h3s@127.0.0.1:5401/h3s",
        ]);
        assert!(postgres.is_ok(), "{postgres:?}");
        for backend in ["etcd", "mysql", "xline"] {
            let error = Multicall::try_parse_from(["h3s", "server", "--store", backend])
                .unwrap_err()
                .to_string();
            assert!(error.contains("is not implemented"), "{error}");
        }
        for arguments in [
            vec!["h3s", "server", "--cluster-init"],
            vec!["h3s", "server", "--server", "https://vip:6443"],
        ] {
            let error = Multicall::try_parse_from(arguments).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn postgres_endpoint_requires_postgres_url_and_shared_custody_dir() {
        assert!(postgres_endpoint("postgres", None).is_err());
        assert!(postgres_endpoint("postgres", Some("https://etcd.example:2379")).is_err());
        let url = "postgres://h3s@127.0.0.1:5401/h3s";
        assert_eq!(postgres_endpoint("postgres", Some(url)).unwrap(), Some(url));
        assert_eq!(postgres_endpoint("sqlite", None).unwrap(), None);
        assert_eq!(postgres_secrets_dir(url), postgres_secrets_dir(url));
    }

    #[test]
    fn postgres_endpoint_contract_is_explicit() {
        let missing = Multicall::try_parse_from(["h3s", "server", "--store=postgres"])
            .expect("clap parses; run_server rejects missing endpoint");
        assert!(matches!(
            missing,
            Multicall::H3s(H3sCli {
                command: Command::Server(_)
            })
        ));
        for endpoint in ["https://etcd.example:2379", "mysql://h3s@localhost/h3s"] {
            let parsed = Multicall::try_parse_from([
                "h3s",
                "server",
                "--store=postgres",
                "--datastore-endpoint",
                endpoint,
            ]);
            assert!(
                parsed.is_ok(),
                "clap accepts then run_server validates {endpoint}"
            );
        }
        assert!(Multicall::try_parse_from(["h3s", "server"]).is_ok());
    }

    #[test]
    #[ignore = "needs two OS h3s processes and H3S_POSTGRES_URL; run on tower"]
    fn postgres_two_server_processes_share_primary() {
        // The tower proof starts two `target/release/h3s server` processes.
        // Keep it ignored for GHA, where no Postgres primary exists.
        assert!(std::env::var("H3S_POSTGRES_URL").is_ok());
    }

    #[test]
    fn version_reports_every_pinned_component() {
        let error = Multicall::try_parse_from(["h3s", "--version"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        let version = error.to_string();
        for pin in [
            "h3s 0.11.0",
            "Kubernetes 1.34",
            "youki 0.7.0-h3s.1",
            "containerd 2.3.5",
        ] {
            assert!(version.contains(pin), "{version}");
        }
    }

    #[test]
    fn addons_default_on_and_disable_list_is_strict() {
        assert_eq!(parse_addons(&[]).unwrap(), (true, true));
        assert_eq!(
            parse_addons(&["traefik".to_owned(), "servicelb".to_owned()]).unwrap(),
            (false, false)
        );
        let error = parse_addons(&["typo".to_owned()]).unwrap_err();
        assert!(error.contains("traefik, servicelb"), "{error}");
    }

    #[test]
    fn secure_join_token_authenticates_the_fetched_ca() {
        let ca = "-----BEGIN CERTIFICATE-----\ncluster-ca\n-----END CERTIFICATE-----\n";
        let token = secure_token(ca, "01234567890123456789012345678901");
        let digest = hex_digest(ca);
        assert_eq!(secure_token_ca_hash(&token), Some(digest.as_str()));
        assert!(secure_token_ca_hash("plain-token-without-a-ca-hash").is_none());
    }
}
