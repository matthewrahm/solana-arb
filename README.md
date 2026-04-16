# solana-arb

![Rust](https://img.shields.io/badge/rust-1.80+-orange?logo=rust&logoColor=white)
![Solana](https://img.shields.io/badge/solana-mainnet-14F195?logo=solana&logoColor=black)
![Postgres](https://img.shields.io/badge/postgres-17-4169E1?logo=postgresql&logoColor=white)
![Jito](https://img.shields.io/badge/jito-block%20engine-000000)
![License](https://img.shields.io/badge/license-MIT-green)

Production-grade Solana MEV/arbitrage detection engine in Rust. Direct AMM math across Raydium V4/CLMM, PumpSwap, Orca Whirlpool, and PumpFun bonding curve. Forge-integrated real-time swap signals, Jito Block Engine bundle submission, honest cost modeling, and a shadow-mode observation pipeline that measures real stale-reserve windows without risking capital.

## Status: research archive

Built and measured to a concrete conclusion, then intentionally closed rather than chasing diminishing returns. The infrastructure works: 32-79ms signal-to-scan latency, five DEXs with direct AMM quoting, local-first scanning with no Jupiter dependency, a cost model calibrated for 2026 memecoin conditions. The market structure does not support the original cross-venue arb hypothesis for a solo developer. Full retrospective at the bottom of this README.

This repository is preserved as a portfolio artifact showing the Rust, Solana, and MEV-engineering depth that produced it.

## What this project does

solana-arb watches the Solana mainnet for exploitable price dislocations and tries to capture them atomically via Jito bundles. Specifically:

1. A sibling indexer, [solana-forge](https://github.com/matthewrahm/solana-forge), streams real-time swap events from Raydium, PumpFun, PumpSwap, and Jupiter.
2. solana-arb subscribes to that stream over WebSocket and triages each swap in under 100ms.
3. When a swap is large enough to meaningfully move its pool, solana-arb quotes the round-trip using on-chain reserves fetched in a single `getMultipleAccounts` call, deducts realistic execution costs (priority fee, Jito tip, slippage, MEV), and records the expected outcome to Postgres.
4. A browser control panel at `localhost:3002` exposes start/stop controls, mode toggle (paper/simulate/live), a real-time event feed, and tables for signals, scans, and executions.
5. An observation pipeline (Phase B2) measures the delta between each triggered pool's implied price and a liquidity-weighted external reference, so we can tell whether stale-reserve windows exist before building execution for them.

## Architecture

```
                     Helius WSS                     DexScreener
                         |                               |
            +------------+------------+                  |
            |                         |                  |
     +------v------+            +-----v-----+            |
     | solana-forge|            |arb-feed   |            |
     | logsSubscribe|            | discovery|            |
     | RPC fetcher |            | RugCheck  |            |
     +------+------+            +-----+-----+            |
            | /ws/events              |                  |
            |                         |                  |
            v                         v                  v
     +------+------------------------ +------------------+------+
     |                 arb-detect + arb-sim                     |
     |                                                          |
     |  forge_feed consumer  ---->  swap_analyzer               |
     |                                   |                      |
     |                                   +-> local_scanner      |
     |                                   |    (AMM math, 5 DEXs)|
     |                                   |                      |
     |                                   +-> StaleReserveObserver|
     |                                        (DexScreener +    |
     |                                         liq-weighted fair)|
     |                                                          |
     |  CostModel  ---->  ScanEconomics  ---->  CrossVenueResult|
     +---------------------------+------------------------------+
                                 |
               +-----------------+-----------------+
               v                 v                 v
        +------+-----+    +------+-----+    +------+-----+
        |  Postgres  |    |  arb-api   |    |Jito Bundle |
        | 7 tables   |    |  Axum REST |    | submission |
        +------+-----+    +------+-----+    +------+-----+
                                 |
                                 v
                        +--------+---------+
                        |  control panel   |
                        |  localhost:3002  |
                        +------------------+
```

Numbered data flow:

1. `forge-cli` subscribes to `logsSubscribe` on Raydium V4, Jupiter, PumpFun, and PumpSwap programs via Helius.
2. Forge fetches full transaction bodies at a configurable rate (default 5/s) and broadcasts decoded swap events over `/ws/events`.
3. `arb-cli` reconnects to that stream, filters by minimum SOL size and skips Jupiter-routed swaps (no residual spread to capture).
4. Each actionable swap fans out in parallel to `local_scanner.scan_triggered` (RPC-heavy, cross-venue AMM quoting) and `StaleReserveObserver.observe` (DexScreener HTTP, free).
5. Results feed `CostModel.compute` for honest net-profit math including Jito tip, priority fee, and execution penalty.
6. Rows are persisted to Postgres: `swap_signals`, `arb_opportunities`, `simulations`, `executions`, `stale_reserve_observations`.
7. The control panel polls the Axum REST API and streams live events via WebSocket.
8. In live mode, profitable scans become Jito bundles submitted to `mainnet.block-engine.jito.wtf`.

## Features

### Direct AMM math, no Jupiter dependency

- `AmmQuoter` computes swap outputs locally for Raydium V4 (constant-product, 25 bps fee), Raydium CLMM (sqrt_price, concentrated liquidity), PumpSwap (constant-product, 25 bps fee), Orca Whirlpool (Q64.64 sqrt_price), and PumpFun bonding curve (virtual reserves, 1% fee).
- Batched pool fetches via `getMultipleAccounts` (up to 100 accounts per RPC call).
- Eliminates the circular-reasoning trap of using Jupiter quotes to detect opportunities Jupiter already closed.

### Forge-integrated real-time signal stream

- WebSocket client to [solana-forge](https://github.com/matthewrahm/solana-forge)'s `/ws/events` endpoint.
- 3-second per-token debounce and minimum SOL threshold prevent signal storms.
- Jupiter-routed swaps filtered out at the analyzer because their spread is already captured.
- End-to-end signal-to-scan latency measured at 32-79ms under normal conditions.

### Honest cost model

The `CostModel` struct in `arb-sim/src/cost_model.rs` bakes in calibrated 2026 memecoin conditions:

| Parameter | Default | Rationale |
|-----------|---------|-----------|
| `base_fee_lamports` | 5,000 | Solana fixed per signature |
| `priority_fee_lamports_per_tx` | 3,000,000 | 50th percentile during memecoin activity |
| `num_txs` | 3 | Two swaps plus tip transfer in a Jito bundle |
| `execution_penalty_bps` | 50 | 30 bps slippage plus 20 bps MEV tax on thin pools |
| `min_jito_tip_lamports` | 50,000 | 75th percentile landed tip floor |
| `jito_tip_fraction` | 0.5 | Searcher convention: half of expected profit to the validator |

On a 1 SOL trade that requires roughly 140 bps of gross edge to net positive. Before this rewrite the scanner used a static 2.5M lamport cost and forgot to subtract the Jito tip, producing phantom-profitable scans that collapsed to losses in practice.

### StaleReserveObserver (phase B2)

- Zero Helius credits. Pure DexScreener HTTP.
- For each triggered signal: fetch all pairs for the token mint, identify the trigger pool by exact address or DEX match, compute a liquidity-weighted fair price across the remaining pairs, and log the delta in bps.
- Persists to `stale_reserve_observations` for offline analysis.
- Found deltas ranging from -1612 bps to +1833 bps during a 20-minute peak-hours sample, most of which were later diagnosed as fair-reference contamination rather than real opportunities (see retrospective).

### Micro-cap token discovery

- DexScreener new-token-profile polling every 3 minutes.
- On-chain safety validation: freeze authority and mint authority checks via `getAccountInfo`.
- RugCheck API integration with score and top-holder cutoffs.
- Fallback to RugCheck-only when RPC rate limits hit.
- Discovered tokens feed the periodic scan cycle.

### Jito bundle submission

- `JitoBundler` client for `mainnet.block-engine.jito.wtf/api/v1/bundles`.
- Random Jito tip account selection from the 8 canonical accounts.
- `simulateTransaction` helper for pre-flight validation.
- Dynamic tip sizing via `calculate_tip(expected_profit)`.

### Browser control panel

- Single-page app served by Axum at `http://localhost:3002`.
- Start/stop system buttons with toast notifications.
- Mode selector (paper/simulate/live) with confirmation dialog for live.
- Real-time event feed via `/ws/live`.
- Stats row, executions table, simulations table, forge signals table.
- Dark trading-terminal aesthetic.

## Tech stack

| Layer | Technology |
|-------|------------|
| Language | Rust (edition 2024) |
| Async runtime | tokio |
| HTTP client | reqwest |
| WebSocket | tokio-tungstenite |
| Database | PostgreSQL via sqlx |
| REST API | axum + tower-http |
| RPC provider | Helius (developer tier, Yellowstone gRPC ready) |
| Block engine | Jito mainnet |
| Wallet keypair | solana-sdk |
| Serialization | serde, bincode, bs58, base64 |
| CLI | clap |

## Workspace structure

```
crates/
  arb-types/      Shared types, DEX enum, token constants, DEX program IDs
  arb-feed/       DexScreener poller, forge WebSocket consumer, pool decoder,
                  pool monitor, discovery, RugCheck client, whale tracker
  arb-detect/     Swap analyzer, price graph, volume tracker
  arb-sim/        AmmQuoter, CostModel, LocalScanner, JitoBundler,
                  StaleReserveObserver, tx_builder, pool_cache
  arb-store/      Postgres migrations, sqlx queries, 7 tables
  arb-api/        Axum router, REST endpoints, static dashboard, live WebSocket
  arb-cli/        Pipeline wiring, discovery loops, graduation sniper,
                  periodic scanner, mode selection

migrations/
  001_schema.sql           price_snapshots, arb_opportunities, simulations
  002_swap_signals.sql     swap_signals
  003_token_safety.sql     token_safety
  004_executions.sql       executions
  005_observations.sql     stale_reserve_observations
```

## Getting started

### Prerequisites

- Rust stable (1.80+)
- PostgreSQL 14+
- A Helius API key on at least the Developer tier ($49/mo as of 2026). Free tier WebSocket is rate-limited to the point where `logsSubscribe` gets 429'd during peak hours.
- The sibling indexer [solana-forge](https://github.com/matthewrahm/solana-forge) running on port 3011 (or reachable by URL).

### Install

```bash
git clone https://github.com/matthewrahm/solana-arb.git
cd solana-arb
createdb solana_arb
cargo build --release
```

### Configure

Create `.env`:

| Variable | Required | Description |
|----------|----------|-------------|
| `HELIUS_API_KEY` | yes | Helius RPC/WebSocket API key |
| `DATABASE_URL` | no | Postgres connection string. Default `postgres://localhost/solana_arb` |
| `ARB_KEYPAIR_PATH` | only for simulate/live | Path to Solana keypair JSON |

### Run

```bash
# Terminal 1: start the indexer
cd ../solana-forge && cargo run --release -- --port 3011

# Terminal 2: start solana-arb in paper mode
cd solana-arb && cargo run --release -- \
  --port 3012 \
  --forge-url ws://localhost:3011/ws/events \
  --mode paper

# Open http://localhost:3012 in a browser
```

## CLI options

```
solana-arb [OPTIONS]

API configuration:
  -k, --api-key <KEY>         Helius API key (or HELIUS_API_KEY env)
      --database-url <URL>    Postgres URL (default postgres://localhost/solana_arb)
  -p, --port <PORT>           REST API port (default 3002)

Price monitoring:
      --poll-interval <SECS>  HTTP polling frequency (default 15s)
      --no-ws                 Disable WebSocket accountSubscribe

Discovery and filtering:
      --min-spread <BPS>      Minimum net spread to flag (default 10)
      --min-liquidity <USD>   Minimum pool liquidity (default 1000)
      --watch <MINTS>         Comma-separated token mints to monitor

Forge integration:
      --forge-url <URL>       WebSocket URL (default ws://localhost:3001/ws/events)
      --no-forge              Disable forge feed
      --min-signal-sol <SOL>  Minimum triggering swap size (default 1)

Execution:
      --mode <MODE>           paper (default), simulate, live
      --keypair <PATH>        Keypair file for simulate/live
```

## API endpoints

| Endpoint | Description |
|----------|-------------|
| `GET /api/v1/opportunities` | Recent cross-venue opportunities |
| `GET /api/v1/stats` | Aggregate opportunity statistics |
| `GET /api/v1/simulations` | Recent paper/simulate scan results |
| `GET /api/v1/signals` | Recent forge signals with trigger/profit flags |
| `GET /api/v1/executions` | Recent execution records |
| `GET /api/v1/status` | System status, signal counts, profit flags |
| `GET /api/v1/config` | Read/write runtime config |
| `POST /api/v1/system/start` | Start forge + pipeline |
| `POST /api/v1/system/stop` | Stop pipeline |
| `GET /ws/live` | Real-time event stream |

## Retrospective

This project was built across three phases over several weeks. Phase 1-5 implemented the original cross-venue arbitrage hypothesis. Phase 6 replaced the Jupiter simulation layer with direct AMM math. Phase A cleaned up the accounting. Phase B2 shipped the observation pipeline. Here are the honest findings.

### What worked

- **Direct AMM math is the right architecture.** Using Jupiter for simulation is circular: Jupiter is an aggregator, so quoting through it measures its own optimization that you cannot capture. The rewrite to local constant-product and sqrt_price math eliminated an entire class of false signals.
- **Forge-integrated signals are fast enough.** 32-79ms signal-to-scan is competitive with what a solo developer on a shared RPC tier can achieve. The bottleneck is `logsSubscribe` versus Yellowstone gRPC, not the arb engine.
- **The honest cost model changed everything.** Before Phase A the scanner reported net profits that evaporated in execution because it omitted the Jito tip and used a 2.5M lamport flat fee that understated priority costs by 3-5x on busy blocks. Recalibrating to 9M lamports fixed plus dynamic tip plus 50 bps penalty matched reality.
- **The observer pipeline is a reusable measurement harness.** Zero Helius credits, persists to Postgres, answers "does the market structure support this strategy" before committing to execution.

### What did not work

- **Cross-venue arbitrage on Solana micro-caps is structurally closed.** At a peak-hours test on 2026-04-14, 28 boosted tokens produced zero candidates with 2+ qualifying pools on different DEXs. Brand-new PumpFun tokens have one venue (the bonding curve), recent graduates live only on PumpSwap, tokens with 2+ real pools are already held in sync by Jupiter routing. The theoretical graduation-to-multi-venue arb window does not exist in practice.
- **DexScreener's liquidity-weighted aggregate is not a fair-price reference.** During the Phase B2 run, observations showed deltas from -1612 bps to +1833 bps on two PumpFun-graduated mid-cap tokens. Those deltas persisted for minutes across repeated signals, which is impossible if they represented real arbitrage windows on $4-5M liquidity pools. The contamination source is stale post-graduation bonding curves and illiquid secondary pools in the aggregate. Using DexScreener as a fair reference conflates post-graduation PumpFun ghost quotes with current Raydium execution prices.
- **Phase 6 persistence was broken for two days undetected.** Migration 004 introducing the `executions` table was authored but not wired into `run_migrations()`. Every `insert_execution` call silently failed because sqlx errors were `.ok()`-swallowed. During that window the new local-AMM pipeline ran but produced no observable output. This was diagnosed in Phase A and surfaced a principle: `.ok()` on database inserts hides correctness bugs. The fix was one-line, but the lesson is architectural.
- **The tx_builder layer is incomplete.** Only Raydium V4 has an instruction builder, and even that is missing the Serum market accounts (`bids`, `asks`, `event_queue`, `coin_vault`, `pc_vault`, `vault_signer`). PumpSwap, Orca, and PumpFun have program IDs defined but no builders. This means the simulate and live modes, while wired through the CLI, cannot actually submit anything. Completing the builders is 2-3 days of work, but it is blocked on the strategy decision, not the other way around.

### The realistic numbers

- **90M successful Solana arbs in 2025** per the Helius MEV report, at an average net profit of **$1.58/trade** after Jito tips (which absorb roughly 50-60% of gross edge on contested bundles).
- **Top bot (DeezNode/Vpe)** extracts approximately **$450K/day** but runs its own validator with 800K+ SOL delegated stake. Solo-dev sandwich is not viable.
- **Solo-dev back-running ceiling** per the research: $5-50/day gross on a shared RPC tier, $50-500/day on Helius Business plus Frankfurt colo (~$800-1200/mo infra floor). Roughly 10% of solo bots achieve sustained profitability.
- **Break-even trade size** under our honest cost model: ~$5K notional at 2-5% inefficiency, smaller on uncontested slots.

### Why I closed it

The project set out to produce a profitable simulation, and the infrastructure to produce one is now in place. What is missing is a strategy with enough real edge to clear the cost floor. Cross-venue micro-cap arb is structurally dead. Single-pool back-run without a reliable fair-price reference is fees-only. Graduation sniping has surface but competes with dedicated graduation bots that out-tip and out-latency a solo developer.

Pursuing the next phase would mean:

1. Swap `logsSubscribe` for Yellowstone gRPC on Helius Business. +$450/mo.
2. Build an uncontaminated fair-reference by querying two specific on-chain pools on different DEXs rather than DexScreener aggregate.
3. Complete the tx_builder suite across PumpSwap, Orca, and PumpFun.
4. Add dynamic Jito tip streaming via `bundles.jito.wtf/api/v1/bundles/tip_stream`.
5. Deploy to Frankfurt Hetzner for <30ms leader latency. +$100-200/mo.
6. Accept 2-3 months of break-even or worse while shadow-mode PnL tuning.
7. Compete with established searchers at every step.

That is a plausible roadmap but not the highest-leverage use of my time given other opportunities in newer ecosystems with less MEV saturation. Closing this as a completed investigation.

### What the code teaches

This repository is a fair example of:

- A 7-crate Rust workspace with clean boundaries (types, feed, detect, sim, store, api, cli)
- Async pipelines in tokio with backpressure via bounded channels
- Direct on-chain account decoding for five AMM variants
- Real AMM math (constant-product, virtual reserves, sqrt_price Q64.64)
- Jito Block Engine client implementation from first principles
- Postgres migrations and query layer with sqlx
- Embedded web UI served from Axum without a separate frontend build step
- WebSocket reconnect handling, rate-limit backoff, graceful degradation
- Honest cost modeling including the traps (tip double-counting, priority fee drift)
- Shadow-mode measurement as a strategy-validation primitive

## Related projects

- [solana-forge](https://github.com/matthewrahm/solana-forge) -- Rust Solana indexer, real-time swap/transfer decoding, Postgres, REST API. Sibling project that produces the swap stream solana-arb consumes.
- [solscope](https://github.com/matthewrahm/solscope) -- Rust TUI dashboard for Solana wallets. First Rust project in this portfolio trilogy.

## License

MIT
