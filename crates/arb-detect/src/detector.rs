use arb_types::{ArbOpportunity, Dex, PriceQuote, PriceSource};
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

/// Tracks a significant price move on a single venue (for delta detection)
#[derive(Debug, Clone)]
pub struct PriceDelta {
    pub dex: Dex,
    pub pool_address: Option<String>,
    pub base_mint: String,
    pub old_price: f64,
    pub new_price: f64,
    pub delta_bps: f64,
    pub source: PriceSource,
}

pub struct Detector {
    graph: PriceGraph,
    min_spread_bps: f64,
    /// Minimum price move (bps) on a single venue to flag as a delta event
    delta_threshold_bps: f64,
    /// Only emit a new opportunity if the spread changed by this many bps
    dedup_threshold_bps: f64,
    /// Last emitted spread per pair key
    last_spreads: HashMap<String, LastSpread>,
    /// Cache token symbols for logging (mint -> symbol)
    pub symbols: HashMap<String, String>,
}

impl Detector {
    pub fn new(min_spread_bps: f64, max_quote_age_secs: i64) -> Self {
        Self {
            graph: PriceGraph::new(max_quote_age_secs),
            min_spread_bps,
            delta_threshold_bps: 20.0, // 20 bps = 0.2% move flags a delta
            dedup_threshold_bps: 5.0,
            last_spreads: HashMap::new(),
            symbols: HashMap::new(),
        }
    }

    pub fn register_symbol(&mut self, mint: &str, symbol: &str) {
        self.symbols.insert(mint.to_string(), symbol.to_string());
    }

    /// Process an incoming price quote.
    /// Returns (Option<ArbOpportunity>, Option<PriceDelta>).
    /// - ArbOpportunity: cross-venue spread detected for same trading pair
    /// - PriceDelta: significant price move on a single venue (potential trigger)
    pub fn process(&mut self, quote: PriceQuote) -> (Option<ArbOpportunity>, Option<PriceDelta>) {
        let mint = quote.base_mint.clone();
        let received_at = Utc::now();

        // Delta detection: check if this quote represents a significant price move
        let delta = self.detect_delta(&quote);

        // Update graph and get all quotes for this pair
        let quotes = self.graph.update(quote);

        // Need at least 2 venues to detect a spread
        if quotes.len() < 2 {
            return (None, delta);
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

        let buy = match cheapest {
            Some(q) => q,
            None => return (None, delta),
        };
        let sell = match priciest {
            Some(q) => q,
            None => return (None, delta),
        };

        // Don't compare a venue against itself
        if buy.dex == sell.dex && buy.pool_address == sell.pool_address {
            return (None, delta);
        }

        let gross_spread_bps = (sell.price_usd - buy.price_usd) / buy.price_usd * 10_000.0;
        let estimated_fees_bps = buy.dex.fee_bps() + sell.dex.fee_bps();
        let net_spread_bps = gross_spread_bps - estimated_fees_bps;

        if net_spread_bps < self.min_spread_bps {
            return (None, delta);
        }

        // Dedup key includes quote_mint so different pairs don't interfere
        let dedup_key = format!("{}:{}", &mint, buy.quote_mint);

        if let Some(last) = self.last_spreads.get(&dedup_key) {
            let same_venues = last.buy_dex == buy.dex && last.sell_dex == sell.dex;
            let spread_delta = (net_spread_bps - last.net_spread_bps).abs();
            if same_venues && spread_delta < self.dedup_threshold_bps {
                return (None, delta);
            }
        }

        self.last_spreads.insert(
            dedup_key,
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

        let opp = ArbOpportunity {
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
        };

        (Some(opp), delta)
    }

    /// Detect if a price quote represents a significant move from the previous price
    fn detect_delta(&self, quote: &PriceQuote) -> Option<PriceDelta> {
        let old_price = self.graph.get_previous_price(quote)?;
        if old_price <= 0.0 {
            return None;
        }

        let delta_bps = (quote.price_usd - old_price) / old_price * 10_000.0;

        if delta_bps.abs() >= self.delta_threshold_bps {
            Some(PriceDelta {
                dex: quote.dex,
                pool_address: quote.pool_address.clone(),
                base_mint: quote.base_mint.clone(),
                old_price,
                new_price: quote.price_usd,
                delta_bps,
                source: quote.source,
            })
        } else {
            None
        }
    }
}
