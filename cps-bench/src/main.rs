use clap::Parser;
use common::{AppConfig, Metric, ReportingClient};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
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

    /// Bind address for the server (when running as server)
    #[arg(short, long, default_value = "0.0.0.0:8001")]
    bind: String,

    /// Number of concurrent connection attempts to sustain
    #[arg(short, long, default_value_t = 100)]
    concurrency: usize,

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
        eprintln!("Usage: cps-bench.exe [server|client] [TARGET]\n");
        eprintln!("Examples:");
        eprintln!("  cps-bench.exe server                             # Start server on 0.0.0.0:8001");
        eprintln!("  cps-bench.exe 192.168.1.10                       # Start client to target 192.168.1.10:8001");
        eprintln!("  cps-bench.exe 192.168.1.10:8001 -c 500           # Target specific port with 500 concurrency");
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
            format!("{}:8001", raw_target)
        };

        let run_duration = cli.duration.unwrap_or(config.duration_secs);
        run_client(&target, cli.concurrency, run_duration, reporter).await?;
    }

    Ok(())
}

async fn run_server(bind: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!("CPS Bench server listening on {}", bind);

    // Track total accepted to optionally log rate
    let accepted = Arc::new(AtomicU64::new(0));
    let accepted_clone = accepted.clone();
    
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        let mut last_count = 0;
        loop {
            interval.tick().await;
            let current = accepted_clone.load(Ordering::Relaxed);
            let rate = current - last_count;
            if rate > 0 {
                info!("Server accepting: {} connections/sec", rate);
            }
            last_count = current;
        }
    });

    loop {
        match listener.accept().await {
            Ok((_socket, _addr)) => {
                accepted.fetch_add(1, Ordering::Relaxed);
                // Immediately drop socket to free resources rapidly
            }
            Err(e) => {
                warn!("Accept error: {}", e);
            }
        }
    }
}

async fn run_client(
    target: &str,
    concurrency: usize,
    duration_secs: u64,
    reporter: ReportingClient,
) -> anyhow::Result<()> {
    let target_addr: SocketAddr = target.parse()?;
    info!(
        "Starting CPS validation to {} (concurrency: {}, duration: {}s)",
        target, concurrency, duration_secs
    );

    let successful = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let start_time = Instant::now();
    let duration = Duration::from_secs(duration_secs);

    let mut handles = Vec::new();

    // Spawn concurrent loopers
    for _ in 0..concurrency {
        let succ = Arc::clone(&successful);
        let fail = Arc::clone(&failed);
        let target = target_addr;

        handles.push(tokio::spawn(async move {
            while start_time.elapsed() < duration {
                // Timeouts are critical so one dead attempt doesn't stall the loop
                match tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(target)).await {
                    Ok(Ok(_stream)) => {
                        succ.fetch_add(1, Ordering::Relaxed);
                        // Stream is dropped immediately here
                    }
                    Ok(Err(_)) => {
                        fail.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => { // Timeout
                        fail.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    // Reporter loop
    let mut report_interval = tokio::time::interval(Duration::from_secs(1));
    let mut last_succ = 0;
    let mut last_fail = 0;
    
    while start_time.elapsed() < duration {
        report_interval.tick().await;
        
        let curr_succ = successful.load(Ordering::Relaxed);
        let curr_fail = failed.load(Ordering::Relaxed);
        
        let cps = curr_succ - last_succ;
        let errors = curr_fail - last_fail;
        
        info!("CPS: {} successful/sec, {} failed/sec", cps, errors);

        let system_ip = common::get_local_ip(&target_addr.ip().to_string());
        let metric = Metric::new("cps-bench", "connections")
            .with_value("cps", cps as f64)
            .with_value("errors", errors as f64)
            .with_tag("target", &target_addr.to_string())
            .with_tag("system_ip", &system_ip)
            .with_tag("target_ip", &target_addr.ip().to_string());
        
        let _ = reporter.report(metric).await;

        last_succ = curr_succ;
        last_fail = curr_fail;
    }

    // Wait for all connect tasks to stop
    for h in handles {
        let _ = h.await;
    }

    info!("Finished CPS Validation.");
    Ok(())
}
