CREATE TABLE IF NOT EXISTS price_snapshots (
    id         BIGSERIAL    PRIMARY KEY,
    base_mint  VARCHAR(44)  NOT NULL,
    dex        VARCHAR(32)  NOT NULL,
    price_usd  DOUBLE PRECISION NOT NULL,
    liquidity_usd DOUBLE PRECISION,
    pool_address  VARCHAR(64),
    source     VARCHAR(16)  NOT NULL,
    captured_at   TIMESTAMPTZ NOT NULL,
    indexed_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS arb_opportunities (
    id             UUID PRIMARY KEY,
    base_mint      VARCHAR(44)  NOT NULL,
    token_symbol   VARCHAR(32)  NOT NULL,
    buy_dex        VARCHAR(32)  NOT NULL,
    buy_price      DOUBLE PRECISION NOT NULL,
    buy_pool       VARCHAR(64),
    sell_dex       VARCHAR(32)  NOT NULL,
    sell_price     DOUBLE PRECISION NOT NULL,
    sell_pool      VARCHAR(64),
    gross_spread_bps   DOUBLE PRECISION NOT NULL,
    estimated_fees_bps DOUBLE PRECISION NOT NULL,
    net_spread_bps     DOUBLE PRECISION NOT NULL,
    estimated_profit_usd DOUBLE PRECISION,
    detected_at    TIMESTAMPTZ NOT NULL,
    detection_latency_ms BIGINT,
    indexed_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS simulations (
    id              UUID PRIMARY KEY,
    opportunity_id  UUID REFERENCES arb_opportunities(id),
    input_amount    BIGINT NOT NULL,
    input_mint      VARCHAR(44) NOT NULL,
    simulated_output BIGINT,
    output_mint     VARCHAR(44) NOT NULL,
    simulated_profit_lamports BIGINT,
    tx_fee_lamports    BIGINT,
    priority_fee_lamports BIGINT,
    simulation_success BOOLEAN NOT NULL,
    error_message   TEXT,
    simulated_at    TIMESTAMPTZ NOT NULL,
    indexed_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_prices_mint_time ON price_snapshots (base_mint, captured_at DESC);
CREATE INDEX IF NOT EXISTS idx_prices_dex ON price_snapshots (dex);
CREATE INDEX IF NOT EXISTS idx_opps_mint_time ON arb_opportunities (base_mint, detected_at DESC);
CREATE INDEX IF NOT EXISTS idx_opps_net_spread ON arb_opportunities (net_spread_bps DESC);
CREATE INDEX IF NOT EXISTS idx_sims_opp ON simulations (opportunity_id);
