use arb_types::{ArbOpportunity, Dex, PriceQuote};
use chrono::Utc;
use std::collections::HashMap;
use uuid::Uuid;

use crate::graph::PriceGraph;

/// Tracks the last emitted spread to avoid duplicate opportunities
struct LastSpread {
    buy_dex: Dex,
    sell_dex: Dex,
    net_spread_bps: f64,
}

pub struct Detector {
    graph: PriceGraph,
    min_spread_bps: f64,
    /// Only emit a new opportunity if the spread changed by this many bps
    dedup_threshold_bps: f64,
    /// Last emitted spread per token mint
    last_spreads: HashMap<String, LastSpread>,
    /// Cache token symbols for logging (mint -> symbol)
    symbols: HashMap<String, String>,
}

impl Detector {
    pub fn new(min_spread_bps: f64, max_quote_age_secs: i64) -> Self {
        Self {
            graph: PriceGraph::new(max_quote_age_secs),
            min_spread_bps,
            dedup_threshold_bps: 5.0,
            last_spreads: HashMap::new(),
            symbols: HashMap::new(),
        }
    }

    /// Register a symbol for a token mint (for display purposes)
    pub fn register_symbol(&mut self, mint: &str, symbol: &str) {
        self.symbols.insert(mint.to_string(), symbol.to_string());
    }

    /// Process an incoming price quote. Returns an ArbOpportunity if a
    /// new profitable spread is detected across venues.
    pub fn process(&mut self, quote: PriceQuote) -> Option<ArbOpportunity> {
        let mint = quote.base_mint.clone();
        let received_at = Utc::now();

        let quotes = self.graph.update(quote);

        // Need at least 2 venues to detect a spread
        if quotes.len() < 2 {
            return None;
        }

        // Find cheapest and most expensive venue
        let mut cheapest: Option<&PriceQuote> = None;
        let mut priciest: Option<&PriceQuote> = None;

        for q in &quotes {
            if cheapest.is_none() || q.price_usd < cheapest.unwrap().price_usd {
                cheapest = Some(q);
            }
            if priciest.is_none() || q.price_usd > priciest.unwrap().price_usd {
                priciest = Some(q);
            }
        }

        let buy = cheapest?;
        let sell = priciest?;

        // Don't compare a venue against itself
        if buy.dex == sell.dex && buy.pool_address == sell.pool_address {
            return None;
        }

        let gross_spread_bps = (sell.price_usd - buy.price_usd) / buy.price_usd * 10_000.0;
        let estimated_fees_bps = buy.dex.fee_bps() + sell.dex.fee_bps();
        let net_spread_bps = gross_spread_bps - estimated_fees_bps;

        if net_spread_bps < self.min_spread_bps {
            return None;
        }

        // Deduplicate: only emit if the spread changed meaningfully
        // (different venues, or spread shifted by more than threshold)
        if let Some(last) = self.last_spreads.get(&mint) {
            let same_venues = last.buy_dex == buy.dex && last.sell_dex == sell.dex;
            let spread_delta = (net_spread_bps - last.net_spread_bps).abs();
            if same_venues && spread_delta < self.dedup_threshold_bps {
                return None;
            }
        }

        // Record this spread as the latest
        self.last_spreads.insert(
            mint.clone(),
            LastSpread {
                buy_dex: buy.dex,
                sell_dex: sell.dex,
                net_spread_bps,
            },
        );

        let trade_size_usd = 1000.0;
        let estimated_profit_usd = trade_size_usd * (net_spread_bps / 10_000.0);

        let detection_latency_ms = (received_at - buy.timestamp.max(sell.timestamp))
            .num_milliseconds()
            .unsigned_abs();

        let symbol = self
            .symbols
            .get(&mint)
            .cloned()
            .unwrap_or_else(|| format!("{}..{}", &mint[..4], &mint[mint.len() - 4..]));

        Some(ArbOpportunity {
            id: Uuid::new_v4(),
            base_mint: mint,
            token_symbol: symbol,
            buy_dex: buy.dex,
            buy_price: buy.price_usd,
            buy_pool: buy.pool_address.clone(),
            sell_dex: sell.dex,
            sell_price: sell.price_usd,
            sell_pool: sell.pool_address.clone(),
            gross_spread_bps,
            estimated_fees_bps,
            net_spread_bps,
            estimated_profit_usd,
            detected_at: received_at,
            detection_latency_ms,
        })
    }
}
