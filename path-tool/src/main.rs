#![allow(dead_code)]
use anyhow;
use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use crossterm::{
    cursor::{Hide, MoveTo},
    execute,
    terminal::{Clear, ClearType},
};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io::stdout;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::warn;
use trust_dns_resolver::TokioAsyncResolver;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long)]
    target: String,

    #[arg(short, long, default_value_t = 30)]
    max_hops: u8,

    #[arg(short, long, default_value_t = 1)]
    interval_secs: u64,

    #[arg(short, long)]
    server_url: Option<String>,

    #[arg(short, long)]
    config: Option<String>,

    /// Explicitly bind to a local IP address
    #[arg(short, long)]
    bind: Option<IpAddr>,
}

#[derive(Debug, Clone, PartialEq)]
struct ResolvedInfo {
    hostname: Option<String>,
    asn: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum ResolveStatus {
    Pending,
    Resolved(ResolvedInfo),
    Failed,
}

#[derive(Debug, Clone)]
struct HopStats {
    addr: Option<IpAddr>,
    sent: u32,
    received: u32,
    last_rtt: Option<Duration>,
    avg_rtt_sum: Duration,
    info: Option<ResolvedInfo>,
    last_updated: Instant,
}

struct Probe {
    ttl: u8,
    sent_at: Instant,
}

lazy_static::lazy_static! {
    static ref RESOLVE_CACHE: Arc<Mutex<HashMap<IpAddr, ResolveStatus>>> = Arc::new(Mutex::new(HashMap::new()));
}

const PROBE_ID: u16 = 0xABCD;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config = AppConfig::load(cli.config.as_deref())?;
    let server_url = cli.server_url.clone().unwrap_or(config.server_url);
    let reporter = ReportingClient::new(server_url);

    run_path_monitor(cli, reporter).await?;

    Ok(())
}

// Global handles to async resolvers, initialized once in main.
static SYSTEM_RESOLVER: tokio::sync::OnceCell<Option<TokioAsyncResolver>> = tokio::sync::OnceCell::const_new();
static PUBLIC_RESOLVER: tokio::sync::OnceCell<Option<TokioAsyncResolver>> = tokio::sync::OnceCell::const_new();

async fn run_path_monitor(cli: Cli, reporter: ReportingClient) -> anyhow::Result<()> {
    let target_addr: IpAddr = cli.target.parse()?;
    
    // Initialize system resolver (for local hostnames) with a strict timeout
    let _ = SYSTEM_RESOLVER.get_or_init(|| async {
        // Some Windows systems hang reading system conf
        match tokio::time::timeout(Duration::from_millis(1500), async {
            TokioAsyncResolver::tokio_from_system_conf()
        }).await {
            Ok(Ok(resolver)) => Some(resolver),
            _ => {
                warn!("System DNS config timed out or failed, using public fallback");
                None
            }
        }
    }).await;

    // Initialize public resolver (for ASN/WHOIS data)
    let _ = PUBLIC_RESOLVER.get_or_init(|| async {
        use trust_dns_resolver::config::*;
        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_millis(1500);
        opts.attempts = 1;
        Some(TokioAsyncResolver::tokio(ResolverConfig::cloudflare(), opts))
    }).await;

    let stats: Arc<Mutex<HashMap<u8, HopStats>>> = Arc::new(Mutex::new(HashMap::new()));
    
    // UI Setup
    let _ = execute!(stdout(), Hide, Clear(ClearType::All), MoveTo(0, 0));
    println!("Path Analysis to {} ({}) | Protocol: ICMP | Max Hops: {}", cli.target, target_addr, cli.max_hops);
    println!("{}", "=".repeat(95));

    #[cfg(windows)]
    {
        if target_addr.is_ipv4() {
            return run_windows_native_icmp(cli, reporter, stats, target_addr).await;
        } else {
            anyhow::bail!("IPv6 is not yet supported in native Windows ICMP mode");
        }
    }

    #[cfg(not(windows))]
    run_standard_monitor(cli, reporter, stats, target_addr).await
}

