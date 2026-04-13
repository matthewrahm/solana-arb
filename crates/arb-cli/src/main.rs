use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info};

use arb_detect::detector::Detector;
use arb_types::{GraduationEvent, PriceQuote, BONK_MINT, WSOL_MINT};

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

    /// Price poll interval in seconds
    #[arg(long, default_value = "5")]
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

    // Build watch list
    let mut watch_mints = vec![BONK_MINT.to_string()];
    for mint in &args.watch {
        if !watch_mints.contains(mint) {
            watch_mints.push(mint.clone());
        }
    }

    info!("Watching {} tokens", watch_mints.len());
    for mint in &watch_mints {
        info!("  {}", mint);
    }

    // Resolve token symbols
    let dex_client = arb_feed::dexscreener::DexScreenerClient::new();
    let mut detector = Detector::new(args.min_spread, 30);

    for mint in &watch_mints {
        if mint == BONK_MINT {
            detector.register_symbol(mint, "BONK");
        } else if let Ok(Some(symbol)) = dex_client.get_token_symbol(mint).await {
            detector.register_symbol(mint, &symbol);
        }
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

    // Start HTTP price poller (clone tx before moving to poller)
    let poll_tx = quote_tx.clone();
    let poll_mints = watch_mints.clone();
    let poll_interval = args.poll_interval;
    let min_liquidity = args.min_liquidity;
    tokio::spawn(async move {
        if let Err(e) =
            arb_feed::poller::run_poll_loop(poll_mints, poll_interval, min_liquidity, poll_tx)
                .await
        {
            error!("Poller crashed: {}", e);
        }
    });

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

    // Create simulator for paper trading
    let simulator = arb_sim::Simulator::new(sol_usd_price.clone());

    // Start WebSocket pool monitor (if API key provided and not disabled)
    if let Some(ref api_key) = helius_api_key {
        if !args.no_ws {

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

            // Graduation handler
            tokio::spawn(async move {
                while let Some(event) = grad_rx.recv().await {
                    info!(
                        "GRADUATION: {} (curve {}) at {}",
                        &event.base_mint[..8],
                        &event.bonding_curve_address[..8],
                        event.graduated_at
                    );
                }
            });

            info!("WebSocket pool monitor enabled");
        }
    } else {
        info!("No HELIUS_API_KEY provided, running HTTP polling only");
    }

    // Drop the original sender so channel closes when all senders are dropped
    drop(quote_tx);

    // Main loop: consume price quotes, detect opportunities, store
    let store_pool = pool.clone();
    let mut opp_count: u64 = 0;
    let mut quote_count: u64 = 0;
    let mut ws_quote_count: u64 = 0;

    info!("Pipeline running. Polling every {}s...", args.poll_interval);
    info!("Min spread threshold: {} bps", args.min_spread);
    info!("API: http://localhost:{}/api/v1/opportunities", args.port);

    while let Some(quote) = quote_rx.recv().await {
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
                "Stats: {} quotes ({} WS, {} HTTP), {} opportunities",
                quote_count,
                ws_quote_count,
                quote_count - ws_quote_count,
                opp_count
            );
        }
    }

    Ok(())
}
