use anyhow::Result;
use arb_types::{ArbOpportunity, PriceQuote, SimResult};
use sqlx::PgPool;

/// Insert a batch of price snapshots
pub async fn insert_price_snapshots(pool: &PgPool, quotes: &[PriceQuote]) -> Result<()> {
    for q in quotes {
        sqlx::query(
            r#"
            INSERT INTO price_snapshots (base_mint, dex, price_usd, liquidity_usd, pool_address, source, captured_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(&q.base_mint)
        .bind(q.dex.as_str())
        .bind(q.price_usd)
        .bind(q.liquidity_usd)
        .bind(&q.pool_address)
        .bind(match q.source {
            arb_types::PriceSource::HttpPoll => "http_poll",
            arb_types::PriceSource::WebSocket => "websocket",
        })
        .bind(q.timestamp)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Insert a detected arbitrage opportunity
pub async fn insert_opportunity(pool: &PgPool, opp: &ArbOpportunity) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO arb_opportunities
            (id, base_mint, token_symbol, buy_dex, buy_price, buy_pool,
             sell_dex, sell_price, sell_pool, gross_spread_bps, estimated_fees_bps,
             net_spread_bps, estimated_profit_usd, detected_at, detection_latency_ms)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        "#,
    )
    .bind(opp.id)
    .bind(&opp.base_mint)
    .bind(&opp.token_symbol)
    .bind(opp.buy_dex.as_str())
    .bind(opp.buy_price)
    .bind(&opp.buy_pool)
    .bind(opp.sell_dex.as_str())
    .bind(opp.sell_price)
    .bind(&opp.sell_pool)
    .bind(opp.gross_spread_bps)
    .bind(opp.estimated_fees_bps)
    .bind(opp.net_spread_bps)
    .bind(opp.estimated_profit_usd)
    .bind(opp.detected_at)
    .bind(opp.detection_latency_ms as i64)
    .execute(pool)
    .await?;
    Ok(())
}

/// Get recent opportunities, ordered by detection time
pub async fn get_recent_opportunities(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<OpportunityRow>> {
    let rows = sqlx::query_as::<_, OpportunityRow>(
        r#"
        SELECT id, base_mint, token_symbol, buy_dex, buy_price, sell_dex, sell_price,
               gross_spread_bps, net_spread_bps, estimated_profit_usd, detected_at, detection_latency_ms
        FROM arb_opportunities
        ORDER BY detected_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Get aggregate stats
pub async fn get_stats(pool: &PgPool) -> Result<StatsRow> {
    let row = sqlx::query_as::<_, StatsRow>(
        r#"
        SELECT
            COUNT(*)::bigint as total_opportunities,
            COALESCE(AVG(net_spread_bps), 0) as avg_spread_bps,
            COALESCE(MAX(net_spread_bps), 0) as max_spread_bps,
            COALESCE(SUM(estimated_profit_usd), 0) as total_estimated_profit,
            COUNT(DISTINCT base_mint)::bigint as tokens_monitored
        FROM arb_opportunities
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Insert a simulation result
pub async fn insert_simulation(pool: &PgPool, sim: &SimResult) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO simulations
            (id, opportunity_id, input_amount, input_mint, simulated_output, output_mint,
             simulated_profit_lamports, tx_fee_lamports, priority_fee_lamports,
             simulation_success, error_message, simulated_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        "#,
    )
    .bind(sim.id)
    .bind(sim.opportunity_id)
    .bind(sim.input_amount)
    .bind(&sim.input_mint)
    .bind(sim.simulated_output)
    .bind(&sim.output_mint)
    .bind(sim.simulated_profit_lamports)
    .bind(sim.tx_fee_lamports)
    .bind(sim.priority_fee_lamports)
    .bind(sim.simulation_success)
    .bind(&sim.error_message)
    .bind(sim.simulated_at)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct OpportunityRow {
    pub id: uuid::Uuid,
    pub base_mint: String,
    pub token_symbol: String,
    pub buy_dex: String,
    pub buy_price: f64,
    pub sell_dex: String,
    pub sell_price: f64,
    pub gross_spread_bps: f64,
    pub net_spread_bps: f64,
    pub estimated_profit_usd: Option<f64>,
    pub detected_at: chrono::DateTime<chrono::Utc>,
    pub detection_latency_ms: Option<i64>,
}

/// Get recent simulation results
pub async fn get_recent_simulations(pool: &PgPool, limit: i64) -> Result<Vec<SimulationRow>> {
    let rows = sqlx::query_as::<_, SimulationRow>(
        r#"
        SELECT s.id, s.opportunity_id, s.input_amount, s.simulated_output,
               s.simulated_profit_lamports, s.tx_fee_lamports, s.simulation_success,
               s.error_message, s.simulated_at,
               COALESCE(o.token_symbol, 'scan') as token_symbol
        FROM simulations s
        LEFT JOIN arb_opportunities o ON s.opportunity_id = o.id AND s.opportunity_id != '00000000-0000-0000-0000-000000000000'
        ORDER BY s.simulated_at DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Get simulation aggregate stats
pub async fn get_simulation_stats(pool: &PgPool) -> Result<SimStatsRow> {
    let row = sqlx::query_as::<_, SimStatsRow>(
        r#"
        SELECT
            COUNT(*)::bigint as total_simulations,
            COUNT(*) FILTER (WHERE simulation_success)::bigint as successful,
            COUNT(*) FILTER (WHERE simulation_success AND simulated_profit_lamports > 0)::bigint as profitable,
            COALESCE(AVG(simulated_profit_lamports::float8) FILTER (WHERE simulation_success), 0)::float8 as avg_profit_lamports,
            COALESCE(MAX(simulated_profit_lamports::float8) FILTER (WHERE simulation_success), 0)::float8 as best_profit_lamports,
            COALESCE(SUM(simulated_profit_lamports::float8) FILTER (WHERE simulation_success), 0)::float8 as total_profit_lamports
        FROM simulations
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// Get opportunity count per DEX pair
pub async fn get_dex_breakdown(pool: &PgPool) -> Result<Vec<DexBreakdownRow>> {
    let rows = sqlx::query_as::<_, DexBreakdownRow>(
        r#"
        SELECT buy_dex, sell_dex, COUNT(*)::bigint as count,
               COALESCE(AVG(net_spread_bps), 0) as avg_spread
        FROM arb_opportunities
        GROUP BY buy_dex, sell_dex
        ORDER BY count DESC
        LIMIT 20
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct StatsRow {
    pub total_opportunities: Option<i64>,
    pub avg_spread_bps: Option<f64>,
    pub max_spread_bps: Option<f64>,
    pub total_estimated_profit: Option<f64>,
    pub tokens_monitored: Option<i64>,
}

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct SimulationRow {
    pub id: uuid::Uuid,
    pub opportunity_id: Option<uuid::Uuid>,
    pub input_amount: i64,
    pub simulated_output: Option<i64>,
    pub simulated_profit_lamports: Option<i64>,
    pub tx_fee_lamports: Option<i64>,
    pub simulation_success: bool,
    pub error_message: Option<String>,
    pub simulated_at: chrono::DateTime<chrono::Utc>,
    pub token_symbol: String,
}

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct SimStatsRow {
    pub total_simulations: Option<i64>,
    pub successful: Option<i64>,
    pub profitable: Option<i64>,
    pub avg_profit_lamports: Option<f64>,
    pub best_profit_lamports: Option<f64>,
    pub total_profit_lamports: Option<f64>,
}

#[derive(sqlx::FromRow, serde::Serialize)]
pub struct DexBreakdownRow {
    pub buy_dex: String,
    pub sell_dex: String,
    pub count: Option<i64>,
    pub avg_spread: Option<f64>,
}
