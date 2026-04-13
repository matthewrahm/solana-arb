use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tracing::{error, info};

use arb_detect::detector::Detector;
use arb_types::{PriceQuote, BONK_MINT};

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

    // Connect to Postgres and run migrations
    info!("Connecting to database...");
    let pool = arb_store::create_pool(&database_url).await?;
    arb_store::run_migrations(&pool).await?;
    info!("Database ready");

    // Build watch list: always include BONK, plus any user-specified tokens
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

    // Resolve token symbols for display
    let dex_client = arb_feed::dexscreener::DexScreenerClient::new();
    let mut detector = Detector::new(args.min_spread, 30);

    for mint in &watch_mints {
        if mint == BONK_MINT {
            detector.register_symbol(mint, "BONK");
        } else if let Ok(Some(symbol)) = dex_client.get_token_symbol(mint).await {
            detector.register_symbol(mint, &symbol);
        }
    }

    // Channel for price quotes
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

    // Start price poller
    let poll_mints = watch_mints.clone();
    let poll_interval = args.poll_interval;
    let min_liquidity = args.min_liquidity;
    tokio::spawn(async move {
        if let Err(e) =
            arb_feed::poller::run_poll_loop(poll_mints, poll_interval, min_liquidity, quote_tx)
                .await
        {
            error!("Poller crashed: {}", e);
        }
    });

    // Main loop: consume price quotes, detect opportunities, store
    let store_pool = pool.clone();
    let mut opp_count: u64 = 0;
    let mut quote_count: u64 = 0;

    info!("Pipeline running. Polling every {}s...", args.poll_interval);
    info!("Min spread threshold: {} bps", args.min_spread);
    info!("API: http://localhost:{}/api/v1/opportunities", args.port);

    while let Some(quote) = quote_rx.recv().await {
        quote_count += 1;

        // Log every 10th price update to keep output readable
        if quote_count % 10 == 1 {
            info!(
                "PRICE [{:>8}] ${:.10} on {:>8} | {} venues tracked (liq: ${:.0})",
                &quote.base_mint[..8],
                quote.price_usd,
                quote.dex,
                quote_count,
                quote.liquidity_usd
            );
        }

        // Store price snapshot
        if let Err(e) = arb_store::queries::insert_price_snapshots(&store_pool, &[quote.clone()]).await {
            error!("DB write failed: {}", e);
        }

        // Run detection
        if let Some(opp) = detector.process(quote) {
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
        }
    }

    Ok(())
}
