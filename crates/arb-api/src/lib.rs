use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::{
    extract::{Path, State, WebSocketUpgrade, ws::{Message, WebSocket}},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures_util::{SinkExt, StreamExt};
use sqlx::PgPool;
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use arb_store::queries;
use arb_types::{RuntimeConfig, SystemStatus};

/// Shared application state for all API handlers.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<RwLock<RuntimeConfig>>,
    pub live_tx: broadcast::Sender<String>,
    /// Counters and health tracking (optional, set by CLI)
    pub signals_received: Arc<std::sync::atomic::AtomicU64>,
    pub scans_triggered: Arc<std::sync::atomic::AtomicU64>,
    pub profitable_scans: Arc<std::sync::atomic::AtomicU64>,
    pub forge_connected: Arc<std::sync::atomic::AtomicBool>,
    pub sol_usd_price: Arc<RwLock<f64>>,
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Existing read endpoints
        .route("/api/v1/opportunities", get(get_opportunities))
        .route("/api/v1/stats", get(get_stats))
        .route("/api/v1/simulations", get(get_simulations))
        .route("/api/v1/simulations/stats", get(get_simulation_stats))
        .route("/api/v1/dex-breakdown", get(get_dex_breakdown))
        .route("/api/v1/signals", get(get_signals))
        .route("/api/v1/signals/stats", get(get_signal_stats))
        .route("/api/v1/safety/{mint}", get(get_safety))
        .route("/api/v1/executions", get(get_executions))
        .route("/api/v1/health", get(health))
        // Control endpoints
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/config", get(get_config).post(update_config))
        .route("/api/v1/system/start", post(system_start))
        .route("/api/v1/system/stop", post(system_stop))
        // WebSocket live feed
        .route("/ws/live", get(ws_live))
        // Static files
        .fallback_service(ServeDir::new("public"))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ── Existing read endpoints ──

async fn get_opportunities(
    State(state): State<AppState>,
) -> Result<Json<Vec<queries::OpportunityRow>>, StatusError> {
    let opps = queries::get_recent_opportunities(&state.db, 100)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(opps))
}

async fn get_stats(
    State(state): State<AppState>,
) -> Result<Json<queries::StatsRow>, StatusError> {
    let stats = queries::get_stats(&state.db)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(stats))
}

async fn get_simulations(
    State(state): State<AppState>,
) -> Result<Json<Vec<queries::SimulationRow>>, StatusError> {
    let sims = queries::get_recent_simulations(&state.db, 50)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(sims))
}

async fn get_simulation_stats(
    State(state): State<AppState>,
) -> Result<Json<queries::SimStatsRow>, StatusError> {
    let stats = queries::get_simulation_stats(&state.db)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(stats))
}

async fn get_dex_breakdown(
    State(state): State<AppState>,
) -> Result<Json<Vec<queries::DexBreakdownRow>>, StatusError> {
    let breakdown = queries::get_dex_breakdown(&state.db)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(breakdown))
}

async fn get_signals(
    State(state): State<AppState>,
) -> Result<Json<Vec<queries::SignalRow>>, StatusError> {
    let signals = queries::get_recent_signals(&state.db, 50)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(signals))
}

async fn get_signal_stats(
    State(state): State<AppState>,
) -> Result<Json<queries::SignalStatsRow>, StatusError> {
    let stats = queries::get_signal_stats(&state.db)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(stats))
}

async fn get_safety(
    State(state): State<AppState>,
    Path(mint): Path<String>,
) -> Result<Json<Option<queries::TokenSafetyRow>>, StatusError> {
    let safety = queries::get_token_safety(&state.db, &mint)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(safety))
}

async fn get_executions(
    State(state): State<AppState>,
) -> Result<Json<Vec<queries::ExecutionRow>>, StatusError> {
    let execs = queries::get_recent_executions(&state.db, 50)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(execs))
}

async fn health() -> &'static str {
    "ok"
}

// ── Control endpoints ──

async fn get_status(State(state): State<AppState>) -> Json<SystemStatus> {
    let config = state.config.read().await;
    let sol_usd = *state.sol_usd_price.read().await;
    let uptime = if config.system_running {
        config.started_at.map(|t| {
            (chrono::Utc::now() - t).num_seconds().max(0) as u64
        })
    } else {
        None
    };

    Json(SystemStatus {
        system_running: config.system_running,
        mode: config.mode,
        forge_connected: state.forge_connected.load(Ordering::Relaxed),
        scanner_active: config.system_running,
        discovery_active: config.system_running,
        uptime_secs: uptime,
        sol_usd_price: sol_usd,
        signals_received: state.signals_received.load(Ordering::Relaxed),
        scans_triggered: state.scans_triggered.load(Ordering::Relaxed),
        profitable_scans: state.profitable_scans.load(Ordering::Relaxed),
    })
}

async fn get_config(State(state): State<AppState>) -> Json<RuntimeConfig> {
    Json(state.config.read().await.clone())
}

#[derive(serde::Deserialize)]
struct ConfigUpdate {
    mode: Option<arb_types::ExecutionMode>,
    min_signal_sol: Option<f64>,
    min_liquidity: Option<f64>,
    max_liquidity: Option<f64>,
    min_spread_bps: Option<f64>,
}

async fn update_config(
    State(state): State<AppState>,
    Json(update): Json<ConfigUpdate>,
) -> Json<RuntimeConfig> {
    let mut config = state.config.write().await;
    if let Some(mode) = update.mode {
        config.mode = mode;
    }
    if let Some(v) = update.min_signal_sol {
        config.min_signal_sol = v;
    }
    if let Some(v) = update.min_liquidity {
        config.min_liquidity = v;
    }
    if let Some(v) = update.max_liquidity {
        config.max_liquidity = v;
    }
    if let Some(v) = update.min_spread_bps {
        config.min_spread_bps = v;
    }

    // Broadcast config change event
    let event = serde_json::json!({
        "type": "config",
        "data": { "mode": config.mode, "min_signal_sol": config.min_signal_sol },
        "timestamp": chrono::Utc::now(),
    });
    state.live_tx.send(event.to_string()).ok();

    Json(config.clone())
}

async fn system_start(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut config = state.config.write().await;
    if config.system_running {
        return Json(serde_json::json!({ "status": "already_running" }));
    }
    config.system_running = true;
    config.started_at = Some(chrono::Utc::now());

    let event = serde_json::json!({
        "type": "system",
        "data": { "action": "started" },
        "timestamp": chrono::Utc::now(),
    });
    state.live_tx.send(event.to_string()).ok();

    Json(serde_json::json!({ "status": "started" }))
}

async fn system_stop(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut config = state.config.write().await;
    config.system_running = false;
    config.started_at = None;

    let event = serde_json::json!({
        "type": "system",
        "data": { "action": "stopped" },
        "timestamp": chrono::Utc::now(),
    });
    state.live_tx.send(event.to_string()).ok();

    Json(serde_json::json!({ "status": "stopped" }))
}

// ── WebSocket live feed ──

async fn ws_live(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_live_socket(socket, state.live_tx))
}

async fn handle_live_socket(socket: WebSocket, live_tx: broadcast::Sender<String>) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = live_tx.subscribe();

    // Spawn task to forward broadcast events to WebSocket
    let send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            if sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Consume incoming messages (ping/pong, close)
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(_msg)) = receiver.next().await {
            // Client messages ignored (control is via POST endpoints)
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
}

// ── Error type ──

struct StatusError(String);

impl axum::response::IntoResponse for StatusError {
    fn into_response(self) -> axum::response::Response {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            self.0,
        )
            .into_response()
    }
}
