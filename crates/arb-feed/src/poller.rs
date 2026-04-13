use anyhow::Result;
use arb_types::PriceQuote;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::dexscreener::DexScreenerClient;

/// Polls DexScreener for all pairs of watched tokens on a fixed interval.
/// Sends PriceQuotes into the channel for the detection engine.
pub async fn run_poll_loop(
    watch_mints: Vec<String>,
    interval_secs: u64,
    min_liquidity: f64,
    tx: mpsc::Sender<PriceQuote>,
) -> Result<()> {
    let client = DexScreenerClient::new();
    let interval = std::time::Duration::from_secs(interval_secs);

    info!(
        "Price poller started: {} tokens, {}s interval, min liquidity ${}",
        watch_mints.len(),
        interval_secs,
        min_liquidity
    );

    loop {
        for mint in &watch_mints {
            match client.get_all_pairs(mint, min_liquidity).await {
                Ok(quotes) => {
                    info!(
                        "Polled {} pairs for {}",
                        quotes.len(),
                        &mint[..8]
                    );
                    for quote in quotes {
                        if tx.send(quote).await.is_err() {
                            info!("Channel closed, stopping poller");
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    error!("DexScreener poll failed for {}: {}", &mint[..8], e);
                }
            }
        }

        tokio::time::sleep(interval).await;
    }
}
