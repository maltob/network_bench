use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{error, info};

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
    #[arg(short, long, default_value = "0.0.0.0:8080")]
    bind: String,

    /// Number of packets to send (when running as client)
    #[arg(short, long)]
    count: Option<u64>,

    /// Interval between packets in milliseconds (when running as client)
    #[arg(short, long, default_value_t = 1000)]
    interval_ms: u64,

    /// Duration of the test in seconds (when running as client)
    #[arg(short, long)]
    duration: Option<u64>,

    #[arg(short, long)]
    server_url: Option<String>,

    #[arg(short, long)]
    config: Option<String>,
}

// #[derive(Subcommand)]
// enum Commands { ... } removed

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config = AppConfig::load(cli.config.as_deref())?;
    let server_url = cli.server_url.unwrap_or(config.server_url);

    let reporter = ReportingClient::new(server_url);

    let mode_or_target = cli.mode_or_target.unwrap_or_else(|| {
        eprintln!("Usage: latency-tool.exe [server|client] [TARGET]\n");
        eprintln!("Examples:");
        eprintln!("  latency-tool.exe server                         # Start server on 0.0.0.0:8080");
        eprintln!("  latency-tool.exe 192.168.1.10                   # Start client targeting 192.168.1.10:8080");
        eprintln!("  latency-tool.exe client 192.168.1.10            # Explicit client targeting 192.168.1.10:8080");
        eprintln!("  latency-tool.exe 192.168.1.10:9000 -c 10        # Target specific port, send 10 packets");
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
            format!("{}:8080", raw_target)
        };

        let run_duration = cli.duration.unwrap_or(config.duration_secs);
        run_client(&target, cli.count, cli.interval_ms, run_duration, reporter).await?;
    }

    Ok(())
}

async fn run_server(bind: &str) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(bind).await?;
    info!("Latency echo server listening on {}", bind);
    let mut buf = [0u8; 1024];

    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        socket.send_to(&buf[..len], addr).await?;
    }
}

async fn run_client(
    target: &str,
    count: Option<u64>,
    interval_ms: u64,
    duration_secs: u64,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    info!(
        "Starting latency test to {} (duration: {}s, count: {:?})",
        target, duration_secs, count
    );

    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
    let mut buf = [0u8; 1024];
    let mut last_rtt_ms: Option<f64> = None;
    let start_time = Instant::now();
    let duration = Duration::from_secs(duration_secs);

    for i in 0.. {
        if let Some(c) = count {
            if i >= c {
                break;
            }
        } else if start_time.elapsed() >= duration {
            break;
        }

        interval.tick().await;

        let send_start = Instant::now();
        let payload = i.to_be_bytes();
        socket.send_to(&payload, target).await?;

        match tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await {
            Ok(Ok((_len, _addr))) => {
                let duration = send_start.elapsed();
                let rtt_ms = duration.as_secs_f64() * 1000.0;

                let jitter_ms = if let Some(last_rtt) = last_rtt_ms {
                    (rtt_ms - last_rtt).abs()
                } else {
                    0.0
                };
                last_rtt_ms = Some(rtt_ms);

                info!(
                    "RT[{}] to {}: {:.3} ms (jitter: {:.3} ms)",
                    i, target, rtt_ms, jitter_ms
                );

                let system_ip = common::get_local_ip(target);
                let metric = Metric::new("latency-tool", "latency")
                    .with_value("rtt_ms", rtt_ms)
                    .with_value("jitter_ms", jitter_ms)
                    .with_tag("target", target)
                    .with_tag("system_ip", &system_ip)
                    .with_tag("target_ip", target);

                if let Err(e) = reporter.report(metric).await {
                    error!("Failed to report metric: {}", e);
                }
            }
            Ok(Err(e)) => error!("Socket error: {}", e),
            Err(_) => {
                error!("Timeout for packet {}", i);
                last_rtt_ms = None; // Reset jitter on timeout
            }
        }
    }

    Ok(())
}
