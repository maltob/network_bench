use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use hickory_resolver::config::{NameServerConfig, Protocol, ResolverConfig, ResolverOpts};
use hickory_resolver::TokioAsyncResolver;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Mode ('server', 'client') or a target IP/hostname (assumes client)
    #[arg(index = 1)]
    mode_or_target: Option<String>,

    /// Target IP/hostname when 'client' is explicitly specified
    #[arg(index = 2)]
    explicit_target: Option<String>,

    /// Domains to query, comma separated (e.g. google.com,cloudflare.com)
    #[arg(short = 'D', long, default_value = "google.com,cloudflare.com,amazon.com,apple.com,microsoft.com")]
    domains: String,

    /// Number of queries per second to attempt
    #[arg(short, long, default_value_t = 10)]
    qps: u64,

    /// Duration of the test in seconds
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
        eprintln!("Usage: dns-bench.exe [client] [TARGET_DNS_IP]\n");
        eprintln!("Examples:");
        eprintln!("  dns-bench.exe 8.8.8.8                           # Start DNS bench against 8.8.8.8");
        eprintln!("  dns-bench.exe 1.1.1.1 --qps 50                  # Target 1.1.1.1 with 50 QPS");
        std::process::exit(1);
    });

    if mode_or_target.eq_ignore_ascii_case("server") {
        eprintln!("Error: dns-bench does not run a DNS server; it only tests them. Please specify a target IP (e.g. dns-bench 8.8.8.8).");
        std::process::exit(1);
    }

    let raw_target = if mode_or_target.eq_ignore_ascii_case("client") {
        cli.explicit_target.unwrap_or_else(|| {
            eprintln!("Error: Target DNS resolver IP must be provided.");
            std::process::exit(1);
        })
    } else {
        mode_or_target
    };

    let target_ip = if raw_target.contains(':') {
        raw_target
    } else {
        format!("{}:53", raw_target)
    };

    let target: SocketAddr = target_ip.parse().unwrap_or_else(|_| {
        eprintln!("Error: Invalid target IP address '{}'", target_ip);
        std::process::exit(1);
    });

    let run_duration = cli.duration.unwrap_or(config.duration_secs);
    let domains: Vec<String> = cli.domains.split(',').map(|s| s.trim().to_string()).collect();

    run_bench(target, domains, cli.qps, run_duration, reporter).await?;

    Ok(())
}

async fn run_bench(
    target: SocketAddr,
    domains: Vec<String>,
    qps: u64,
    duration_secs: u64,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    info!("Starting DNS Bench against {} ({} domains, {} QPS)", target, domains.len(), qps);

    let mut resolver_cfg = ResolverConfig::new();
    resolver_cfg.add_name_server(NameServerConfig::new(target, Protocol::Udp));
    
    let mut opts = ResolverOpts::default();
    opts.timeout = Duration::from_secs(2);
    opts.attempts = 1;
    // We want to actually measure network time, not local cache
    opts.cache_size = 0; 

    let resolver = TokioAsyncResolver::tokio(resolver_cfg, opts);

    let start_time = Instant::now();
    let total_duration = Duration::from_secs(duration_secs);
    
    let interval_ms = 1000 / std::cmp::max(1, qps);
    let mut timer = tokio::time::interval(Duration::from_millis(interval_ms));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let successful = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));

    // Shared jitter tracking
    let total_latency_ms = Arc::new(tokio::sync::Mutex::new(0.0f64));

    let mut handles = Vec::new();
    let mut domain_idx = 0;

    let system_ip = common::get_local_ip(&target.ip().to_string());

    // Spawn reporting task
    let succ_clone = Arc::clone(&successful);
    let fail_clone = Arc::clone(&failed);
    let lat_clone = Arc::clone(&total_latency_ms);
    let target_str = target.to_string();
    let target_ip_str = target.ip().to_string();
    
    let reporter_handle = tokio::spawn(async move {
        let mut report_interval = tokio::time::interval(Duration::from_secs(1));
        let mut last_succ = 0;
        let mut last_fail = 0;

        loop {
            report_interval.tick().await;

            let curr_succ = succ_clone.load(Ordering::Relaxed);
            let curr_fail = fail_clone.load(Ordering::Relaxed);
            let d_succ = curr_succ - last_succ;
            let d_fail = curr_fail - last_fail;

            let mut avg_latency = 0.0;
            {
                let mut lat_lock = lat_clone.lock().await;
                if d_succ > 0 {
                    avg_latency = *lat_lock / d_succ as f64;
                }
                *lat_lock = 0.0; // Reset for next tick
            }

            if d_succ > 0 || d_fail > 0 {
                info!("DNS: {} successful, {} failed, Avg Latency: {:.2}ms", d_succ, d_fail, avg_latency);

                let metric = Metric::new("dns-bench", "resolution")
                    .with_value("success_rate", d_succ as f64)
                    .with_value("error_rate", d_fail as f64)
                    .with_value("latency_ms", avg_latency)
                    .with_tag("target", &target_str)
                    .with_tag("system_ip", &system_ip)
                    .with_tag("target_ip", &target_ip_str);
                
                let _ = reporter.report(metric).await;
            }

            last_succ = curr_succ;
            last_fail = curr_fail;
        }
    });

    handles.push(reporter_handle);

    // Main query loop
    while start_time.elapsed() < total_duration {
        timer.tick().await;

        let domain = domains[domain_idx % domains.len()].clone();
        domain_idx += 1;

        let resolver = resolver.clone();
        let succ = Arc::clone(&successful);
        let fail = Arc::clone(&failed);
        let lat = Arc::clone(&total_latency_ms);

        tokio::spawn(async move {
            let attempt_start = Instant::now();
            match resolver.ipv4_lookup(domain.as_str()).await {
                Ok(_) => {
                    let elapsed = attempt_start.elapsed().as_secs_f64() * 1000.0;
                    succ.fetch_add(1, Ordering::Relaxed);
                    let mut l = lat.lock().await;
                    *l += elapsed;
                }
                Err(_) => {
                    fail.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    }

    // Wait slightly for trailing queries
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Abort handles
    for h in handles {
        h.abort();
    }

    info!("Finished DNS Bench.");
    Ok(())
}