#[cfg(windows)]
async fn run_windows_native_icmp(
    cli: Cli,
    reporter: ReportingClient,
    stats: Arc<Mutex<HashMap<u8, HopStats>>>,
    target_addr: IpAddr,
) -> anyhow::Result<()> {
    use win_icmp::WinIcmp;
    let mut ticker = interval(Duration::from_secs(cli.interval_secs));

    loop {
        ticker.tick().await;
        let icmp = WinIcmp::new();
        for ttl in 1..=cli.max_hops {
            {
                let mut map = stats.lock().unwrap();
                map.entry(ttl).or_insert(HopStats {
                    addr: None, sent: 0, received: 0, last_rtt: None, avg_rtt_sum: Duration::ZERO, info: None, last_updated: Instant::now(),
                }).sent += 1;
            }

            if let Some((addr, rtt)) = icmp.send_echo(target_addr, ttl, 1000) {
                handle_hop_res(ttl, addr, rtt, &stats);
                if addr == target_addr { break; }
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
            // Report after every hop to keep the UI snappy
            report_stats(Arc::clone(&stats), &reporter, &target_addr.to_string()).await;
        }
    }
}

fn handle_hop_res(ttl: u8, addr: IpAddr, rtt: Duration, stats: &Arc<Mutex<HashMap<u8, HopStats>>>) {
    let mut map = stats.lock().unwrap();
    if let Some(entry) = map.get_mut(&ttl) {
        entry.addr = Some(addr);
        entry.received += 1;
        entry.last_rtt = Some(rtt);
        entry.avg_rtt_sum += rtt;
        entry.last_updated = Instant::now();
        
        // Resolve info check
        let mut spawn_res = false;
        {
            let mut cache = RESOLVE_CACHE.lock().unwrap();
            match cache.get(&addr) {
                Some(ResolveStatus::Resolved(info)) => {
                    entry.info = Some(info.clone());
                }
                Some(ResolveStatus::Pending) | Some(ResolveStatus::Failed) => {}
                _ => {
                    cache.insert(addr, ResolveStatus::Pending);
                    spawn_res = true;
                }
            }
        }

        if spawn_res {
            let stats_c = Arc::clone(stats);
            tokio::spawn(async move {
                if let Some(info) = resolve_ip(addr).await {
                    // Lock order: ALWAYS Update Cache, then Update Map (avoiding deadlock)
                    {
                        let mut cache = RESOLVE_CACHE.lock().unwrap();
                        cache.insert(addr, ResolveStatus::Resolved(info.clone()));
                    }
                    if let Ok(mut map) = stats_c.lock() {
                        if let Some(e) = map.get_mut(&ttl) {
                            if e.addr == Some(addr) {
                                e.info = Some(info);
                            }
                        }
                    }
                } else {
                    let mut cache = RESOLVE_CACHE.lock().unwrap();
                    cache.insert(addr, ResolveStatus::Failed);
                }
            });
        }
    }
}

async fn resolve_ip(ip: IpAddr) -> Option<ResolvedInfo> {
    let sys = SYSTEM_RESOLVER.get().and_then(|o| o.as_ref());
    let pub_res = PUBLIC_RESOLVER.get().and_then(|o| o.as_ref());
    
    // 1. Try reverse lookup for hostname
    let hostname = if let Some(resolver) = sys {
        match tokio::time::timeout(Duration::from_millis(1500), resolver.reverse_lookup(ip)).await {
            Ok(Ok(lookup)) => lookup.iter().next().map(|n| n.to_string().trim_end_matches('.').to_string()),
            _ => None,
        }
    } else if let Some(resolver) = pub_res {
        match tokio::time::timeout(Duration::from_millis(1500), resolver.reverse_lookup(ip)).await {
            Ok(Ok(lookup)) => lookup.iter().next().map(|n| n.to_string().trim_end_matches('.').to_string()),
            _ => None,
        }
    } else {
        None
    };

    // 2. Try ASN lookup (skip for private IPs)
    let asn = if is_private_ip(ip) { 
        None 
    } else if let Some(resolver) = pub_res {
        tokio::time::timeout(Duration::from_millis(2000), lookup_asn_dns(resolver, ip)).await.ok().flatten()
    } else { 
        None 
    };

    Some(ResolvedInfo { hostname, asn })
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        _ => false,
    }
}

