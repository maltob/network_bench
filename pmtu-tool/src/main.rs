use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::time::Duration;
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
    #[arg(short, long, default_value = "0.0.0.0:8002")]
    bind: String,

    /// Minimum payload size to test
    #[arg(long, default_value_t = 100)]
    min_size: usize,

    /// Maximum payload size to test
    #[arg(long, default_value_t = 9000)]
    max_size: usize,

    /// Step size to increase the payload by
    #[arg(long, default_value_t = 10)]
    step: usize,

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
        eprintln!("Usage: pmtu-tool.exe [server|client] [TARGET]\n");
        eprintln!("Examples:");
        eprintln!("  pmtu-tool.exe server                             # Start echo server on 0.0.0.0:8002");
        eprintln!("  pmtu-tool.exe 192.168.1.10                       # Sweep PMTU to 192.168.1.10:8002");
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
            format!("{}:8002", raw_target)
        };

        run_client(&target, cli.min_size, cli.max_size, cli.step, reporter).await?;
    }

    Ok(())
}

async fn run_server(bind: &str) -> anyhow::Result<()> {
    let socket = tokio::net::UdpSocket::bind(bind).await?;
    info!("PMTU Echo Server listening on {}", bind);
    
    let mut buf = vec![0u8; 65535];
    loop {
        match socket.recv_from(&mut buf).await {
            Ok((len, addr)) => {
                let _ = socket.send_to(&buf[..len], addr).await;
            }
            Err(e) => warn!("Recv error: {}", e),
        }
    }
}

async fn run_client(
    target: &str,
    min_size: usize,
    max_size: usize,
    step: usize,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    let target_addr: SocketAddr = target.parse()?;
    
    let domain = if target_addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let sock2 = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    
    // Attempt to set Don't Fragment bit (DF)
    if target_addr.is_ipv4() {
        // IP_DONTFRAG (Windows) / IP_MTU_DISCOVER (Linux via libc) is somewhat platform specific.
        // We'll use cross-platform where possible, but for true portability on sockets in Rust, 
        // socket2 provides basic support but sometimes requires OS level bindings.
        // As a simple approximation, socket2 doesn't expose a unified `set_dont_fragment`.
        // We will do our best with what socket2 offers or rely on the OS throwing Message Too Long errors.
        
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::io::AsRawSocket;
            use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IP_DONTFRAGMENT};
            let raw_sock = sock2.as_raw_socket() as usize;
            unsafe {
                let optval: i32 = 1;
                setsockopt(
                    raw_sock,
                    IPPROTO_IP as i32,
                    IP_DONTFRAGMENT as i32,
                    &optval as *const _ as *const _,
                    std::mem::size_of_val(&optval) as i32,
                );
            }
        }
        
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let raw_fd = sock2.as_raw_fd();
            unsafe {
                let optval: libc::c_int = libc::IP_PMTUDISC_DO; 
                libc::setsockopt(
                    raw_fd,
                    libc::IPPROTO_IP,
                    libc::IP_MTU_DISCOVER,
                    &optval as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&optval) as libc::socklen_t,
                );
            }
        }

        #[cfg(target_os = "macos")]
        {
            use std::os::unix::io::AsRawFd;
            let raw_fd = sock2.as_raw_fd();
            // macOS IP_DONTFRAG = 28 (not exposed by libc crate)
            const IP_DONTFRAG: libc::c_int = 28;
            unsafe {
                let optval: libc::c_int = 1; 
                libc::setsockopt(
                    raw_fd,
                    libc::IPPROTO_IP,
                    IP_DONTFRAG,
                    &optval as *const _ as *const libc::c_void,
                    std::mem::size_of_val(&optval) as libc::socklen_t,
                );
            }
        }
    }

    sock2.bind(&"0.0.0.0:0".parse::<SocketAddr>()?.into())?;
    sock2.set_nonblocking(true)?;
    let socket = tokio::net::UdpSocket::from_std(sock2.into())?;
    
    info!("Starting PMTU Sweep towards {} ({} to {} bytes, step {})", target, min_size, max_size, step);

    let mut current_size = min_size;
    let mut last_successful_size = 0;
    let mut recv_buf = vec![0u8; 65535];
    
    let system_ip = common::get_local_ip(&target_addr.ip().to_string());

    while current_size <= max_size {
        let payload = vec![0u8; current_size];
        
        // Send packet
        let send_res = socket.send_to(&payload, target_addr).await;
        match send_res {
            Ok(_) => {
                // Wait for echo to confirm it didn't get blackholed downstream
                let mut success = false;
                match tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut recv_buf)).await {
                    Ok(Ok((len, _))) => {
                        if len == current_size {
                            success = true;
                        }
                    }
                    _ => {}
                }

                if success {
                    last_successful_size = current_size;
                    info!("Size {} bytes: SUCCESS", current_size);
                } else {
                    info!("Size {} bytes: TIMEOUT (Possible Blackhole)", current_size);
                    break;
                }
            }
            // EMSGSIZE or WSAEMSGSIZE
            Err(e) if e.raw_os_error() == Some(90) || e.raw_os_error() == Some(10040) || e.to_string().contains("too long") => {
                info!("Size {} bytes: TOO LARGE (OS Error)", current_size);
                break;
            }
            Err(e) => {
                warn!("Size {} bytes: ERROR - {}", current_size, e);
                break;
            }
        }
        
        current_size += step;
    }

    // IP header (20 bytes) + UDP header (8 bytes) = 28 bytes overhead
    let presumed_pmtu = if last_successful_size > 0 { last_successful_size + 28 } else { 0 };
    info!("===================================");
    info!("Estimated Path MTU: {} bytes", presumed_pmtu);
    info!("===================================");

    let metric = Metric::new("pmtu-tool", "discovery")
        .with_value("pmtu_bytes", presumed_pmtu as f64)
        .with_tag("target", &target_addr.to_string())
        .with_tag("system_ip", &system_ip)
        .with_tag("target_ip", &target_addr.ip().to_string());
    
    let _ = reporter.report(metric).await;

    Ok(())
}
