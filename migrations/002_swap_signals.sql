CREATE TABLE IF NOT EXISTS swap_signals (
    id BIGSERIAL PRIMARY KEY,
    signature VARCHAR(88),
    token_mint VARCHAR(44),
    platform VARCHAR(32),
    signer VARCHAR(44),
    direction VARCHAR(4),
    sol_equivalent DOUBLE PRECISION,
    triggered_scan BOOLEAN DEFAULT FALSE,
    scan_profitable BOOLEAN,
    received_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_signals_token ON swap_signals(token_mint);
CREATE INDEX IF NOT EXISTS idx_signals_time ON swap_signals(received_at DESC);
