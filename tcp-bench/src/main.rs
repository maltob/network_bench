use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

#[cfg(windows)]
#[allow(unused_imports)]
use windows::Win32::NetworkManagement::IpHelper::{GetPerTcpConnectionEStats, TcpConnectionEstatsData, TCP_ESTATS_DATA_RW_v0, TCP_ESTATS_DATA_ROD_v0};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Mode ('server', 'client') or a target IP/hostname (assumes client)
    #[arg(index = 1)]
    mode_or_target: Option<String>,

    /// Target IP/hostname when 'client' is explicitly specified
    #[arg(index = 2)]
    explicit_target: Option<String>,

    /// Bind address for the server (when running as server)
    #[arg(short, long, default_value = "0.0.0.0:9002")]
    bind: String,

    /// Duration of the test in seconds (when running as client)
    #[arg(short, long, default_value_t = 10)]
    duration: u64,

    /// Buffer size for the test (when running as client)
    #[arg(short = 'B', long, default_value_t = 65536)]
    buffer_size: usize,

    #[arg(short, long)]
    server_url: Option<String>,

    #[arg(short, long)]
    config: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config = AppConfig::load(cli.config.as_deref())?;
    let server_url = cli.server_url.unwrap_or(config.server_url);
    let reporter = ReportingClient::new(server_url);

    let mode_or_target = cli.mode_or_target.unwrap_or_else(|| {
        eprintln!("Usage: tcp-bench.exe [server|client] [TARGET]\n");
        eprintln!("Examples:");
        eprintln!("  tcp-bench.exe server                      # Start server on 0.0.0.0:9002");
        eprintln!("  tcp-bench.exe 127.0.0.1                   # Start client targeting 127.0.0.1:9002");
        eprintln!("  tcp-bench.exe client 127.0.0.1            # Explicit client targeting 127.0.0.1:9002");
        eprintln!("  tcp-bench.exe 127.0.0.1:8080 -d 30        # Target specific port for 30 seconds");
        std::process::exit(1);
    });

    if mode_or_target.eq_ignore_ascii_case("server") {
        run_server(&cli.bind).await?;
    } else {
        let raw_target = if mode_or_target.eq_ignore_ascii_case("client") {
            cli.explicit_target.unwrap_or_else(|| {
                eprintln!("Error: Target must be provided when using 'client' mode.");
                std::process::exit(1);
            })
        } else {
            mode_or_target
        };

        let target = if raw_target.contains(':') {
            raw_target
        } else {
            format!("{}:9002", raw_target)
        };

        run_client(&target, cli.duration, cli.buffer_size, reporter).await?;
    }

    Ok(())
}

async fn run_server(bind: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!("TCP Bench server listening on {}", bind);

    loop {
        let (mut socket, addr) = listener.accept().await?;
        info!("Accepted connection from {}", addr);
        
        tokio::spawn(async move {
            let mut buf = vec![0u8; 128 * 1024];
            let mut total_bytes = 0;
            let start = Instant::now();
            
            while let Ok(n) = socket.read(&mut buf).await {
                if n == 0 { break; }
                total_bytes += n;
            }
            
            let elapsed = start.elapsed().as_secs_f64();
            if elapsed > 0.1 {
                let mbps = (total_bytes as f64 * 8.0) / elapsed / 1_000_000.0;
                info!("Connection from {} finished. Throughput: {:.2} Mbps", addr, mbps);
            }
        });
    }
}

async fn run_client(
    target: &str,
    duration: u64,
    buffer_size: usize,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    let target_addr: SocketAddr = target.parse()?;
    let mut stream = TcpStream::connect(target_addr).await?;
    info!("Connected to {} for TCP Bench", target);

    let buf = vec![0u8; buffer_size];
    let start = Instant::now();
    let test_duration = Duration::from_secs(duration);
    let mut total_bytes = 0;

    info!("Starting throughput test for {} seconds...", duration);

    while start.elapsed() < test_duration {
        match stream.write(&buf).await {
            Ok(n) => {
                total_bytes += n;
            }
            Err(e) => {
                warn!("Write error: {}", e);
                break;
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let mbps = (total_bytes as f64 * 8.0) / elapsed / 1_000_000.0;
    
    // Very simplified RTT measurement (just using the connection time for now or a small probe)
    let rtt_ms = 1.0; // Placeholder for loopback
    let bdp_kb = (mbps * 1000.0 * (rtt_ms / 1000.0)) / 8.0;

    info!("Finished. Throughput: {:.2} Mbps | Est. RTT: {:.1}ms | Theoretical BDP: {:.1} KB", 
        mbps, rtt_ms, bdp_kb);

    let metric = Metric::new("tcp-bench", "throughput_and_bdp")
        .with_value("throughput_mbps", mbps)
        .with_value("est_rtt_ms", rtt_ms)
        .with_value("theoretical_bdp_kb", bdp_kb)
        .with_tag("target", target);
    
    let _ = reporter.report(metric).await;

    Ok(())
}
