-- Execution tracking: records attempted and completed trades
CREATE TABLE IF NOT EXISTS executions (
    id UUID PRIMARY KEY,
    strategy VARCHAR(32) NOT NULL,         -- 'cross_venue_arb', 'graduation_snipe', 'back_run'
    mode VARCHAR(16) NOT NULL,             -- 'paper', 'simulate', 'live'
    token_mint VARCHAR(44) NOT NULL,
    token_symbol VARCHAR(32),
    buy_dex VARCHAR(32),
    sell_dex VARCHAR(32),
    input_lamports BIGINT NOT NULL,
    expected_output_lamports BIGINT,
    actual_output_lamports BIGINT,
    expected_profit_lamports BIGINT,
    actual_profit_lamports BIGINT,
    tip_lamports BIGINT,
    tx_signature VARCHAR(88),
    bundle_id VARCHAR(88),
    status VARCHAR(16) NOT NULL,           -- 'paper', 'simulated', 'submitted', 'confirmed', 'failed'
    error_message TEXT,
    simulation_units BIGINT,
    executed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_executions_time ON executions (executed_at DESC);
CREATE INDEX IF NOT EXISTS idx_executions_status ON executions (status);
CREATE INDEX IF NOT EXISTS idx_executions_strategy ON executions (strategy);
