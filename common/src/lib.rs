use chrono::Utc;
use config::{Config, ConfigError, Environment, File};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Metric {
    pub id: Uuid,
    pub system_name: String,
    pub tool_name: String,
    pub timestamp: i64,
    pub metric_type: String,
    pub values: std::collections::HashMap<String, f64>,
    pub tags: std::collections::HashMap<String, String>,
}

impl Metric {
    pub fn new(tool_name: &str, metric_type: &str) -> Self {
        let system_name = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        Self {
            id: Uuid::new_v4(),
            system_name,
            tool_name: tool_name.to_string(),
            timestamp: Utc::now().timestamp_nanos_opt().unwrap_or(0),
            metric_type: metric_type.to_string(),
            values: std::collections::HashMap::new(),
            tags: std::collections::HashMap::new(),
        }
    }

    pub fn with_value(mut self, key: &str, value: f64) -> Self {
        self.values.insert(key.to_string(), value);
        self
    }

    pub fn with_tag(mut self, key: &str, value: &str) -> Self {
        self.tags.insert(key.to_string(), value.to_string());
        self
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub server_url: String,
    pub system_name: Option<String>,
    pub duration_secs: u64,
}

impl AppConfig {
    pub fn load(config_path: Option<&str>) -> Result<Self, ConfigError> {
        let mut s = Config::builder()
            .set_default("server_url", "http://localhost:3000")?
            .set_default("duration_secs", 300)?;

        if let Some(path) = config_path {
            s = s.add_source(File::with_name(path).required(false));
        } else {
            s = s.add_source(File::with_name("config").required(false));
        }

        s = s.add_source(Environment::with_prefix("NETBENCH"));

        s.build()?.try_deserialize()
    }
}

pub struct ReportingClient {
    client: reqwest::Client,
    server_url: String,
}

impl ReportingClient {
    pub fn new(server_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            server_url,
        }
    }

    pub async fn report(&self, metric: Metric) -> anyhow::Result<()> {
        let url = format!("{}/metrics", self.server_url);
        let resp = self.client.post(&url).json(&metric).send().await?;

        if !resp.status().is_success() {
            anyhow::bail!("Failed to report metric: {}", resp.status());
        }

        Ok(())
    }
}

pub fn get_local_ip(target: &str) -> String {
    // Try to find the local IP used to reach the target
    // We do this by creating a UDP socket and "connecting" it (no packets sent)
    // We use port 1 as a dummy port if none is provided
    let target_addr = if target.contains(':') {
        target.to_string()
    } else {
        format!("{}:1", target)
    };

    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect(target_addr).is_ok() {
            if let Ok(addr) = socket.local_addr() {
                let ip = addr.ip();
                if !ip.is_unspecified() {
                    return ip.to_string();
                }
            }
        }
    }

    // Fallback: Use hostname to try and resolve a local IP if possible,
    // though the dummy connect is usually much more reliable for finding the "right" interface.
    "0.0.0.0".to_string()
}
