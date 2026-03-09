use clap::{Parser, Subcommand};
use common::{AppConfig, Metric, ReportingClient};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::time::Duration;
use std::sync::Arc;
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
    /// Start an MTU discovery server (echo)
    Server {
        #[arg(short, long, default_value = "0.0.0.0:9001")]
        bind: String,
    },
    /// Start an MTU discovery client
    Client {
        #[arg(short, long)]
        target: String,
        #[arg(short, long, default_value_t = 100)]
        retries: u32,
        /// Optional: Check performance impact of fragmentation after finding PMTU
        #[arg(long)]
        perf_check: bool,
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
        Commands::Client { target, retries, perf_check } => {
            run_client(&target, retries, perf_check, reporter).await?;
        }
    }

    Ok(())
}

async fn run_server(bind: &str) -> anyhow::Result<()> {
    let socket = UdpSocket::bind(bind).await?;
    info!("MTU Discovery server listening on {}", bind);
    let mut buf = [0u8; 9000];

    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        let resp = format!("OK:{}", len);
        socket.send_to(resp.as_bytes(), addr).await?;
    }
}

async fn run_client(target: &str, _retries: u32, perf_check: bool, reporter: ReportingClient) -> anyhow::Result<()> {
    let target_addr: SocketAddr = target.parse()?;
    
    info!("Starting MTU Discovery to {}", target);

    let mut min = 64;
    let mut max = 9000;
    let mut best = 64;

    while min <= max {
        let mid = (min + max) / 2;
        if probe_mtu(target_addr, mid).await {
            info!("Probe success at {} bytes", mid);
            best = mid;
            min = mid + 1;
        } else {
            info!("Probe failed at {} bytes", mid);
            max = mid - 1;
        }
    }

    info!("Discovered Path MTU: {} bytes", best);

    let metric = Metric::new("mtu-discovery", "path_mtu")
        .with_value("mtu_bytes", best as f64)
        .with_tag("target", target);
    
    let _ = reporter.report(metric).await;

    if perf_check && best > 64 {
        run_perf_check(target_addr, best, reporter).await?;
    }

    Ok(())
}

