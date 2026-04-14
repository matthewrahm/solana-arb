use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};

use arb_detect::detector::Detector;
use arb_detect::swap_analyzer::SwapAnalyzer;
use arb_detect::volume_tracker::VolumeTracker;
use arb_types::{GraduationEvent, PriceQuote, SwapSignal, WSOL_MINT};
use chrono;
use uuid;

#[derive(Parser, Debug)]
#[command(
    name = "solana-arb",
    version,
    about = "Solana MEV/arbitrage detection engine"
)]
struct Args {
    /// Helius API key (or set HELIUS_API_KEY env var)
    #[arg(short = 'k', long)]
    api_key: Option<String>,

    /// Database URL (or set DATABASE_URL env var)
    #[arg(long)]
    database_url: Option<String>,

    /// API server port
    #[arg(short, long, default_value = "3002")]
    port: u16,

    /// Price poll interval in seconds (higher is fine when forge feed is active)
    #[arg(long, default_value = "15")]
    poll_interval: u64,

    /// Minimum net spread in bps to flag an opportunity
    #[arg(long, default_value = "10")]
    min_spread: f64,

    /// Minimum pool liquidity in USD to consider
    #[arg(long, default_value = "1000")]
    min_liquidity: f64,

    /// Additional token mints to watch (comma-separated)
    #[arg(long, value_delimiter = ',')]
    watch: Vec<String>,

    /// Disable WebSocket pool monitoring (HTTP polling only)
    #[arg(long)]
    no_ws: bool,

    /// Forge WebSocket URL for real-time swap stream
    #[arg(long, default_value = "ws://localhost:3001/ws/events")]
    forge_url: String,

    /// Disable forge feed (no swap-triggered scanning)
    #[arg(long)]
    no_forge: bool,

    /// Minimum SOL value for forge swap signals to trigger scans (lower = more micro-cap sensitive)
    #[arg(long, default_value = "2")]
    min_signal_sol: f64,

    /// Whale wallet addresses to prioritize (comma-separated)
    #[arg(long, value_delimiter = ',')]
    whale: Vec<String>,

    /// Path to whale wallets file (one address per line)
    #[arg(long)]
    whale_file: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "arb=info,solana_arb=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();

    let database_url = args
        .database_url
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .unwrap_or_else(|| "postgres://localhost/solana_arb".to_string());

    let helius_api_key = args
        .api_key
        .or_else(|| std::env::var("HELIUS_API_KEY").ok());

    // Connect to Postgres and run migrations
    info!("Connecting to database...");
    let pool = arb_store::create_pool(&database_url).await?;
    arb_store::run_migrations(&pool).await?;
    info!("Database ready");

    // Micro-cap focused: no default large-cap watchlist.
    // Tokens are discovered dynamically via DexScreener discovery + forge stream.
    let watch_mints: Vec<String> = args.watch.clone();
    let token_labels: Vec<(String, String)> = watch_mints.iter()
        .map(|m| (m.clone(), format!("{}..{}", &m[..4], &m[m.len()-4..])))
        .collect();

    // Shared list of discovered micro-cap tokens (discovery loop populates this)
    let discovered_tokens: Arc<RwLock<Vec<(String, String)>>> = Arc::new(RwLock::new(token_labels.clone()));

    if watch_mints.is_empty() {
        info!("Micro-cap mode: no static watchlist. Tokens discovered dynamically.");
    } else {
        info!("Watching {} user-specified tokens", watch_mints.len());
        for (mint, symbol) in &token_labels {
            info!("  {} ({})", symbol, &mint[..8]);
        }
    }

    // Register symbols in detector
    let mut detector = Detector::new(args.min_spread, 30);
    for (mint, symbol) in &token_labels {
        detector.register_symbol(mint, symbol);
    }

    // Channel for price quotes (shared between HTTP poller and WebSocket monitor)
    let (quote_tx, mut quote_rx) = mpsc::channel::<PriceQuote>(5000);