async fn lookup_asn_dns(resolver: &trust_dns_resolver::TokioAsyncResolver, ip: IpAddr) -> Option<String> {
    if let IpAddr::V4(v4) = ip {
        let octets = v4.octets();
        let query = format!("{}.{}.{}.{}.origin.asn.cymru.com.", octets[3], octets[2], octets[1], octets[0]);
        if let Ok(lookup) = resolver.txt_lookup(query).await {
            for txt in lookup.iter() {
                for data in txt.txt_data() {
                    if let Ok(s) = String::from_utf8(data.to_vec()) {
                        let parts: Vec<&str> = s.split('|').collect();
                        if !parts.is_empty() {
                            let as_num = parts[0].trim();
                            let name_query = format!("AS{}.asn.cymru.com.", as_num);
                            if let Ok(name_lookup) = resolver.txt_lookup(name_query).await {
                                for ntxt in name_lookup.iter() {
                                    for ndata in ntxt.txt_data() {
                                        if let Ok(ns) = String::from_utf8(ndata.to_vec()) {
                                            let nparts: Vec<&str> = ns.split('|').collect();
                                            if nparts.len() >= 5 {
                                                return Some(format!("AS{} ({})", as_num, nparts[4].trim()));
                                            }
                                        }
                                    }
                                }
                            }
                            return Some(format!("AS{}", as_num));
                        }
                    }
                }
            }
        }
    }
    None
}

async fn run_standard_monitor(
    cli: Cli,
    reporter: ReportingClient,
    stats: Arc<Mutex<HashMap<u8, HopStats>>>,
    target_addr: IpAddr,
) -> anyhow::Result<()> {
    let local_ip: IpAddr = if let Some(bind_addr) = cli.bind {
        bind_addr
    } else {
        let local_ip_str = common::get_local_ip(&target_addr.to_string());
        local_ip_str.parse().unwrap_or(if target_addr.is_ipv4() {
            IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
        })
    };

    let domain = if target_addr.is_ipv4() { Domain::IPV4 } else { Domain::IPV6 };
    let proto = if target_addr.is_ipv4() { Protocol::ICMPV4 } else { Protocol::ICMPV6 };
    
    let socket = Socket::new(domain, Type::RAW, Some(proto))?;
    let _ = socket.bind(&SocketAddr::new(local_ip, 0).into());
    socket.set_nonblocking(true)?;
    let socket = Arc::new(tokio::net::UdpSocket::from_std(socket.into())?);

    let inflight: Arc<Mutex<HashMap<u16, Probe>>> = Arc::new(Mutex::new(HashMap::new()));
    let stats_c = Arc::clone(&stats);
    let inflight_c = Arc::clone(&inflight);
    let target_addr_c = target_addr;
    let socket_rcv = Arc::clone(&socket);

    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            if let Ok((len, addr)) = socket_rcv.recv_from(&mut buf).await {
                let now = Instant::now();
                let buf_ref = &buf[..len];
                let responder_addr = match addr {
                    SocketAddr::V4(v4) => IpAddr::V4(*v4.ip()),
                    SocketAddr::V6(v6) => IpAddr::V6(*v6.ip()),
                };

                let ip_header_len = if (buf_ref[0] >> 4) == 4 { (buf_ref[0] & 0x0F) as usize * 4 } else { 0 };
                if len < ip_header_len + 8 { continue; }
                let icmp_data = &buf_ref[ip_header_len..];
                let icmp_type = icmp_data[0];

                if (target_addr_c.is_ipv4() && icmp_type == 0) || (target_addr_c.is_ipv6() && icmp_type == 129) {
                    let id = u16::from_be_bytes([icmp_data[4], icmp_data[5]]);
                    if id == PROBE_ID {
                        let seq = u16::from_be_bytes([icmp_data[6], icmp_data[7]]);
                        handle_response(seq, responder_addr, now, &stats_c, &inflight_c);
                    }
                } else if (target_addr_c.is_ipv4() && icmp_type == 11) || (target_addr_c.is_ipv6() && icmp_type == 3) {
                    if icmp_data.len() >= 8 + 20 + 8 {
                        let inner_ip_len = (icmp_data[8] & 0x0F) as usize * 4;
                        let inner_icmp = &icmp_data[8+inner_ip_len..];
                        if inner_icmp.len() >= 8 {
                            let id = u16::from_be_bytes([inner_icmp[4], inner_icmp[5]]);
                            if id == PROBE_ID {
                                let seq = u16::from_be_bytes([inner_icmp[6], inner_icmp[7]]);
                                handle_response(seq, responder_addr, now, &stats_c, &inflight_c);
                            }
                        }
                    }
                }
            }
        }
    });

    let mut ticker = interval(Duration::from_secs(cli.interval_secs));
    let mut sequence: u16 = 0;

    loop {
        ticker.tick().await;
        for ttl in 1..=cli.max_hops {
            sequence = sequence.wrapping_add(1);
            let probe = Probe { ttl, sent_at: Instant::now() };
            inflight.lock().unwrap().insert(sequence, probe);
            
            stats.lock().unwrap().entry(ttl).or_insert(HopStats {
                addr: None, sent: 0, received: 0, last_rtt: None, avg_rtt_sum: Duration::ZERO, info: None, last_updated: Instant::now(),
            }).sent += 1;

            let _ = send_probe(&socket, target_addr, ttl, sequence).await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        report_stats(Arc::clone(&stats), &reporter, &target_addr.to_string()).await;
    }
}

