pub mod queries;

use anyhow::Result;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tracing::info;

pub async fn create_pool(database_url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await?;
    Ok(pool)
}

pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    let schema = include_str!("../../../migrations/001_schema.sql");
    sqlx::raw_sql(schema).execute(pool).await?;

    let signals = include_str!("../../../migrations/002_swap_signals.sql");
    sqlx::raw_sql(signals).execute(pool).await?;

    let safety = include_str!("../../../migrations/003_token_safety.sql");
    sqlx::raw_sql(safety).execute(pool).await?;

    let executions = include_str!("../../../migrations/004_executions.sql");
    sqlx::raw_sql(executions).execute(pool).await?;

    let observations = include_str!("../../../migrations/005_observations.sql");
    sqlx::raw_sql(observations).execute(pool).await?;

    info!("Database migrations applied");
    Ok(())
}
