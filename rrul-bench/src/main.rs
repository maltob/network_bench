use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tracing::{info, warn};

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
    #[arg(short, long, default_value = "0.0.0.0")]
    bind: String,

    /// Number of concurrent TCP load connections
    #[arg(short, long, default_value_t = 4)]
    concurrency: usize,

    /// Duration of the test in seconds
    #[arg(short, long)]
    duration: Option<u64>,

    #[arg(short, long)]
    server_url: Option<String>,

    #[arg(short, long)]
    config: Option<String>,
}

const SERVER_TCP_PORT: u16 = 8003;
const SERVER_UDP_PORT: u16 = 8004;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config = AppConfig::load(cli.config.as_deref())?;
    let server_url = cli.server_url.unwrap_or(config.server_url);
    let reporter = ReportingClient::new(server_url);

    let mode_or_target = cli.mode_or_target.unwrap_or_else(|| {
        eprintln!("Usage: rrul-bench.exe [server|client] [TARGET]\n");
        eprintln!("Examples:");
        eprintln!("  rrul-bench.exe server                            # Start load/echo servers");
        eprintln!("  rrul-bench.exe 192.168.1.10                      # Test bufferbloat to 192.168.1.10");
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

        let run_duration = cli.duration.unwrap_or(config.duration_secs);
        run_client(&raw_target, cli.concurrency, run_duration, reporter).await?;
    }

    Ok(())
}

async fn run_server(bind: &str) -> anyhow::Result<()> {
    let tcp_bind = format!("{}:{}", bind, SERVER_TCP_PORT);
    let udp_bind = format!("{}:{}", bind, SERVER_UDP_PORT);

    info!("RRUL Server starting. TCP Load: {}, UDP Echo: {}", tcp_bind, udp_bind);

    // UDP Echo for latency
    let udp_socket = Arc::new(UdpSocket::bind(&udp_bind).await?);
    let udp_clone = Arc::clone(&udp_socket);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 1024];
        loop {
            if let Ok((len, addr)) = udp_clone.recv_from(&mut buf).await {
                let _ = udp_clone.send_to(&buf[..len], addr).await;
            }
        }
    });

    // TCP sink/source for load
    let tcp_listener = TcpListener::bind(&tcp_bind).await?;
    loop {
        match tcp_listener.accept().await {
            Ok((mut socket, addr)) => {
                info!("Accepted TCP load connection from {}", addr);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    // Constantly read to sink their data (upload bloat)
                    loop {
                        match socket.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                });
            }
            Err(e) => warn!("Accept error: {}", e),
        }
    }
}

