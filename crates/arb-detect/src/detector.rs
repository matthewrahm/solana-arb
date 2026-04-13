use arb_types::{ArbOpportunity, PriceQuote};
use chrono::Utc;
use uuid::Uuid;

use crate::graph::PriceGraph;

pub struct Detector {
    graph: PriceGraph,
    min_spread_bps: f64,
    /// Cache token symbols for logging (mint -> symbol)
    symbols: std::collections::HashMap<String, String>,
}

impl Detector {
    pub fn new(min_spread_bps: f64, max_quote_age_secs: i64) -> Self {
        Self {
            graph: PriceGraph::new(max_quote_age_secs),
            min_spread_bps,
            symbols: std::collections::HashMap::new(),
        }
    }

    /// Register a symbol for a token mint (for display purposes)
    pub fn register_symbol(&mut self, mint: &str, symbol: &str) {
        self.symbols.insert(mint.to_string(), symbol.to_string());
    }

    /// Process an incoming price quote. Returns an ArbOpportunity if a
    /// profitable spread is detected across venues.
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

        // Estimated round-trip fees: buy-side fee + sell-side fee
        let estimated_fees_bps = buy.dex.fee_bps() + sell.dex.fee_bps();

        let net_spread_bps = gross_spread_bps - estimated_fees_bps;

        if net_spread_bps < self.min_spread_bps {
            return None;
        }

        // Estimate profit on a $1000 trade
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
