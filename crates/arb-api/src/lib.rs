use axum::{extract::State, routing::get, Json, Router};
use sqlx::PgPool;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use arb_store::queries;

#[derive(Clone)]
struct AppState {
    pool: PgPool,
}

pub fn build_router(pool: PgPool) -> Router {
    let state = AppState { pool };

    Router::new()
        .route("/api/v1/opportunities", get(get_opportunities))
        .route("/api/v1/stats", get(get_stats))
        .route("/api/v1/health", get(health))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn get_opportunities(
    State(state): State<AppState>,
) -> Result<Json<Vec<queries::OpportunityRow>>, StatusError> {
    let opps = queries::get_recent_opportunities(&state.pool, 100)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(opps))
}

async fn get_stats(
    State(state): State<AppState>,
) -> Result<Json<queries::StatsRow>, StatusError> {
    let stats = queries::get_stats(&state.pool)
        .await
        .map_err(|e| StatusError(format!("{e}")))?;
    Ok(Json(stats))
}

async fn health() -> &'static str {
    "ok"
}

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
