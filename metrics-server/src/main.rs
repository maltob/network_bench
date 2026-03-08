use axum::{
    extract::State,
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use common::{AppConfig, Metric};
use futures_util::stream::Stream;
use serde_json::json;
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::{error, info};

#[derive(Parser)]
struct Cli {
    #[arg(short, long)]
    config: Option<String>,
    #[arg(short, long)]
    bind: Option<String>,
}

struct AppState {
    pool: SqlitePool,
    tx: broadcast::Sender<Metric>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let _config = AppConfig::load(cli.config.as_deref())?;
    let bind_addr: SocketAddr = cli
        .bind
        .unwrap_or_else(|| "0.0.0.0:3000".to_string())
        .parse()?;

    info!("Starting metrics server on {}", bind_addr);

    let db_path = "metrics.db";
    let db_url = format!("sqlite:{}", db_path);
    if !std::path::Path::new(db_path).exists() {
        std::fs::File::create(db_path)?;
    }

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS metrics (
            id TEXT PRIMARY KEY,
            system_name TEXT NOT NULL,
            tool_name TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            metric_type TEXT NOT NULL,
            values_json TEXT NOT NULL,
            tags_json TEXT NOT NULL
        )",
    )
    .execute(&pool)
    .await?;

    let (tx, _) = broadcast::channel(100);
    let state = Arc::new(AppState { pool, tx });

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/events", get(events_handler))
        .route("/api/history", get(get_history))
        .route("/metrics", post(record_metric))
        .with_state(state);

    info!("Metrics server listening on {}", bind_addr);

    axum::Server::bind(&bind_addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

async fn dashboard() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("dashboard.html"))
}

async fn events_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = state.tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|m| match m {
        Ok(metric) => Some(Ok(
            Event::default().data(serde_json::to_string(&metric).unwrap())
        )),
        Err(_) => None,
    });

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(1))
            .text("keep-alive"),
    )
}

async fn get_history(State(state): State<Arc<AppState>>) -> Json<Vec<serde_json::Value>> {
    let rows = sqlx::query("SELECT * FROM metrics ORDER BY timestamp DESC LIMIT 200")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();

    let history: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|row| {
            let values: HashMap<String, f64> =
                serde_json::from_str(&row.get::<String, _>("values_json")).unwrap_or_default();
            let tags: HashMap<String, String> =
                serde_json::from_str(&row.get::<String, _>("tags_json")).unwrap_or_default();
            json!({
                "id": row.get::<String, _>("id"),
                "system_name": row.get::<String, _>("system_name"),
                "tool_name": row.get::<String, _>("tool_name"),
                "timestamp": row.get::<i64, _>("timestamp"),
                "metric_type": row.get::<String, _>("metric_type"),
                "values": values,
                "tags": tags
            })
        })
        .collect();

    Json(history)
}

async fn record_metric(
    State(state): State<Arc<AppState>>,
    Json(metric): Json<Metric>,
) -> Result<String, (axum::http::StatusCode, String)> {
    let values_json = serde_json::to_string(&metric.values).unwrap_or_default();
    let tags_json = serde_json::to_string(&metric.tags).unwrap_or_default();

    sqlx::query(
        "INSERT INTO metrics (id, system_name, tool_name, timestamp, metric_type, values_json, tags_json)
         VALUES (?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(metric.id.to_string())
    .bind(&metric.system_name)
    .bind(&metric.tool_name)
    .bind(metric.timestamp)
    .bind(&metric.metric_type)
    .bind(values_json)
    .bind(tags_json)
    .execute(&state.pool)
    .await
    .map_err(|e| {
        error!("Database error: {}", e);
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "Database error".to_string())
    })?;

    // Broadcast to connected dashboard clients
    let _ = state.tx.send(metric);

    Ok("Metric recorded".to_string())
}
