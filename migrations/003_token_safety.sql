CREATE TABLE IF NOT EXISTS token_safety (
    mint VARCHAR(44) PRIMARY KEY,
    rugcheck_score DOUBLE PRECISION,
    risk_level VARCHAR(10),
    mint_revoked BOOLEAN,
    freeze_revoked BOOLEAN,
    top_holder_pct DOUBLE PRECISION,
    safe BOOLEAN,
    checked_at TIMESTAMPTZ DEFAULT NOW()
);
