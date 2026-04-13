# solana-arb

High-performance Solana MEV/arbitrage detection engine in Rust. Real-time cross-DEX price monitoring, paper-trading simulation via Jupiter, and micro-cap token discovery with on-chain safety validation.

## Architecture

```
                    ┌─────────────────────────────────────────────┐
                    │              solana-arb engine               │
                    └─────────────────────────────────────────────┘
                                        │
          ┌─────────────┬───────────────┼───────────────┬─────────────┐
          ▼             ▼               ▼               ▼             ▼
   ┌────────────┐ ┌──────────┐ ┌─────────────┐ ┌────────────┐ ┌──────────┐
   │ HTTP Price │ │ WebSocket│ │Profitability│ │  Micro-Cap │ │Graduation│
   │  Poller    │ │ Monitor  │ │  Scanner    │ │  Discovery │ │  Sniper  │
   │            │ │          │ │             │ │            │ │          │
   │DexScreener │ │ account  │ │ Jupiter     │ │DexScreener │ │ PumpFun  │
   │ 5 tokens   │ │Subscribe │ │ round-trip  │ │ new tokens │ │complete  │
   │ 5s interval│ │ Orca +   │ │ + triangular│ │ + safety   │ │ flag     │
   │            │ │ Raydium  │ │ arb scans   │ │ checks     │ │ triggers │
   └─────┬──────┘ └────┬─────┘ └──────┬──────┘ └─────┬──────┘ └────┬─────┘
         │              │              │              │              │
         └──────────────┴──────┬───────┴──────────────┘              │
                               ▼                                     │
                    ┌─────────────────────┐                          │
                    │   Detection Engine  │◄─────────────────────────┘
                    │ Quote-token grouped │
                    │ Delta tracking      │
                    └──────────┬──────────┘
                               │
                    ┌──────────▼──────────┐
                    │ Jupiter Simulation  │
                    │ DEX-specific routing│
                    │ 10 SOL paper trades │
                    └──────────┬──────────┘
                               │
              ┌────────────────┼────────────────┐
              ▼                ▼                ▼
       ┌────────────┐  ┌────────────┐  ┌────────────┐
       │  Postgres  │  │  REST API  │  │ Dashboard  │
       │  3 tables  │  │  Axum      │  │ localhost  │
       └────────────┘  └────────────┘  └────────────┘
```

## Features

**Price Monitoring**
- Real-time HTTP polling via DexScreener across Raydium, Orca, Meteora, PumpFun, PumpSwap
- WebSocket `accountSubscribe` for Orca Whirlpool and Raydium vault accounts
- On-chain pool account decoding (sqrt_price for CLMM, vault balances for AMM)
- 5 default tokens: BONK, WIF, POPCAT, MEW, FARTCOIN

**Arbitrage Detection**
- Quote-token-aware price graph (only compares same-pair pools)
- Cross-DEX spread detection with per-DEX fee modeling
- Delta tracking (flags significant price moves as potential triggers)
- Deduplication to prevent duplicate opportunity alerts

**Paper Trading Simulation**
- Jupiter V6 Quote API with DEX-specific routing
- Round-trip profitability scanning (SOL to TOKEN to SOL)
- Triangular arbitrage (SOL to TOKEN to USDC/USDT to SOL)
- 10 SOL default trade size with dynamic scaling

**Token Discovery**
- Micro-cap token discovery via DexScreener new token profiles
- On-chain safety validation (freeze authority + mint authority checks)
- PumpFun graduation detection with 60-second sniper scanning

**Dashboard**
- Embedded web dashboard served by Axum (no build step)
- Live stats, opportunity table, simulation results, DEX breakdown
- Auto-refresh every 5 seconds
- Dark theme aligned with trading terminal aesthetics

## Workspace Structure

```
crates/
  arb-types/     Shared types, DEX enum, token constants
  arb-feed/      Price feeds: HTTP polling, WebSocket, discovery
  arb-detect/    Detection engine: price graph, spread detection
  arb-sim/       Simulation: Jupiter quotes, scanner, atomic tx
  arb-store/     Postgres storage via sqlx
  arb-api/       Axum REST API + static file serving
  arb-cli/       CLI entry point, pipeline wiring
```

## Quick Start

### Prerequisites

- Rust (stable)
- PostgreSQL
- Helius API key (optional, for WebSocket monitoring)

### Setup

```bash
# Clone
git clone https://github.com/matthewrahm/solana-arb.git
cd solana-arb

# Create database
createdb solana_arb

# Configure (optional)
cp .env.example .env
# Edit .env with your HELIUS_API_KEY

# Build
cargo build --release

# Run
cargo run --release -- --database-url postgres://localhost/solana_arb
```

### CLI Options

```
solana-arb [OPTIONS]

Options:
  -k, --api-key <KEY>        Helius API key (or HELIUS_API_KEY env)
      --database-url <URL>   Postgres URL (default: postgres://localhost/solana_arb)
  -p, --port <PORT>          API/dashboard port (default: 3002)
      --poll-interval <SECS> Price polling interval (default: 5)
      --min-spread <BPS>     Minimum spread to flag (default: 10)
      --min-liquidity <USD>  Minimum pool liquidity (default: 1000)
      --watch <MINTS>        Additional token mints (comma-separated)
      --no-ws                Disable WebSocket monitoring
```

### Dashboard

Open `http://localhost:3002` in your browser.

### API Endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/v1/opportunities` | Recent arbitrage opportunities |
| `GET /api/v1/stats` | Aggregate opportunity statistics |
| `GET /api/v1/simulations` | Recent simulation results |
| `GET /api/v1/simulations/stats` | Simulation aggregate stats |
| `GET /api/v1/dex-breakdown` | Opportunity count per DEX pair |
| `GET /api/v1/health` | Health check |

## Key Findings

Running against live Solana mainnet data:

- **Liquid memecoins are extremely efficient.** BONK, WIF, MEW round-trips lose 5-53 bps. Jupiter's aggregator closes cross-DEX spreads faster than HTTP polling can detect them.
- **Triangular routes offer no advantage.** SOL to TOKEN to USDC to SOL performs identically to direct round-trips for established tokens.
- **Micro-cap PumpSwap tokens have wider spreads.** Newly graduated tokens (MARUN, HIMA, GIGADOGE) on PumpSwap with $5K-27K liquidity show wider pricing, but Jupiter rate limits constrain scanning throughput.
- **Real arb requires sub-millisecond execution.** WebSocket detection at ~50ms is 100x faster than HTTP, but still too slow for competitive MEV. Production systems use Jito bundles with validator-level colocation.

## Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `reqwest` | HTTP client (DexScreener, Jupiter) |
| `tokio-tungstenite` | WebSocket (Solana accountSubscribe) |
| `sqlx` | Async PostgreSQL |
| `axum` | REST API + dashboard serving |
| `tower-http` | CORS, tracing, static files |
| `serde` | Serialization |
| `clap` | CLI argument parsing |
| `chrono` | Timestamps |
| `base64` / `bs58` | On-chain account data decoding |

## License

MIT
