-- Phase B2: stale-reserve observation mode.
-- Captures post-trigger pool implied price vs external fair-price reference,
-- so we can measure whether the stale-reserve opportunity surface exists
-- before building execution logic for it.

CREATE TABLE IF NOT EXISTS stale_reserve_observations (
    id BIGSERIAL PRIMARY KEY,
    signature VARCHAR(88),
    token_mint VARCHAR(44) NOT NULL,
    token_symbol VARCHAR(32),
    dex VARCHAR(32) NOT NULL,
    pool_address VARCHAR(64) NOT NULL,
    trigger_direction VARCHAR(4),
    trigger_sol_equivalent DOUBLE PRECISION,
    pool_implied_price_usd DOUBLE PRECISION,
    fair_price_usd DOUBLE PRECISION,
    delta_bps DOUBLE PRECISION,
    pool_liquidity_usd DOUBLE PRECISION,
    observation_latency_ms BIGINT,
    observed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_obs_time ON stale_reserve_observations (observed_at DESC);
CREATE INDEX IF NOT EXISTS idx_obs_token ON stale_reserve_observations (token_mint);
CREATE INDEX IF NOT EXISTS idx_obs_delta ON stale_reserve_observations (delta_bps DESC);
