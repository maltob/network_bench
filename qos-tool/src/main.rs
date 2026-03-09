use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
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
    #[arg(short, long, default_value = "0.0.0.0:8000")]
    bind: String,

    /// Interval between packets in milliseconds (client mode)
    #[arg(short, long, default_value_t = 100)]
    interval_ms: u64,

    /// Target DSCP value to mark packets with (client mode), e.g. 46 for EF
    #[arg(long, default_value_t = 46)]
    dscp: u8,

    /// Duration of the test in seconds (client mode)
    #[arg(short, long)]
    duration: Option<u64>,

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
        eprintln!("Usage: qos-tool.exe [server|client] [TARGET]\n");
        eprintln!("Examples:");
        eprintln!("  qos-tool.exe server                             # Start server on 0.0.0.0:8000");
        eprintln!("  qos-tool.exe 192.168.1.10                       # Start client to target 192.168.1.10:8000 (default DSCP 46)");
        eprintln!("  qos-tool.exe 192.168.1.10:8000 --dscp 34        # Target specific port and DSCP marking");
        std::process::exit(1);
    });

    if mode_or_target.eq_ignore_ascii_case("server") {
        run_server(&cli.bind, reporter).await?;
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
            format!("{}:8000", raw_target)
        };

        let run_duration = cli.duration.unwrap_or(config.duration_secs);
        run_client(&target, cli.dscp, cli.interval_ms, run_duration).await?;
    }

    Ok(())
}

#[derive(Default)]
struct SessionStats {
    last_seq: u64,
    received_count: u64,
    lost_count: u64,
    out_of_order_count: u64,
    jitters: Vec<f64>,
    last_arrival: Option<Instant>,
    start_time: Option<Instant>,
}

async fn run_server(bind: &str, reporter: ReportingClient) -> anyhow::Result<()> {
    let socket = std::net::UdpSocket::bind(bind)?;
    socket.set_nonblocking(true)?;
    let socket = tokio::net::UdpSocket::from_std(socket)?;
    info!("QoS server listening on {}", bind);

    // Track stats per client address
    let mut sessions: HashMap<SocketAddr, SessionStats> = HashMap::new();
    let mut report_interval = tokio::time::interval(Duration::from_secs(5));
    let mut buf = [0u8; 1024];

    loop {
        tokio::select! {
            result = socket.recv_from(&mut buf) => {
                let (len, addr) = result?;
                if len < 10 { continue; } // Need at least sequence (8) + dscp (1)
                
                let seq = u64::from_be_bytes(buf[0..8].try_into()?);
                let _dscp = buf[8];
                
                let stats = sessions.entry(addr).or_insert_with(|| SessionStats {
                    start_time: Some(Instant::now()),
                    ..Default::default()
                });

                stats.received_count += 1;

                if stats.last_arrival.is_some() {
                    let now = Instant::now();
                    if let Some(prev_arrival) = stats.last_arrival {
                         let arrival_delta = now.duration_since(prev_arrival).as_secs_f64() * 1000.0;
                         stats.jitters.push(arrival_delta);
                    }
                }
                stats.last_arrival = Some(Instant::now());

                if seq > stats.last_seq + 1 {
                    stats.lost_count += seq - stats.last_seq - 1;
                } else if seq < stats.last_seq && seq > 0 {
                    stats.out_of_order_count += 1;
                }
                stats.last_seq = seq;

                // Log every ~5 seconds handled by report tick
            }
            _ = report_interval.tick() => {
                for (addr, stats) in sessions.iter_mut() {
                    if stats.received_count == 0 { continue; }

                    let loss_pct = if stats.received_count + stats.lost_count == 0 { 0.0 } else {
                        (stats.lost_count as f64 / (stats.received_count + stats.lost_count) as f64) * 100.0
                    };

                    let avg_jitter = if stats.jitters.is_empty() { 0.0 } else {
                        let avg = stats.jitters.iter().sum::<f64>() / stats.jitters.len() as f64;
                        let variance = stats.jitters.iter().map(|j| (j - avg).powi(2)).sum::<f64>() / stats.jitters.len() as f64;
                        variance.sqrt()
                    };

                    info!("QoS Stream [{}] - Loss: {:.2}%, Jitter: {:.2}ms, OOO: {}", 
                        addr, loss_pct, avg_jitter, stats.out_of_order_count);

                    let metric = Metric::new("qos-tool", "dscp_validation")
                        .with_value("loss_pct", loss_pct)
                        .with_value("jitter_ms", avg_jitter)
                        .with_value("received_count", stats.received_count as f64)
                        .with_tag("client_addr", &addr.to_string());
                    
                    let _ = reporter.report(metric).await;
                    
                    // Reset stats for next window
                    stats.received_count = 0;
                    stats.lost_count = 0;
                    stats.out_of_order_count = 0;
                    stats.jitters.clear();
                }
            }
        }
    }
}

async fn run_client(target: &str, dscp: u8, interval_ms: u64, duration_secs: u64) -> anyhow::Result<()> {
    let target_addr: SocketAddr = target.parse()?;
    
    // Create socket using socket2 to set IP_TOS
    let domain = if target_addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let sock2 = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    
    // IP_TOS takes the DSCP value shifted left by 2 bits.
    let tos = (dscp << 2) as u32;
    if target_addr.is_ipv4() {
        if let Err(e) = sock2.set_tos(tos) {
            warn!("Failed to set IPv4 TOS parameter: {}. QoS markings might not be applied correctly.", e);
        }
    } else {
        warn!("QoS markings (DSCP) are only supported for IPv4 targets in this version.");
    }

    sock2.bind(&"0.0.0.0:0".parse::<SocketAddr>()?.into())?;
    sock2.set_nonblocking(true)?;
    
    let socket = tokio::net::UdpSocket::from_std(sock2.into())?;

    info!("Starting QoS validation Client to {} with DSCP {} (TOS: {})", target, dscp, tos);

    let mut timer = tokio::time::interval(Duration::from_millis(interval_ms));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    
    let start_time = Instant::now();
    let total_duration = Duration::from_secs(duration_secs);
    let mut seq: u64 = 1;

    while start_time.elapsed() < total_duration {
        timer.tick().await;

        let mut buf = vec![0u8; 128];
        buf[0..8].copy_from_slice(&seq.to_be_bytes());
        buf[8] = dscp; // Embed intended DSCP tracking in packet
        
        if let Err(e) = socket.send_to(&buf, target_addr).await {
            warn!("Failed to send packet: {}", e);
        }
        seq += 1;
    }

    info!("Finished QoS sending {} packets", seq - 1);
    Ok(())
}
