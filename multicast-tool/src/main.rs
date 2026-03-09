use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Mode ('sender', 'receiver') or a multicast group IP (assumes receiver)
    #[arg(index = 1)]
    mode_or_group: Option<String>,

    /// Multicast group IP when 'sender' or 'receiver' is explicitly specified
    #[arg(index = 2)]
    explicit_group: Option<String>,

    /// Packets per second (when running as sender)
    #[arg(short, long, default_value_t = 1000)]
    pps: u64,

    /// Size of UDP payload in bytes (when running as sender)
    #[arg(short, long, default_value_t = 1024)]
    size: usize,

    /// Interface IP to bind to (when running as receiver)
    #[arg(short, long)]
    interface: Option<Ipv4Addr>,

    /// Duration of the test in seconds
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

    let mode_or_group = cli.mode_or_group.unwrap_or_else(|| {
        eprintln!("Usage: multicast-tool.exe [sender|receiver] [GROUP]\n");
        eprintln!("Examples:");
        eprintln!("  multicast-tool.exe 224.0.0.251                          # Start receiver for 224.0.0.251:9000");
        eprintln!("  multicast-tool.exe receiver 224.0.0.251                 # Explicit receiver");
        eprintln!("  multicast-tool.exe sender 224.0.0.251                   # Start sender to 224.0.0.251:9000");
        eprintln!("  multicast-tool.exe sender 224.0.0.251:8000 --pps 500    # Sender on custom port and rate");
        std::process::exit(1);
    });

    let (is_sender, raw_group) = if mode_or_group.eq_ignore_ascii_case("sender") {
        (true, cli.explicit_group.unwrap_or_else(|| {
            eprintln!("Error: Multicast group must be provided when using 'sender' mode.");
            std::process::exit(1);
        }))
    } else if mode_or_group.eq_ignore_ascii_case("receiver") {
        (false, cli.explicit_group.unwrap_or_else(|| {
            eprintln!("Error: Multicast group must be provided when using 'receiver' mode.");
            std::process::exit(1);
        }))
    } else {
        // Default to receiver if just an IP is provided
        (false, mode_or_group)
    };

    let group_str = if raw_group.contains(':') {
        raw_group
    } else {
        format!("{}:9000", raw_group)
    };

    let group: SocketAddr = group_str.parse().unwrap_or_else(|_| {
        eprintln!("Error: Invalid multicast group address '{}'", group_str);
        std::process::exit(1);
    });

    let run_duration = cli.duration.unwrap_or(config.duration_secs);

    if is_sender {
        run_sender(group, cli.pps, cli.size, run_duration, reporter).await?;
    } else {
        run_receiver(group, cli.interface, run_duration, reporter).await?;
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
    // set_reuse_address is generally sufficient for multicast on Windows, Linux, and macOS 
    // to allow multiple listeners to bind to the same port.

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
