use clap::{Parser, Subcommand};
use common::{AppConfig, Metric, ReportingClient};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{info, warn};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long)]
    server_url: Option<String>,

    #[arg(short, long)]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start a UDP benchmark server
    Server {
        #[arg(short, long, default_value = "0.0.0.0:9000")]
        bind: String,
    },
    /// Start a UDP benchmark client
    Client {
        #[arg(short, long)]
        target: String,
        #[arg(short, long, default_value = "10mbps")]
        rate: String,
        #[arg(short, long, default_value_t = 1400)]
        packet_size: usize,
        #[arg(short, long)]
        duration: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config = AppConfig::load(cli.config.as_deref())?;
    let server_url = cli.server_url.unwrap_or(config.server_url);
    let reporter = ReportingClient::new(server_url);

    match cli.command {
        Commands::Server { bind } => {
            run_server(&bind, reporter).await?;
        }
        Commands::Client {
            target,
            rate,
            packet_size,
            duration,
        } => {
            let run_duration = duration.unwrap_or(config.duration_secs);
            run_client(&target, &rate, packet_size, run_duration).await?;
        }
    }

    Ok(())
}

struct SessionStats {
    last_seq: u64,
    received_count: u64,
    lost_count: u64,
    out_of_order_count: u64,
    jitters: Vec<f64>,
    last_arrival: Option<Instant>,
    start_time: Instant,
    bytes_received: u64,
}

async fn run_server(bind: &str, reporter: ReportingClient) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(bind).await?;
    info!("UDP Bench server listening on {}", bind);
    let mut buf = [0u8; 9000];
    let mut sessions: HashMap<SocketAddr, SessionStats> = HashMap::new();

    let mut report_interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            result = socket.recv_from(&mut buf) => {
                let (len, addr) = result?;
                if len < 16 { continue; }

                let seq = u64::from_be_bytes(buf[0..8].try_into()?);
                let _timestamp = u64::from_be_bytes(buf[8..16].try_into()?);

                let stats = sessions.entry(addr).or_insert_with(|| SessionStats {
                    last_seq: 0,
                    received_count: 0,
                    lost_count: 0,
                    out_of_order_count: 0,
                    jitters: Vec::new(),
                    last_arrival: None,
                    start_time: Instant::now(),
                    bytes_received: 0,
                });

                stats.received_count += 1;
                stats.bytes_received += len as u64;

                if stats.last_arrival.is_some() {
                    let now = Instant::now();
                    // Jitter is the variance in arrival intervals. 
                    if let Some(prev_arrival) = stats.last_arrival {
                         let arrival_delta = now.duration_since(prev_arrival).as_secs_f64() * 1000.0;
                         stats.jitters.push(arrival_delta);
                    }
                }
                stats.last_arrival = Some(Instant::now());

                if seq > stats.last_seq + 1 {
                    stats.lost_count += seq - stats.last_seq - 1;
                } else if seq < stats.last_seq {
                    stats.out_of_order_count += 1;
                }
                stats.last_seq = seq;
            }
            _ = report_interval.tick() => {
                let now = Instant::now();
                for (addr, stats) in sessions.iter_mut() {
                    let duration = now.duration_since(stats.start_time).as_secs_f64();
                    if duration < 1.0 { continue; }

                    let throughput_mbps = (stats.bytes_received as f64 * 8.0) / (duration * 1_000_000.0);
                    let loss_pct = if stats.received_count + stats.lost_count == 0 { 0.0 } else {
                        (stats.lost_count as f64 / (stats.received_count + stats.lost_count) as f64) * 100.0
                    };

                    // Calculate jitter (standard deviation of arrival intervals)
                    let avg_jitter = if stats.jitters.is_empty() { 0.0 } else {
                        let avg = stats.jitters.iter().sum::<f64>() / stats.jitters.len() as f64;
                        let variance = stats.jitters.iter().map(|j| (j - avg).powi(2)).sum::<f64>() / stats.jitters.len() as f64;
                        variance.sqrt()
                    };

                    info!("Addr: {}, Throughput: {:.2} Mbps, Loss: {:.2}%, Jitter: {:.2}ms, OOO: {}", 
                        addr, throughput_mbps, loss_pct, avg_jitter, stats.out_of_order_count);

                    let metric = Metric::new("udp-bench", "throughput")
                        .with_value("throughput_mbps", throughput_mbps)
                        .with_value("loss_pct", loss_pct)
                        .with_value("jitter_ms", avg_jitter)
                        .with_value("ooo_count", stats.out_of_order_count as f64)
                        .with_tag("client_addr", &addr.to_string());
                    
                    let _ = reporter.report(metric).await;
                }
            }
        }
    }
}

async fn run_client(target: &str, rate_str: &str, packet_size: usize, duration_secs: u64) -> anyhow::Result<()> {
    let target_addr: SocketAddr = target.parse()?;
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    
    let rate_bps = parse_rate(rate_str)?;
    let pps = rate_bps / (packet_size as u64 * 8);
    let interval = Duration::from_secs_f64(1.0 / pps as f64);

    info!("Starting UDP Bench to {} at {} ({} PPS), size {} bytes", target, rate_str, pps, packet_size);

    let start_time = Instant::now();
    let total_duration = Duration::from_secs(duration_secs);
    let mut seq: u64 = 0;
    let mut timer = tokio::time::interval(interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

    while start_time.elapsed() < total_duration {
        timer.tick().await;

        let mut buf = vec![0u8; packet_size];
        buf[0..8].copy_from_slice(&seq.to_be_bytes());
        let now_ms = Instant::now().duration_since(start_time).as_millis() as u64;
        buf[8..16].copy_from_slice(&now_ms.to_be_bytes());

        if let Err(e) = socket.send_to(&buf, target_addr).await {
            warn!("Failed to send packet: {}", e);
        }
        seq += 1;
    }

    info!("Finished sending {} packets", seq);
    Ok(())
}

fn parse_rate(rate: &str) -> anyhow::Result<u64> {
    let rate = rate.to_lowercase();
    if rate.ends_with("kbps") {
        Ok(rate.trim_end_matches("kbps").parse::<u64>()? * 1_000)
    } else if rate.ends_with("mbps") {
        Ok(rate.trim_end_matches("mbps").parse::<u64>()? * 1_000_000)
    } else if rate.ends_with("gbps") {
        Ok(rate.trim_end_matches("gbps").parse::<u64>()? * 1_000_000_000)
    } else {
        Ok(rate.parse::<u64>()?)
    }
}
