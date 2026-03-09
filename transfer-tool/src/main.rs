use axum::{extract::Path, routing::get, Router};
use clap::{Parser, ValueEnum};
use common::{AppConfig, Metric, ReportingClient};
use futures_util::StreamExt;
use quinn::{ClientConfig, Endpoint, ServerConfig};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::time::{Duration, Instant};
use tracing::{error, info};

#[derive(Parser)]
#[command(
    name = "transfer-tool",
    author,
    version,
    about = "High-performance network throughput benchmarking suite",
    long_about = "A multi-protocol throughput testing tool supporting QUIC, HTTP, HTTPS, and raw TCP. \
                  Includes probabilistic traffic scenarios and real-time metrics reporting to a central dashboard. \
                  Defaults for many flags can be specified in a 'config.toml' file."
)]
struct Cli {
    /// Mode ('server', 'client') or a target IP/hostname (assumes client)
    #[arg(index = 1)]
    mode_or_target: Option<String>,

    /// Target IP/hostname when 'client' is explicitly specified
    #[arg(index = 2)]
    explicit_target: Option<String>,

    /// Address to listen on (e.g., 0.0.0.0:4433).
    #[arg(short, long, default_value = "0.0.0.0:4433")]
    listen: SocketAddr,

    /// Protocol for the transfer (quic, http, https, tcp).
    #[arg(short, long, default_value = "quic")]
    proto: Protocol,

    /// Explicit size of data to download (e.g., 1048576 for 1MB).
    #[arg(short, long)]
    size: Option<u64>,

    /// Predefined transfer scenario determining file size distribution.
    #[arg(short = 'S', long, default_value = "default")]
    scenario: Scenario,

    /// Number of parallel streams/connections to open.
    #[arg(short, long, default_value_t = 1)]
    parallel: u32,

    /// Total time to run the client loop in seconds.
    #[arg(short, long)]
    duration: Option<u64>,

    /// If set, a fresh connection is initiated for every transfer.
    #[arg(long)]
    connect_each: bool,

    /// URL of the metrics server for real-time dashboard reporting.
    /// Overrides 'server_url' in config.toml.
    #[arg(short, long)]
    server_url: Option<String>,