async fn send_probe(socket: &tokio::net::UdpSocket, target: IpAddr, ttl: u8, seq: u16) -> anyhow::Result<()> {
    set_ttl(socket, ttl)?;
    let mut packet = [0u8; 8];
    packet[0] = if target.is_ipv4() { 8 } else { 128 };
    packet[4] = (PROBE_ID >> 8) as u8; packet[5] = (PROBE_ID & 0xFF) as u8;
    packet[6] = (seq >> 8) as u8; packet[7] = (seq & 0xFF) as u8;
    if target.is_ipv4() {
        let mut sum = 0u32;
        for i in (0..8).step_by(2) { sum += u16::from_be_bytes([packet[i], packet[i+1]]) as u32; }
        while sum > 0xFFFF { sum = (sum & 0xFFFF) + (sum >> 16); }
        let cksum = !sum as u16;
        packet[2] = (cksum >> 8) as u8; packet[3] = (cksum & 0xFF) as u8;
    }
    socket.send_to(&packet, SocketAddr::new(target, 0)).await?;
    Ok(())
}

fn set_ttl(socket: &tokio::net::UdpSocket, ttl: u8) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_IP, IP_TTL};
        let handle = socket.as_raw_socket();
        let val = ttl as i32;
        unsafe { setsockopt(handle as usize, IPPROTO_IP as _, IP_TTL as _, &val as *const i32 as *const _, 4); }
    }
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        unsafe { libc::setsockopt(socket.as_raw_fd(), libc::IPPROTO_IP, libc::IP_TTL, &ttl as *const u8 as *const libc::c_void, 1); }
    }
    Ok(())
}

fn handle_response(seq: u16, addr: IpAddr, now: Instant, stats: &Arc<Mutex<HashMap<u8, HopStats>>>, inflight: &Arc<Mutex<HashMap<u16, Probe>>>) {
    if let Some(probe) = inflight.lock().unwrap().remove(&seq) {
        handle_hop_res(probe.ttl, addr, now.duration_since(probe.sent_at), stats);
    }
}