    // Start API server
    let api_pool = pool.clone();
    let api_port = args.port;
    tokio::spawn(async move {
        info!("API server on http://localhost:{}", api_port);
        let app = arb_api::build_router(api_pool);
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", api_port))
            .await
            .expect("Failed to bind API port");
        axum::serve(listener, app)
            .await
            .expect("API server crashed");
    });

    // Only start HTTP price poller if user specified tokens via --watch
    let min_liquidity = args.min_liquidity;
    if !watch_mints.is_empty() {
        let poll_tx = quote_tx.clone();
        let poll_mints = watch_mints.clone();
        let poll_interval = args.poll_interval;
        tokio::spawn(async move {
            if let Err(e) =
                arb_feed::poller::run_poll_loop(poll_mints, poll_interval, min_liquidity, poll_tx)
                    .await
            {
                error!("Poller crashed: {}", e);
            }
        });
        info!("HTTP poller enabled for {} user-specified tokens", watch_mints.len());
    } else {
        info!("HTTP poller disabled (micro-cap mode: forge + discovery only)");
    }

    // SOL/USD price tracker (shared across WebSocket monitor + simulator)
    let sol_usd_price = Arc::new(RwLock::new(0.0));

    // Background: update SOL/USD every 30s
    let sol_price_ref = sol_usd_price.clone();
    tokio::spawn(async move {
        let jup = arb_feed::jupiter::JupiterClient::new();
        loop {
            match jup.get_price(WSOL_MINT).await {
                Ok(price) if price > 0.0 => {
                    *sol_price_ref.write().await = price;
                    info!("SOL/USD: ${:.2}", price);
                }
                Ok(_) => {}
                Err(e) => error!("SOL/USD fetch failed: {}", e),
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // Wait for initial SOL price
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // Create simulator for paper trading (legacy Jupiter-based, kept for spread detection path)
    let simulator = arb_sim::Simulator::new(sol_usd_price.clone());

    // Local scanner: direct AMM math, no Jupiter dependency
    let rpc_url = helius_api_key
        .as_ref()
        .map(|k| format!("https://mainnet.helius-rpc.com/?api-key={k}"))
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());
    let local_scanner = arb_sim::LocalScanner::new(&rpc_url, sol_usd_price.clone());

    // Start WebSocket pool monitor (only if API key provided, not disabled, AND we have tokens to watch)
    // In micro-cap mode with no static watchlist, skip this -- forge feed handles real-time data
    if let Some(ref api_key) = helius_api_key {
        if !args.no_ws && !watch_mints.is_empty() {

            // Graduation event channel
            let (grad_tx, mut grad_rx) = mpsc::channel::<GraduationEvent>(100);

            // Start WebSocket pool monitor
            let ws_tx = quote_tx.clone();
            let ws_mints = watch_mints.clone();
            let ws_key = api_key.clone();
            let ws_sol_price = sol_usd_price.clone();
            let ws_min_liq = min_liquidity;
            tokio::spawn(async move {
                let monitor = arb_feed::pool_monitor::PoolMonitor::new(
                    &ws_key, ws_tx, grad_tx, ws_sol_price,
                );
                loop {
                    if let Err(e) = monitor.run(ws_mints.clone(), ws_min_liq).await {
                        error!("WebSocket monitor error: {}. Restarting in 5s...", e);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            });

            // Graduation handler with sniper
            let grad_scanner = arb_sim::ProfitScanner::new(sol_usd_price.clone());
            let grad_store = pool.clone();
            tokio::spawn(async move {
                while let Some(event) = grad_rx.recv().await {
                    info!(
                        "GRADUATION: {} (curve {}) at {} -- LAUNCHING SNIPER",
                        &event.base_mint[..8],
                        &event.bonding_curve_address[..8],
                        event.graduated_at
                    );

                    let sniper = grad_scanner.clone();
                    let mint = event.base_mint.clone();
                    let symbol = if event.token_symbol.is_empty() {
                        format!("{}..{}", &event.base_mint[..4], &event.base_mint[event.base_mint.len()-4..])
                    } else {
                        event.token_symbol.clone()
                    };
                    let snipe_pool = grad_store.clone();

                    tokio::spawn(async move {
                        let results = sniper.snipe_graduation(&mint, &symbol).await;
                        for r in results {
                            if r.profitable {
                                let sim = r.to_sim_result();
                                arb_store::queries::insert_simulation(&snipe_pool, &sim).await.ok();
                            }
                        }
                    });
                }
            });

            info!("WebSocket pool monitor enabled");
        }
    } else {
        info!("No HELIUS_API_KEY provided, running HTTP polling only");
    }

    // Periodic scanner: scans discovered micro-cap tokens (populated by discovery loop)
    let scanner = arb_sim::ProfitScanner::new(sol_usd_price.clone());
    let scan_pool = pool.clone();
    let scan_discovered = discovered_tokens.clone();
    tokio::spawn(async move {
        // Wait for discovery to populate some tokens
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;

        let mut scan_cycle: u64 = 0;
        loop {
            let tokens = scan_discovered.read().await.clone();
            if tokens.is_empty() {
                info!("SCAN: no discovered tokens yet, waiting...");
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }

            scan_cycle += 1;
            info!("--- SCAN CYCLE #{} ({} micro-cap tokens) ---", scan_cycle, tokens.len());

            let mut profitable_count = 0;
            let mut total_count = 0;
            for (mint, symbol) in &tokens {
                match scanner.scan_round_trip(mint, symbol).await {
                    Ok(r) => {
                        total_count += 1;
                        let sol_usd = *scanner.sol_usd_price.read().await;
                        let net_sol = r.net_profit_lamports as f64 / 1e9;
                        let net_usd = net_sol * sol_usd;

                        if r.profitable {
                            profitable_count += 1;
                            info!(
                                "SCAN *** PROFIT *** {} | {:.4} SOL -> {:.4} SOL | {:.1} bps | +{:.6} SOL (${:.4}) | {}",
                                r.token_symbol,
                                r.input_lamports as f64 / 1e9,
                                r.output_lamports as f64 / 1e9,
                                r.profit_bps,
                                net_sol,
                                net_usd,
                                r.route_description,
                            );
                        }

                        // Store all scan results (profitable and not) for dashboard visibility
                        let sim = r.to_sim_result();
                        arb_store::queries::insert_simulation(&scan_pool, &sim).await.ok();
                    }
                    Err(e) => warn!("Scan failed for {}: {}", symbol, e),
                }
            }

            info!(
                "--- SCAN CYCLE #{} complete: {}/{} profitable ---",
                scan_cycle, profitable_count, total_count
            );

            // Scan every 3 minutes (API budget shared with forge-triggered scans)
            tokio::time::sleep(std::time::Duration::from_secs(180)).await;
        }
    });

    // Micro-cap token discovery loop
    // Discovers new tokens, runs safety checks, adds safe ones to the shared scan list
    let disc_scanner = arb_sim::ProfitScanner::new(sol_usd_price.clone());
    let disc_store = pool.clone();
    let disc_rpc = helius_api_key.as_ref().map(|k| format!("https://mainnet.helius-rpc.com/?api-key={k}"));
    let disc_known: std::collections::HashSet<String> = watch_mints.iter().cloned().collect();
    let disc_discovered = discovered_tokens.clone();
    tokio::spawn(async move {
        // Wait for system to stabilize
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;

        let mut known = disc_known;
        loop {
            info!("--- DISCOVERY: scanning for new micro-cap tokens ---");

            match arb_feed::discovery::discover_new_tokens(
                &known,
                5000.0,  // min $5K liquidity (too low = scam, too high = no edge)
                disc_rpc.as_deref(),
            ).await {
                Ok(tokens) => {
                    // Filter: safe + liquidity under $200K (micro-cap sweet spot)
                    let safe_tokens: Vec<_> = tokens.into_iter()
                        .filter(|t| t.safe && t.liquidity_usd < 200_000.0)
                        .collect();

                    info!("DISCOVERY: {} safe micro-cap tokens found", safe_tokens.len());

                    for token in &safe_tokens {
                        known.insert(token.mint.clone());

                        // Add to shared discovered list for periodic scanner
                        {
                            let mut list = disc_discovered.write().await;
                            if !list.iter().any(|(m, _)| m == &token.mint) {
                                list.push((token.mint.clone(), token.symbol.clone()));
                                // Cap at 50 tokens to keep scan cycles manageable
                                if list.len() > 50 {
                                    list.remove(0); // drop oldest
                                }
                            }
                        }

                        // Initial round-trip scan on discovery
                        match disc_scanner.scan_round_trip(&token.mint, &token.symbol).await {
                            Ok(r) => {
                                let sol_usd = *disc_scanner.sol_usd_price.read().await;
                                let net_sol = r.net_profit_lamports as f64 / 1e9;

                                if r.profitable {
                                    info!(
                                        "DISCOVERY *** PROFIT *** {} | {:.1} bps | +{:.6} SOL (${:.4}) | liq: ${:.0} | {}",
                                        r.token_symbol, r.profit_bps, net_sol, net_sol * sol_usd,
                                        token.liquidity_usd, r.route_description,
                                    );
                                }

                                // Store all discovery scans for dashboard visibility
                                let sim = r.to_sim_result();
                                arb_store::queries::insert_simulation(&disc_store, &sim).await.ok();
                            }
                            Err(e) => {
                                warn!("Discovery scan failed for {}: {}", token.symbol, e);
                            }
                        }
                    }
                }
                Err(e) => warn!("Discovery loop error: {}", e),
            }

            // Run discovery every 3 minutes (core pipeline for finding new opportunities)
            tokio::time::sleep(std::time::Duration::from_secs(180)).await;
        }
    });

    // Forge feed: real-time swap stream from solana-forge
    let (signal_tx, mut signal_rx) = mpsc::channel::<SwapSignal>(1000);

    if !args.no_forge {
        let forge_url = args.forge_url.clone();
        let forge_signal_tx = signal_tx.clone();
        tokio::spawn(async move {
            loop {
                match arb_feed::forge_feed::run_forge_feed(&forge_url, forge_signal_tx.clone()).await {
                    Ok(()) => info!("Forge feed ended, reconnecting in 5s..."),
                    Err(e) => error!("Forge feed error: {}. Reconnecting in 5s...", e),
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
        info!("Forge feed enabled ({})", args.forge_url);
    } else {
        info!("Forge feed disabled");
    }
    drop(signal_tx); // drop original so channel closes when forge task's clone drops

    // Swap analyzer for forge signals
    let swap_analyzer = SwapAnalyzer::new(args.min_signal_sol);

    // Whale tracker for prioritizing known profitable wallets
    let mut whale_tracker = if let Some(ref path) = args.whale_file {
        arb_feed::whale_tracker::WhaleTracker::load_from_file(std::path::Path::new(path))
    } else {
        arb_feed::whale_tracker::WhaleTracker::new()
    };
    whale_tracker.add_wallets(&args.whale);
    if whale_tracker.wallet_count() > 0 {
        info!("Tracking {} whale wallets", whale_tracker.wallet_count());
    }

    // Volume tracker for detecting activity spikes
    let mut volume_tracker = VolumeTracker::new();

    // Drop the original sender so channel closes when all senders are dropped
    drop(quote_tx);

    // Main loop: consume price quotes AND forge swap signals
    let store_pool = pool.clone();
    let mut opp_count: u64 = 0;
    let mut quote_count: u64 = 0;
    let mut ws_quote_count: u64 = 0;
    let mut signal_count: u64 = 0;
    let mut triggered_count: u64 = 0;
    let mut quotes_alive = true; // false once all quote senders drop

    info!("Pipeline running. Polling every {}s...", args.poll_interval);
    info!("Min spread threshold: {} bps", args.min_spread);
    info!("Min signal size: {} SOL", args.min_signal_sol);
    info!("API: http://localhost:{}/api/v1/opportunities", args.port);

    loop {
        let quote = tokio::select! {
            q = quote_rx.recv(), if quotes_alive => {
                match q {
                    Some(q) => Some(q),
                    None => {
                        quotes_alive = false;
                        info!("Quote channel closed, running on forge signals + discovery only");
                        continue;
                    }
                }
            }
            sig = signal_rx.recv() => {
                if let Some(signal) = sig {
                    signal_count += 1;

                    let is_whale = whale_tracker.is_whale(&signal.signer);
                    let whale_tag = if is_whale { " [WHALE]" } else { "" };

                    info!(
                        "FORGE [{:>5}] {} {:.1} SOL {} on {} [{}]{}",
                        signal_count,
                        signal.direction,
                        signal.sol_equivalent,
                        signal.token_symbol.as_deref().unwrap_or("?"),
                        signal.platform,
                        &signal.signature[..8.min(signal.signature.len())],
                        whale_tag,
                    );

                    // Track volume for spike detection (flag only, no independent scan)
                    volume_tracker.record_swap(&signal.token_mint);

                    // Periodic cleanup (every ~1000 signals)
                    if signal_count % 1000 == 0 {
                        volume_tracker.cleanup();
                    }

                    // Analyze signal and trigger scan if actionable
                    if let Some(req) = swap_analyzer.analyze(&signal) {
                        triggered_count += 1;
                        let symbol = req.token_symbol.clone()
                            .unwrap_or_else(|| format!("{}..{}", &req.token_mint[..4], &req.token_mint[req.token_mint.len()-4..]));
                        let trig_local = local_scanner.clone();
                        let trig_pool = store_pool.clone();
                        let trig_signal = signal.clone();

                        let signal_sol = req.sol_equivalent;
                        tokio::spawn(async move {
                            // Primary: local AMM math (no Jupiter)
                            match trig_local.scan_triggered(
                                &req.token_mint,
                                &symbol,
                                req.trigger_dex,
                                req.trigger_direction,
                                signal_sol,
                            ).await {
                                Ok(r) => {
                                    let profitable = r.profitable;
                                    let sol_usd = *trig_local.sol_usd_price.read().await;
                                    let net_sol = r.net_profit_lamports as f64 / 1e9;

                                    if profitable {
                                        info!(
                                            "LOCAL PROFIT {} | buy {} sell {} | +{:.6} SOL (${:.4}) | {:.1} bps",
                                            symbol, r.buy_pool.dex, r.sell_pool.dex,
                                            net_sol, net_sol * sol_usd, r.profit_bps,
                                        );
                                    }

                                    // Store signal + result
                                    arb_store::queries::insert_signal(
                                        &trig_pool, &trig_signal, true, Some(profitable),
                                    ).await.ok();

                                    // Convert CrossVenueResult to SimResult for DB storage
                                    let sim = arb_types::SimResult {
                                        id: uuid::Uuid::new_v4(),
                                        opportunity_id: uuid::Uuid::nil(),
                                        input_amount: r.input_lamports as i64,
                                        input_mint: arb_types::WSOL_MINT.to_string(),
                                        simulated_output: Some(r.output_lamports as i64),
                                        output_mint: arb_types::WSOL_MINT.to_string(),
                                        simulated_profit_lamports: Some(r.net_profit_lamports),
                                        tx_fee_lamports: Some(2_500_000),
                                        priority_fee_lamports: Some(2_000_000),
                                        simulation_success: true,
                                        error_message: None,
                                        simulated_at: chrono::Utc::now(),
                                    };
                                    arb_store::queries::insert_simulation(&trig_pool, &sim).await.ok();
                                }
                                Err(e) => {
                                    warn!("Local scan failed for {}: {}", symbol, e);
                                    arb_store::queries::insert_signal(
                                        &trig_pool, &trig_signal, true, None,
                                    ).await.ok();
                                }
                            }
                        });
                    } else {
                        // Signal received but not actionable -- store for dashboard
                        info!("STORING signal {} (not actionable)", &signal.signature[..8.min(signal.signature.len())]);
                        if let Err(e) = arb_store::queries::insert_signal(
                            &store_pool, &signal, false, None,
                        ).await {
                            error!("DB insert_signal failed: {}", e);
                        }
                    }
                }
                continue; // back to select!
            }
        };

        let quote = match quote {
            Some(q) => q,
            None => break,
        };
        quote_count += 1;

        let is_ws = quote.source == arb_types::PriceSource::WebSocket;
        if is_ws {
            ws_quote_count += 1;
        }

        // Log prices: always log WebSocket quotes (they're the interesting ones)
        if is_ws || quote_count % 10 == 1 {
            let src = if is_ws { "WS" } else { "HTTP" };
            info!(
                "PRICE [{:>4}] [{:>8}] ${:.10} on {:>8} (liq: ${:.0})",
                src,
                &quote.base_mint[..8],
                quote.price_usd,
                quote.dex,
                quote.liquidity_usd
            );
        }

        // Store price snapshot
        if let Err(e) = arb_store::queries::insert_price_snapshots(&store_pool, &[quote.clone()]).await {
            error!("DB write failed: {}", e);
        }

        // Run detection
        let (opp, delta) = detector.process(quote);

        // Log significant price moves (delta detection)
        // On WebSocket deltas, immediately run a round-trip profitability check
        if let Some(d) = delta {
            let direction = if d.delta_bps > 0.0 { "UP" } else { "DOWN" };
            info!(
                "DELTA {} {:.1} bps on {} {:>8} | ${:.10} -> ${:.10}",
                direction,
                d.delta_bps.abs(),
                d.dex,
                &d.base_mint[..8],
                d.old_price,
                d.new_price,
            );

            // Delta-triggered scanning removed: forge-triggered scanner handles
            // real-time signals better with DEX-restricted routing on both legs.
        }

        if let Some(opp) = opp {
            opp_count += 1;

            info!(
                "ARB #{} {} | BUY {} @ ${:.8} | SELL {} @ ${:.8} | spread {:.1} bps (net {:.1}) | est profit ${:.4}",
                opp_count,
                opp.token_symbol,
                opp.buy_dex,
                opp.buy_price,
                opp.sell_dex,
                opp.sell_price,
                opp.gross_spread_bps,
                opp.net_spread_bps,
                opp.estimated_profit_usd,
            );

            if let Err(e) = arb_store::queries::insert_opportunity(&store_pool, &opp).await {
                error!("DB write failed (opp): {}", e);
            }

            // Simulate in background (don't block the price feed loop)
            let sim = simulator.clone();
            let sim_pool = store_pool.clone();
            let sim_sol_usd = sol_usd_price.clone();
            let sim_opp = opp.clone();
            tokio::spawn(async move {
                match sim.simulate(&sim_opp).await {
                    Ok(result) => {
                        if result.simulation_success {
                            let profit_sol = result.simulated_profit_lamports
                                .map(|p| p as f64 / 1e9)
                                .unwrap_or(0.0);
                            let sol_usd = *sim_sol_usd.read().await;
                            let profit_usd = profit_sol * sol_usd;
                            let tag = if profit_sol > 0.0 { "PROFIT" } else { "LOSS" };
                            info!(
                                "SIM {} {} | {:.4} SOL in -> {:.4} SOL out | {:.6} SOL (${:.4})",
                                tag,
                                sim_opp.token_symbol,
                                result.input_amount as f64 / 1e9,
                                result.simulated_output.unwrap_or(0) as f64 / 1e9,
                                profit_sol,
                                profit_usd,
                            );
                        } else {
                            info!(
                                "SIM FAIL {} | {}",
                                sim_opp.token_symbol,
                                result.error_message.as_deref().unwrap_or("unknown error"),
                            );
                        }
                        if let Err(e) = arb_store::queries::insert_simulation(&sim_pool, &result).await {
                            error!("DB write failed (sim): {}", e);
                        }
                    }
                    Err(e) => {
                        error!("Simulation error for {}: {}", sim_opp.token_symbol, e);
                    }
                }
            });
        }

        // Periodic stats
        if quote_count % 100 == 0 {
            info!(
                "Stats: {} quotes ({} WS, {} HTTP), {} opportunities, {} signals ({} triggered)",
                quote_count,
                ws_quote_count,
                quote_count - ws_quote_count,
                opp_count,
                signal_count,
                triggered_count,
            );
        }
    }

    Ok(())
}