    /// Path to a TOML configuration file (default is 'config.toml').
    #[arg(short, long)]
    config: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Protocol {
    Quic,
    Http,
    Https,
    Tcp,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Scenario {
    /// 1GB transfers (default)
    Default,
    /// 10GB transfers
    Jumbo,
    /// 1MB transfers
    Small,
    /// 98.9% 1MB, 1% 1GB, 0.1% 10GB
    Mixture,
}

// #[derive(Subcommand)]
// enum Commands { ... } removed

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    let cli = Cli::parse();

    let config = AppConfig::load(cli.config.as_deref())?;
    let server_url = cli.server_url.unwrap_or(config.server_url);

    let reporter = ReportingClient::new(server_url);

    let mode_or_target = cli.mode_or_target.unwrap_or_else(|| {
        eprintln!("Usage: transfer-tool.exe [server|client] [TARGET]\n");
        eprintln!("Examples:");
        eprintln!("  transfer-tool.exe server                             # Start server on 0.0.0.0:4433");
        eprintln!("  transfer-tool.exe 192.168.1.10                       # Start client to 192.168.1.10:4433");
        eprintln!("  transfer-tool.exe client 192.168.1.10                # Explicit client to 192.168.1.10:4433");
        eprintln!("  transfer-tool.exe 192.168.1.10:8443 --proto https    # HTTPS client on custom port");
        std::process::exit(1);
    });

    if mode_or_target.eq_ignore_ascii_case("server") {
        match cli.proto {
            Protocol::Quic => run_quic_server(cli.listen).await?,
            Protocol::Http => run_http_server(cli.listen, false).await?,
            Protocol::Https => run_http_server(cli.listen, true).await?,
            Protocol::Tcp => run_tcp_server(cli.listen).await?,
        }
    } else {
        let raw_target = if mode_or_target.eq_ignore_ascii_case("client") {
            cli.explicit_target.unwrap_or_else(|| {
                eprintln!("Error: Target must be provided when using 'client' mode.");
                std::process::exit(1);
            })
        } else {
            mode_or_target
        };

        let target_str = if raw_target.contains(':') {
            raw_target
        } else {
            format!("{}:4433", raw_target)
        };

        let target: SocketAddr = target_str.parse().unwrap_or_else(|_| {
            eprintln!("Error: Invalid target address '{}'", target_str);
            std::process::exit(1);
        });

        let run_duration = cli.duration.unwrap_or(config.duration_secs);

        match cli.proto {
            Protocol::Quic => {
                run_quic_client(target, cli.size, cli.scenario, cli.parallel, run_duration, reporter).await?
            }
            Protocol::Http => {
                run_http_client(target, cli.size, cli.scenario, cli.parallel, run_duration, reporter, false, cli.connect_each).await?
            }
            Protocol::Https => {
                run_http_client(target, cli.size, cli.scenario, cli.parallel, run_duration, reporter, true, cli.connect_each).await?
            }
            Protocol::Tcp => {
                run_tcp_client(target, cli.size, cli.scenario, cli.parallel, run_duration, reporter, cli.connect_each).await?
            }
        }
    }

    Ok(())
}

async fn run_quic_server(listen: SocketAddr) -> anyhow::Result<()> {
    let (cert_der, key_der) = generate_self_signed_cert()?;
    let mut crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    crypto.alpn_protocols = vec![b"hq-29".to_vec()];

    let server_config = ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?,
    ));

    let endpoint = Endpoint::server(server_config, listen)?;
    info!("QUIC server listening on {}", listen);

    while let Some(conn) = endpoint.accept().await {
        tokio::spawn(async move {
            if let Ok(connection) = conn.await {
                info!("New connection from {}", connection.remote_address());
                while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                    tokio::spawn(async move {
                        let mut buf = [0u8; 8];
                        if recv.read_exact(&mut buf).await.is_ok() {
                            let size = u64::from_be_bytes(buf);
                            let data = vec![0u8; 1024 * 1024]; // 1MB chunk
                            let mut remaining = size;
                            while remaining > 0 {
                                let to_send = std::cmp::min(remaining, data.len() as u64);
                                if send.write_all(&data[..to_send as usize]).await.is_err() {
                                    break;
                                }
                                remaining -= to_send;
                            }
                            let _ = send.finish();
                        }
                    });
                }
            }
        });
    }

    Ok(())
}

async fn run_http_server(listen: SocketAddr, use_tls: bool) -> anyhow::Result<()> {
    let app = Router::new().route("/download/:size", get(|Path(size): Path<u64>| async move {
        let chunk = Arc::new(vec![0u8; 1024 * 1024]); // 1MB chunk
        let stream = async_stream::stream! {
            let mut remaining = size;
            while remaining > 0 {
                let to_send = std::cmp::min(remaining, chunk.len() as u64) as usize;
                yield Ok::<axum::body::Bytes, std::io::Error>(axum::body::Bytes::copy_from_slice(&chunk[..to_send]));
                remaining -= to_send as u64;
            }
        };
        axum::body::Body::from_stream(stream)
    }));

    let listener = tokio::net::TcpListener::bind(listen).await?;

    if use_tls {
        let (cert_der, key_der) = generate_self_signed_cert()?;
        let mut crypto = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)?;
        crypto.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(crypto));

        info!("HTTPS server listening on https://{}", listen);

        loop {
            let (stream, _remote_addr) = listener.accept().await?;
            let acceptor = acceptor.clone();
            let app = app.clone();

            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        error!("TLS accept error: {}", e);
                        return;
                    }
                };

                let hyper_service = hyper_util::service::TowerToHyperService::new(app);
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(hyper_util::rt::TokioIo::new(tls_stream), hyper_service)
                .await;
            });
        }
    } else {
        info!("HTTP server listening on http://{}", listen);
        axum::serve(listener, app).await?;
    }

    Ok(())
}

