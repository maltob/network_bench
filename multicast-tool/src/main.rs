use clap::{Parser, Subcommand};
use common::{AppConfig, Metric, ReportingClient};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::time::interval;
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
    /// Send multicast packets
    Sender {
        #[arg(short, long)]
        group: SocketAddr,
        #[arg(short, long, default_value_t = 1000)]
        pps: u64,
        #[arg(short, long, default_value_t = 1024)]
        size: usize,
        #[arg(short, long)]
        duration: Option<u64>,
    },
    /// Receive multicast packets
    Receiver {
        #[arg(short, long)]
        group: SocketAddr,
        #[arg(short, long)]
        interface: Option<Ipv4Addr>,
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
        Commands::Sender {
            group,
            pps,
            size,
            duration,
        } => {
            let run_duration = duration.unwrap_or(config.duration_secs);
            run_sender(group, pps, size, run_duration, reporter).await?;
        }
        Commands::Receiver {
            group,
            interface,
            duration,
        } => {
            let run_duration = duration.unwrap_or(config.duration_secs);
            run_receiver(group, interface, run_duration, reporter).await?;
        }
    }

    Ok(())
}

async fn run_sender(
    group: SocketAddr,
    pps: u64,
    size: usize,
    duration_secs: u64,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.set_nonblocking(true)?;
    let socket = tokio::net::UdpSocket::from_std(socket)?;

    info!(
        "Starting multicast sender to {} at {} pps (duration: {}s)",
        group, pps, duration_secs
    );

    let mut interval = interval(Duration::from_micros(1_000_000 / pps));
    let data = vec![0u8; size];
    let start_time = Instant::now();
    let duration = Duration::from_secs(duration_secs);
    let mut last_report = Instant::now();
    let mut current_window_sent = 0u64;

    loop {
        if start_time.elapsed() >= duration {
            info!("Run duration reached, exiting.");
            break;
        }

        interval.tick().await;
        socket.send_to(&data, group).await?;
        current_window_sent += 1;

        if last_report.elapsed() >= Duration::from_secs(1) {
            let elapsed = last_report.elapsed().as_secs_f64();
            let pps_actual = current_window_sent as f64 / elapsed;
            let mbps = (current_window_sent * size as u64 * 8) as f64 / (1_000_000.0 * elapsed);

            info!("Sent: {:.2} PPS, {:.2} Mbps", pps_actual, mbps);

            let system_ip = common::get_local_ip(&group.ip().to_string());
            let metric = Metric::new("multicast-tool", "throughput")
                .with_value("pps", pps_actual)
                .with_value("mbps", mbps)
                .with_tag("mode", "sender")
                .with_tag("group", &group.to_string())
                .with_tag("system_ip", &system_ip)
                .with_tag("target_ip", &group.ip().to_string());

            let _ = reporter.report(metric).await;

            last_report = Instant::now();
            current_window_sent = 0;
        }
    }

    Ok(())
}

async fn run_receiver(
    group: SocketAddr,
    interface: Option<Ipv4Addr>,
    duration_secs: u64,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(not(windows))]
    socket.set_reuse_port(true)?;

    let bind_addr: SocketAddr = match group {
        SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), v4.port()),
        SocketAddr::V6(v6) => {
            SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), v6.port())
        }
    };

    socket.bind(&bind_addr.into())?;

    if let SocketAddr::V4(group_v4) = group {
        let interface_v4 = interface.unwrap_or(Ipv4Addr::UNSPECIFIED);
        socket.join_multicast_v4(group_v4.ip(), &interface_v4)?;
    }

    socket.set_nonblocking(true)?;
    let socket = tokio::net::UdpSocket::from_std(socket.into())?;

    info!(
        "Listening for multicast on {} (duration: {}s)",
        group, duration_secs
    );

    let mut buf = [0u8; 65535];
    let mut total_received = 0u64;
    let mut total_bytes = 0u64;
    let start_time = Instant::now();
    let duration = Duration::from_secs(duration_secs);
    let mut last_report = Instant::now();

    loop {
        if start_time.elapsed() >= duration {
            info!("Run duration reached, exiting.");
            break;
        }

        // Use a timeout to allow checking the duration even if no packets arrive
        match tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _))) => {
                total_received += 1;
                total_bytes += len as u64;
            }
            Ok(Err(e)) => {
                error!("Socket error: {}", e);
                break;
            }
            Err(_) => {
                // Timeout, just loop back and check duration
            }
        }

        if last_report.elapsed() >= Duration::from_secs(1) {
            let elapsed = last_report.elapsed().as_secs_f64();
            let pps = total_received as f64 / elapsed;
            let mbps = (total_bytes * 8) as f64 / (1_000_000.0 * elapsed);

            info!("Received: {:.2} PPS, {:.2} Mbps", pps, mbps);

            let system_ip = common::get_local_ip(&group.ip().to_string());
            let metric = Metric::new("multicast-tool", "throughput")
                .with_value("pps", pps)
                .with_value("mbps", mbps)
                .with_tag("mode", "receiver")
                .with_tag("group", &group.to_string())
                .with_tag("system_ip", &system_ip)
                .with_tag("target_ip", &group.ip().to_string());

            let _ = reporter.report(metric).await;

            total_received = 0;
            total_bytes = 0;
            last_report = Instant::now();
        }
    }

    Ok(())
}
