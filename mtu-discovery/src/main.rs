use clap::{Parser, Subcommand};
use common::{AppConfig, Metric, ReportingClient};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::time::Duration;
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
        Commands::Client { target, retries } => {
            run_client(&target, retries, reporter).await?;
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

async fn run_client(target: &str, _retries: u32, reporter: ReportingClient) -> anyhow::Result<()> {
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

    #[cfg(unix)]
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