async fn run_quic_client(
    target: SocketAddr,
    size: Option<u64>,
    scenario: Scenario,
    parallel: u32,
    duration_secs: u64,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"hq-29".to_vec()];

    let client_config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    ));

    let endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;

    info!(
        "Connecting to QUIC server at {} (duration: {}s)",
        target, duration_secs
    );
    let connection = endpoint
        .connect_with(client_config, target, "localhost")?
        .await?;
    info!("Connected!");

    let start_time = Instant::now();
    let total_duration = Duration::from_secs(duration_secs);

    while start_time.elapsed() < total_duration {
        let batch_start = Instant::now();
        let mut futures = Vec::new();

        for _ in 0..parallel {
            let (mut send, mut recv) = connection.open_bi().await?;
            let sub_size = get_transfer_size(size, scenario) / parallel as u64;
            futures.push(tokio::spawn(async move {
                let _ = send.write_all(&sub_size.to_be_bytes()).await;
                let _ = send.finish();
                let mut downloaded = 0u64;
                let mut buf = [0u8; 16384];
                while let Ok(len) = recv.read(&mut buf).await {
                    match len {
                        Some(l) => downloaded += l as u64,
                        None => break,
                    }
                }
                downloaded
            }));
        }

        let mut batch_downloaded = 0u64;
        for f in futures {
            batch_downloaded += f.await?;
        }

        let batch_duration = batch_start.elapsed();
        let mbps = (batch_downloaded as f64 * 8.0) / (1_000_000.0 * batch_duration.as_secs_f64());
        info!(
            "QUIC Batch finished: downloaded {} bytes in {:.2?} ({:.2} Mbps)",
            batch_downloaded, batch_duration, mbps
        );

        let system_ip = common::get_local_ip(&target.ip().to_string());
        let metric = Metric::new("transfer-tool", "throughput")
            .with_value("download_mbps", mbps)
            .with_value("batch_bytes", batch_downloaded as f64)
            .with_tag("target", &target.to_string())
            .with_tag("proto", "quic")
            .with_tag("system_ip", &system_ip)
            .with_tag("target_ip", &target.ip().to_string());
        let _ = reporter.report(metric).await;
    }

    Ok(())
}

async fn run_http_client(
    target: SocketAddr,
    size: Option<u64>,
    scenario: Scenario,
    parallel: u32,
    duration_secs: u64,
    reporter: ReportingClient,
    use_tls: bool,
    connect_each: bool,
) -> anyhow::Result<()> {
    let proto_str = if use_tls { "https" } else { "http" };

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    info!(
        "Starting {} transfer to {} (scenario: {:?}, connect_each: {})",
        proto_str.to_uppercase(),
        target,
        scenario,
        connect_each
    );

    let start_time = Instant::now();
    let total_duration = Duration::from_secs(duration_secs);
    let persistent_client = client.clone();

    while start_time.elapsed() < total_duration {
        let batch_start = Instant::now();
        let mut futures = Vec::new();

        for _ in 0..parallel {
            let c = if connect_each {
                reqwest::Client::builder()
                    .danger_accept_invalid_certs(true)
                    .build()?
            } else {
                persistent_client.clone()
            };
            let sub_size = get_transfer_size(size, scenario) / parallel as u64;
            let u = format!("{}://{}/download/{}", proto_str, target, sub_size);
            futures.push(tokio::spawn(async move {
                let resp = c.get(u).send().await?;
                let mut downloaded = 0u64;
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk: reqwest::Result<axum::body::Bytes> = chunk;
                    let chunk = chunk?;
                    downloaded += chunk.len() as u64;
                }
                Ok::<u64, anyhow::Error>(downloaded)
            }));
        }

        let mut batch_downloaded = 0u64;
        for f in futures {
            batch_downloaded += f.await??;
        }

        let batch_duration = batch_start.elapsed();
        let mbps = (batch_downloaded as f64 * 8.0) / (1_000_000.0 * batch_duration.as_secs_f64());
        info!(
            "{} Batch finished: downloaded {} bytes in {:.2?} ({:.2} Mbps)",
            proto_str.to_uppercase(),
            batch_downloaded,
            batch_duration,
            mbps
        );

        let system_ip = common::get_local_ip(&target.ip().to_string());
        let metric = Metric::new("transfer-tool", "throughput")
            .with_value("download_mbps", mbps)
            .with_value("batch_bytes", batch_downloaded as f64)
            .with_tag("target", &target.to_string())
            .with_tag("proto", proto_str)
            .with_tag("system_ip", &system_ip)
            .with_tag("target_ip", &target.ip().to_string());
        let _ = reporter.report(metric).await;
    }

    Ok(())
}