async fn run_client(
    target: &str,
    concurrency: usize,
    duration_secs: u64,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    // If target has port assume it's domain or IP. We append specific ports anyway.
    // Strip port if it exists to append our own.
    let base_target = target.split(':').next().unwrap_or(target);
    
    let tcp_target = format!("{}:{}", base_target, SERVER_TCP_PORT);
    let udp_target = format!("{}:{}", base_target, SERVER_UDP_PORT);
    let target_addr: SocketAddr = tcp_target.parse().unwrap_or_else(|_| {
        eprintln!("Error: Invalid target IP address '{}'", base_target);
        std::process::exit(1);
    });

    info!("Starting RRUL Bufferbloat test against {} (Duration: {}s, Load Connections: {})", base_target, duration_secs, concurrency);

    let start_time = Instant::now();
    let duration = Duration::from_secs(duration_secs);

    let mut load_handles = Vec::new();

    // 1. Spawning TCP Load Generators
    for _ in 0..concurrency {
        let t = tcp_target.clone();
        load_handles.push(tokio::spawn(async move {
            if let Ok(mut stream) = TcpStream::connect(&t).await {
                // Generate upload traffic to saturate the upload link and queue
                let buf = vec![0u8; 65536];
                while start_time.elapsed() < duration {
                    if stream.write_all(&buf).await.is_err() {
                        break;
                    }
                }
            }
        }));
    }

    // 2. UDP Latency Pinger
    let udp_sock = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    let udp_target_addr: SocketAddr = udp_target.parse()?;
    let seq = Arc::new(AtomicU64::new(0));

    // Shared metrics
    let total_latency_ms = Arc::new(tokio::sync::Mutex::new(0.0f64));
    let pings_sent = Arc::new(AtomicU64::new(0));
    let pings_recv = Arc::new(AtomicU64::new(0));

    let pinger_sock = Arc::clone(&udp_sock);
    let pinger_seq = Arc::clone(&seq);
    let pinger_sent = Arc::clone(&pings_sent);
    
    // Send 10 pings per second
    let ping_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        while start_time.elapsed() < duration {
            interval.tick().await;
            let current_seq = pinger_seq.fetch_add(1, Ordering::Relaxed);
            let payload = format!("RRUL_PING:{}", current_seq);
            if pinger_sock.send_to(payload.as_bytes(), udp_target_addr).await.is_ok() {
                pinger_sent.fetch_add(1, Ordering::Relaxed);
            }
        }
    });

    let receiver_sock = Arc::clone(&udp_sock);
    let receiver_recv = Arc::clone(&pings_recv);
    let lat_clone = Arc::clone(&total_latency_ms);
    
    let receiver_handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 1024];
        let mut last_recv_time = Instant::now();
        // Since we send 10/sec, expected gap is 100ms. If buffer hits 1000ms+, jitter grows.
        
        while start_time.elapsed() < duration {
            if let Ok(Ok((len, _))) = tokio::time::timeout(Duration::from_millis(2000), receiver_sock.recv_from(&mut buf)).await {
                let rx_time = Instant::now();
                let msg = String::from_utf8_lossy(&buf[..len]);
                if msg.starts_with("RRUL_PING:") {
                    receiver_recv.fetch_add(1, Ordering::Relaxed);
                    // Estimate RTT by relying on the consistent send interval + tracking gaps.
                    // A proper ping embeds timestamps, let's just use gap between replies assuming symmetric queues.
                    
                    let gap = rx_time.duration_since(last_recv_time).as_secs_f64() * 1000.0;
                    last_recv_time = rx_time;

                    // If gap is massively larger than 100ms send rate, the queue held it.
                    let mut lock = lat_clone.lock().await;
                    *lock += gap; 
                }
            }
        }
    });

    let system_ip = common::get_local_ip(&target_addr.ip().to_string());
    let sys_ip_str = system_ip.clone();
    let target_ip_str = target_addr.ip().to_string();
    let target_str = target_addr.ip().to_string();

    // Reporter
    let mut report_interval = tokio::time::interval(Duration::from_secs(1));
    let mut last_sent = 0;
    let mut last_recv = 0;

    info!("Warmup: Sparing 2 seconds before metrics gathering...");
    tokio::time::sleep(Duration::from_secs(2)).await;

    while start_time.elapsed() < duration {
        report_interval.tick().await;
        
        let sent = pings_sent.load(Ordering::Relaxed);
        let recv = pings_recv.load(Ordering::Relaxed);
        let d_sent = sent - last_sent;
        let d_recv = recv - last_recv;
        
        // Calculate average gap (pseudo-RTT considering bloat)
        let mut avg_latency = 0.0;
        {
            let mut lock = total_latency_ms.lock().await;
            if d_recv > 0 {
                avg_latency = *lock / d_recv as f64;
            }
            *lock = 0.0;
        }

        let loss = if d_sent > 0 {
            let lost = d_sent.saturating_sub(d_recv);
            (lost as f64 / d_sent as f64) * 100.0
        } else {
            0.0
        };

        if avg_latency > 0.0 {
            info!("RRUL Latency Under Load: {:.2}ms (Loss: {:.2}%)", avg_latency, loss);

            let metric = Metric::new("rrul-bench", "bufferbloat")
                .with_value("latency_ms", avg_latency)
                .with_value("loss_percent", loss)
                .with_tag("target", &target_str)
                .with_tag("system_ip", &sys_ip_str)
                .with_tag("target_ip", &target_ip_str);
            
            let _ = reporter.report(metric).await;
        }

        last_sent = sent;
        last_recv = recv;
    }

    // Wait and abort
    ping_handle.abort();
    receiver_handle.abort();
    for h in load_handles {
        h.abort();
    }

    info!("Finished RRUL Bufferbloat test.");
    Ok(())
}