async fn run_perf_check(
    target: SocketAddr,
    pmtu_size: usize,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    info!("--- Fragmentation Performance Check ---");
    let unfrag_size = pmtu_size;
    let frag_size = pmtu_size + 500; // Force fragmentation

    let socket = Arc::new(tokio::net::UdpSocket::bind("0.0.0.0:0").await?);
    
    async fn ping_sweep(socket: &tokio::net::UdpSocket, target: SocketAddr, size: usize, count: usize) -> (usize, f64) {
        let mut recv_buf = vec![0u8; 65535];
        let mut success = 0;
        let mut total_rtt = 0.0;
        let payload = vec![0u8; size];

        for _ in 0..count {
            let start = std::time::Instant::now();
            if socket.send_to(&payload, target).await.is_ok() {
                if let Ok(Ok((len, _))) = tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut recv_buf)).await {
                    let resp = String::from_utf8_lossy(&recv_buf[..len]);
                    if resp.starts_with("OK:") {
                        success += 1;
                        total_rtt += start.elapsed().as_secs_f64() * 1000.0;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let avg_rtt = if success > 0 { total_rtt / success as f64 } else { 0.0 };
        (success, avg_rtt)
    }

    async fn throughput_sweep(socket: Arc<tokio::net::UdpSocket>, target: SocketAddr, size: usize) -> f64 {
        let payload = vec![0u8; size];
        let duration = Duration::from_secs(1);
        let start = std::time::Instant::now();
        
        let socket_tx = Arc::clone(&socket);
        
        let _tx_handle = tokio::spawn(async move {
            while start.elapsed() < duration {
                let _ = socket_tx.send_to(&payload, target).await;
                tokio::task::yield_now().await;
            }
        });

        let mut success = 0;
        let mut recv_buf = vec![0u8; 1024]; 
        while start.elapsed() < duration + Duration::from_millis(200) {
            match tokio::time::timeout(Duration::from_millis(50), socket.recv_from(&mut recv_buf)).await {
                Ok(Ok((len, _))) => {
                    let resp = String::from_utf8_lossy(&recv_buf[..len]);
                    if resp.starts_with("OK:") {
                        success += 1;
                    }
                }
                _ => {}
            }
        }
        
        let bytes_delivered = success * size; 
        (bytes_delivered as f64 * 8.0) / 1_000_000.0
    }

    let (unfrag_succ, unfrag_rtt) = ping_sweep(&socket, target, unfrag_size, 100).await;
    let unfrag_tput = throughput_sweep(Arc::clone(&socket), target, unfrag_size).await;
    info!("Unfragmented ({} bytes) -> Success: {}/100, Avg Latency: {:.2}ms, Tput: {:.2} Mbps", unfrag_size, unfrag_succ, unfrag_rtt, unfrag_tput);

    let (frag_succ, frag_rtt) = ping_sweep(&socket, target, frag_size, 100).await;
    let frag_tput = throughput_sweep(Arc::clone(&socket), target, frag_size).await;
    info!("Fragmented   ({} bytes) -> Success: {}/100, Avg Latency: {:.2}ms, Tput: {:.2} Mbps", frag_size, frag_succ, frag_rtt, frag_tput);

    let system_ip = common::get_local_ip(&target.ip().to_string());
    
    let metric = Metric::new("mtu-discovery", "fragmentation_perf")
        .with_value("unfrag_latency_ms", unfrag_rtt)
        .with_value("unfrag_success_pct", (unfrag_succ as f64 / 100.0) * 100.0)
        .with_value("unfrag_tput_mbps", unfrag_tput)
        .with_value("frag_latency_ms", frag_rtt)
        .with_value("frag_success_pct", (frag_succ as f64 / 100.0) * 100.0)
        .with_value("frag_tput_mbps", frag_tput)
        .with_tag("target", &target.to_string())
        .with_tag("system_ip", &system_ip)
        .with_tag("target_ip", &target.ip().to_string());
        
    let _ = reporter.report(metric).await;

    Ok(())
}

async fn probe_mtu(target: SocketAddr, size: usize) -> bool {
    let domain = if target.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP)).unwrap();
    
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use winapi::um::winsock2::{setsockopt, SOCKET_ERROR};
        // IP_DONTFRAGMENT = 14, IPPROTO_IP = 0 on Windows
        const IPPROTO_IP: i32 = 0;
        const IP_DONTFRAGMENT: i32 = 14;
        let handle = socket.as_raw_socket();
        let val: i32 = 1;
        unsafe {
            let res = setsockopt(handle as usize, IPPROTO_IP, IP_DONTFRAGMENT, &val as *const i32 as *const i8, 4);
            if res == SOCKET_ERROR {
                warn!("Failed to set IP_DONTFRAGMENT on Windows");
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = socket.as_raw_fd();
        let val: i32 = libc::IP_PMTUDISC_DO; // 2
        unsafe {
            let res = libc::setsockopt(fd, libc::IPPROTO_IP, libc::IP_MTU_DISCOVER, &val as *const i32 as *const libc::c_void, 4);
            if res != 0 {
                warn!("Failed to set IP_MTU_DISCOVER on Linux");
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::io::AsRawFd;
        let fd = socket.as_raw_fd();
        let val: i32 = 1;
        // macOS IP_DONTFRAG = 28 (not exposed by libc crate)
        const IP_DONTFRAG: i32 = 28;
        unsafe {
            let res = libc::setsockopt(fd, libc::IPPROTO_IP, IP_DONTFRAG, &val as *const i32 as *const libc::c_void, 4);
            if res != 0 {
                warn!("Failed to set IP_DONTFRAG on macOS");
            }
        }
    }

    let bind_addr = if target.is_ipv4() {
        SocketAddr::new("0.0.0.0".parse().unwrap(), 0)
    } else {
        SocketAddr::new("::".parse().unwrap(), 0)
    };
    
    let _ = socket.bind(&bind_addr.into());
    
    // Convert socket2::Socket to std::net::UdpSocket
    #[cfg(windows)]
    let std_socket: StdUdpSocket = unsafe {
        use std::os::windows::io::{FromRawSocket, IntoRawSocket};
        StdUdpSocket::from_raw_socket(socket.into_raw_socket())
    };
    #[cfg(unix)]
    let std_socket: StdUdpSocket = unsafe {
        use std::os::unix::io::{FromRawFd, IntoRawFd};
        StdUdpSocket::from_raw_fd(socket.into_raw_fd())
    };

    let socket = UdpSocket::from_std(std_socket).unwrap();

    let buf = vec![0u8; size];
    if let Err(e) = socket.send_to(&buf, target).await {
        warn!("Send failed for size {}: {}", size, e);
        return false;
    }

    let mut recv_buf = [0u8; 1024];
    match tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut recv_buf)).await {
        Ok(Ok((len, _))) => {
            let resp = String::from_utf8_lossy(&recv_buf[..len]);
            resp.starts_with("OK:")
        }
        _ => false,
    }
}