fn generate_self_signed_cert() -> anyhow::Result<(
    rustls::pki_types::CertificateDer<'static>,
    rustls::pki_types::PrivateKeyDer<'static>,
)> {
    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let ck = rcgen::generate_simple_self_signed(subject_alt_names)?;
    let cert_der = rustls::pki_types::CertificateDer::from(ck.cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::Pkcs8(ck.key_pair.serialize_der().into());
    Ok((cert_der, key_der))
}

#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
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
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn run_tcp_server(listen: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!("TCP server listening on {}", listen);

    loop {
        let (mut socket, addr) = listener.accept().await?;
        info!("New TCP connection from {}", addr);
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 8];
            if socket.read_exact(&mut buf).await.is_ok() {
                let size = u64::from_be_bytes(buf);
                let chunk = vec![0u8; 65536]; // 64KB chunk
                let mut remaining = size;
                while remaining > 0 {
                    let to_send = std::cmp::min(remaining, chunk.len() as u64);
                    if socket.write_all(&chunk[..to_send as usize]).await.is_err() {
                        break;
                    }
                    remaining -= to_send;
                }
            }
        });
    }
}

async fn run_tcp_client(
    target: SocketAddr,
    size: Option<u64>,
    scenario: Scenario,
    parallel: u32,
    duration_secs: u64,
    reporter: ReportingClient,
    connect_each: bool,
) -> anyhow::Result<()> {
    info!(
        "Starting TCP transfer to {} (scenario: {:?}, connect_each: {})",
        target, scenario, connect_each
    );

    let start_time = Instant::now();
    let total_duration = Duration::from_secs(duration_secs);

    while start_time.elapsed() < total_duration {
        let batch_start = Instant::now();
        let mut futures = Vec::new();

        for _ in 0..parallel {
            let sub_size = get_transfer_size(size, scenario) / parallel as u64;
            futures.push(tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut socket = tokio::net::TcpStream::connect(target).await?;
                socket.write_all(&sub_size.to_be_bytes()).await?;
                let mut downloaded = 0u64;
                let mut buf = [0u8; 16384];
                while let Ok(len) = socket.read(&mut buf).await {
                    if len == 0 {
                        break;
                    }
                    downloaded += len as u64;
                }
                Ok::<u64, anyhow::Error>(downloaded)
            }));
        }

        let mut batch_downloaded = 0u64;
        for f in futures {
            batch_downloaded += f.await??;
        }

        let batch_duration = batch_start.elapsed();
        let mbps = (batch_downloaded as f64 * 8.0) / (1_000_000.0 * batch_duration.as_secs_f64());
        info!(
            "TCP Batch finished: downloaded {} bytes in {:.2?} ({:.2} Mbps)",
            batch_downloaded, batch_duration, mbps
        );

        let system_ip = common::get_local_ip(&target.ip().to_string());
        let metric = Metric::new("transfer-tool", "throughput")
            .with_value("download_mbps", mbps)
            .with_value("batch_bytes", batch_downloaded as f64)
            .with_tag("target", &target.to_string())
            .with_tag("proto", "tcp")
            .with_tag("system_ip", &system_ip)
            .with_tag("target_ip", &target.ip().to_string());
        let _ = reporter.report(metric).await;
    }

    Ok(())
}

fn get_transfer_size(override_size: Option<u64>, scenario: Scenario) -> u64 {
    if let Some(s) = override_size {
        return s;
    }

    match scenario {
        Scenario::Default => 1024 * 1024 * 1024,    // 1GB
        Scenario::Jumbo => 1024 * 1024 * 1024 * 10, // 10GB
        Scenario::Small => 1024 * 1024,             // 1MB
        Scenario::Mixture => {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            let val: f64 = rng.gen_range(0.0..100.0);
            if val < 0.5 {
                1024 * 1024 * 1024 * 10 // 10GB
            } else if val < 5.6 {
                1024 * 1024 * 1024 // 1GB
            } else {
                1024 * 1024 // 1MB
            }
        }
    }
}
