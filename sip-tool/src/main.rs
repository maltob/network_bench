use clap::{Parser, Subcommand};
use common::{AppConfig, Metric, ReportingClient};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tracing::{error, info};

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
    /// Start a SIP echo server
    Server {
        #[arg(short, long, default_value = "0.0.0.0:5060")]
        bind: String,
    },
    /// Start a SIP latency client
    Client {
        #[arg(short, long)]
        target: String,
        #[arg(short, long)]
        count: Option<u64>,
        #[arg(short, long, default_value_t = 1000)]
        interval_ms: u64,
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
            run_server(&bind).await?;
        }
        Commands::Client {
            target,
            count,
            interval_ms,
            duration,
        } => {
            let run_duration = duration.unwrap_or(config.duration_secs);
            run_client(&target, count, interval_ms, run_duration, reporter).await?;
        }
    }

    Ok(())
}

async fn run_server(bind: &str) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(bind).await?;
    info!("SIP/RTP server listening on {}", bind);
    let mut buf = [0u8; 4096];

    // Simple session state
    let mut call_active = false;
    let mut remote_addr = None;

    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        let msg = String::from_utf8_lossy(&buf[..len]);

        if msg.starts_with("INVITE") {
            info!("Received INVITE from {}", addr);
            let response = "SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n";
            socket.send_to(response.as_bytes(), addr).await?;
            remote_addr = Some(addr);
        } else if msg.starts_with("ACK") {
            info!("Received ACK from {}, call established", addr);
            call_active = true;
        } else if msg.starts_with("BYE") {
            info!("Received BYE from {}, teardown", addr);
            let response = "SIP/2.0 200 OK\r\nContent-Length: 0\r\n\r\n";
            socket.send_to(response.as_bytes(), addr).await?;
            call_active = false;
        } else if call_active && remote_addr == Some(addr) {
            // Echo RTP-like packets
            socket.send_to(&buf[..len], addr).await?;
        } else if msg.starts_with("OPTIONS") {
            let response = "SIP/2.0 200 OK\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n";
            socket.send_to(response.as_bytes(), addr).await?;
        }
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
    info!("Starting SIP Call Simulation to {} ({}s)", target, duration_secs);

    let start_time = Instant::now();
    let total_duration = Duration::from_secs(duration_secs);

    let mut call_id = 0;
    while start_time.elapsed() < total_duration {
        if let Some(c) = count {
            if call_id >= c { break; }
        }

        // 1. INVITE
        let invite = format!("INVITE sip:{} SIP/2.0\r\nCall-ID: {}\r\nContent-Length: 0\r\n\r\n", target, call_id);
        let setup_start = Instant::now();
        socket.send_to(invite.as_bytes(), target).await?;

        let mut buf = [0u8; 4096];
        match tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _))) => {
                let resp = String::from_utf8_lossy(&buf[..len]);
                if resp.contains("200 OK") {
                    let setup_latency = setup_start.elapsed().as_secs_f64() * 1000.0;
                    info!("Call {} setup: {:.2}ms", call_id, setup_latency);

                    // 2. ACK
                    let ack = format!("ACK sip:{} SIP/2.0\r\nCall-ID: {}\r\n\r\n", target, call_id);
                    socket.send_to(ack.as_bytes(), target).await?;

                    // 3. Media Phase (5 seconds of RTP-like packets)
                    let media_duration = Duration::from_secs(5);
                    let media_start = Instant::now();
                    let mut media_interval = tokio::time::interval(Duration::from_millis(20));
                    let mut seq: u32 = 0;
                    
                    let mut last_recv_time = None;
                    let mut jitters = Vec::new();
                    let mut packets_sent = 0;
                    let mut packets_recv = 0;

                    while media_start.elapsed() < media_duration {
                        media_interval.tick().await;
                        
                        // Fake RTP packet: [Seq (4) | Timestamp (8) | Payload (148)] = 160 bytes
                        let mut pkt = [0u8; 160];
                        pkt[0..4].copy_from_slice(&seq.to_be_bytes());
                        
                        socket.send_to(&pkt, target).await?;
                        packets_sent += 1;

                        // Try to receive echo
                        // Use a non-blocking or short timeout recv
                        match tokio::time::timeout(Duration::from_millis(10), socket.recv_from(&mut buf)).await {
                            Ok(Ok((recv_len, _))) if recv_len == 160 => {
                                packets_recv += 1;
                                let now = Instant::now();
                                if let Some(prev) = last_recv_time {
                                    let delta = now.duration_since(prev).as_secs_f64() * 1000.0;
                                    jitters.push((delta - 20.0).abs());
                                }
                                last_recv_time = Some(now);
                            }
                            _ => {}
                        }
                        seq += 1;
                    }

                    let avg_jitter = if jitters.is_empty() { 0.0 } else { jitters.iter().sum::<f64>() / jitters.len() as f64 };
                    let loss_rate = if packets_sent == 0 { 0.0 } else { (packets_sent - packets_recv) as f64 / packets_sent as f64 * 100.0 };

                    // 4. BYE
                    let bye = format!("BYE sip:{} SIP/2.0\r\nCall-ID: {}\r\n\r\n", target, call_id);
                    socket.send_to(bye.as_bytes(), target).await?;
                    let _ = tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await;

                    // Report Metrics
                    let system_ip = common::get_local_ip(target);
                    let metric = Metric::new("sip-tool", "call_quality")
                        .with_value("setup_ms", setup_latency)
                        .with_value("jitter_ms", avg_jitter)
                        .with_value("loss_pct", loss_rate)
                        .with_tag("system_ip", &system_ip)
                        .with_tag("target", target);
                    let _ = reporter.report(metric).await;
                }
            }
            _ => error!("Call {} setup timeout", call_id),
        }

        call_id += 1;
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
    }

    Ok(())
}