async fn report_stats(stats: Arc<Mutex<HashMap<u8, HopStats>>>, reporter: &ReportingClient, target: &str) {
    let mut hops_data: Vec<(u8, HopStats)> = Vec::new();
    {
        let map = stats.lock().unwrap();
        for (&ttl, s) in map.iter() {
            hops_data.push((ttl, s.clone()));
        }
    }
    hops_data.sort_by_key(|(ttl, _)| *ttl);

    // Pre-calculate info strings and max width
    let mut info_strings = Vec::with_capacity(hops_data.len());
    let mut max_info_w = 30; // Min width for header "Hostname | ASN Info"

    for (_, s) in &hops_data {
        let info_str = if let Some(info) = &s.info {
            let h = info.hostname.as_deref().unwrap_or("");
            let a = info.asn.as_deref().unwrap_or("");
            if !h.is_empty() && !a.is_empty() {
                format!("{} ({})", h, a)
            } else if !h.is_empty() {
                h.to_string()
            } else if !a.is_empty() {
                a.to_string()
            } else {
                String::new()
            }
        } else if let Some(ip) = s.addr {
            let cache = RESOLVE_CACHE.lock().unwrap();
            match cache.get(&ip) {
                Some(ResolveStatus::Pending) => "resolving...".to_string(),
                _ => String::new(),
            }
        } else {
            String::new()
        };
        
        if info_str.len() > max_info_w {
            max_info_w = info_str.len();
        }
        info_strings.push(info_str);
    }
    
    let mut out = stdout();
    let _ = execute!(out, MoveTo(0, 2));
    
    // Total width components: 4 (Hop) + 15 (IP) + width (Info) + 10 (RTT) + 6 (Loss) + 10 (Sent/Recv) + 5 spaces
    let line_w = 4 + 15 + max_info_w + 10 + 6 + 10 + 5;
    
    println!("{:<4} {:<15} {:<width$} {:<10} {:<6} {:<8}", "Hop", "IP Address", "Hostname | ASN Info", "RTT", "Loss%", "Sent/Recv", width = max_info_w);
    println!("{}", "-".repeat(line_w));

    for (i, (ttl, s)) in hops_data.into_iter().enumerate() {
        let loss = 100.0 * (1.0 - (s.received as f64 / s.sent as f64));
        let avg_rtt = if s.received > 0 { s.avg_rtt_sum.as_millis() as f64 / s.received as f64 } else { 0.0 };
        
        let addr_str = s.addr.map(|a| a.to_string()).unwrap_or_else(|| "* * *".to_string());
        let info_str = &info_strings[i];
        let rtt_str = if let Some(rtt) = s.last_rtt { format!("{:.1}ms", rtt.as_secs_f64() * 1000.0) } else { "-".to_string() };
        
        println!("{:<4} {:<15} {:<width$} {:<10} {:<6.1} {}/{}", ttl, addr_str, info_str, rtt_str, loss, s.sent, s.received, width = max_info_w);

        // Report to dashboard periodically
        if s.addr.is_some() && Instant::now().duration_since(s.last_updated) < Duration::from_secs(2) {
            let metric = Metric::new("path-tool", "hop_stats")
                .with_value("loss_percent", loss).with_value("avg_rtt_ms", avg_rtt)
                .with_tag("hop_index", &ttl.to_string()).with_tag("hop_addr", &addr_str).with_tag("target", target);
            if let Some(info) = &s.info {
                let m = metric.with_tag("hostname", info.hostname.as_deref().unwrap_or("")).with_tag("asn", info.asn.as_deref().unwrap_or(""));
                let _ = reporter.report(m).await;
            } else {
                let _ = reporter.report(metric).await;
            }
        }
    }
}

#[cfg(windows)]
mod win_icmp {
    use std::net::IpAddr;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::NetworkManagement::IpHelper::{IcmpCreateFile, IcmpCloseHandle, IcmpSendEcho, IP_OPTION_INFORMATION};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;

    pub struct WinIcmp {
        handle: isize,
    }

    impl WinIcmp {
        pub fn new() -> Self {
            let handle = unsafe { IcmpCreateFile() };
            Self { handle }
        }

        pub fn send_echo(&self, target: IpAddr, ttl: u8, timeout_ms: u32) -> Option<(IpAddr, Duration)> {
            if self.handle == INVALID_HANDLE_VALUE { return None; }
            
            let ip_v4 = match target {
                IpAddr::V4(v4) => u32::from_ne_bytes(v4.octets()),
                _ => return None,
            };

            let options = IP_OPTION_INFORMATION {
                Ttl: ttl,
                Tos: 0,
                Flags: 0,
                OptionsSize: 0,
                OptionsData: std::ptr::null_mut(),
            };

            let mut reply_buffer = [0u8; 1024];
            let start = Instant::now();
            let res = unsafe {
                IcmpSendEcho(
                    self.handle,
                    ip_v4,
                    std::ptr::null_mut(),
                    0,
                    &options,
                    reply_buffer.as_mut_ptr() as *mut _,
                    reply_buffer.len() as u32,
                    timeout_ms,
                )
            };

            if res > 0 {
                let rtt = start.elapsed();
                let addr_bytes = [reply_buffer[0], reply_buffer[1], reply_buffer[2], reply_buffer[3]];
                let reply_addr = std::net::Ipv4Addr::from(addr_bytes);
                Some((IpAddr::V4(reply_addr), rtt))
            } else {
                None
            }
        }
    }

    impl Drop for WinIcmp {
        fn drop(&mut self) {
            if self.handle != INVALID_HANDLE_VALUE {
                unsafe { IcmpCloseHandle(self.handle) };
            }
        }
    }
}
