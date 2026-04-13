use arb_types::PriceQuote;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// Composite key for price grouping: (base_mint, quote_mint)
/// Only pools trading the same pair are comparable.
type PairKey = (String, String);

/// In-memory price graph: tracks the latest price per DEX venue,
/// grouped by trading pair (base_mint + quote_mint).
pub struct PriceGraph {
    /// (base_mint, quote_mint) -> (dex_key -> PriceQuote)
    prices: HashMap<PairKey, HashMap<String, PriceQuote>>,
    /// Max age before a quote is considered stale
    max_age_secs: i64,
}

impl PriceGraph {
    pub fn new(max_age_secs: i64) -> Self {
        Self {
            prices: HashMap::new(),
            max_age_secs,
        }
    }

    /// Update the graph with a new price quote.
    /// Returns all current (non-stale) quotes for this trading pair.
    pub fn update(&mut self, quote: PriceQuote) -> Vec<&PriceQuote> {
        let pair_key = self.pair_key(&quote);
        let dex_key = self.dex_key(&quote);

        let venue_map = self.prices.entry(pair_key.clone()).or_default();
        venue_map.insert(dex_key, quote);

        self.get_pair_quotes(&pair_key)
    }

    /// Get all non-stale quotes for a specific trading pair
    fn get_pair_quotes(&self, pair_key: &PairKey) -> Vec<&PriceQuote> {
        let now = Utc::now();
        self.prices
            .get(pair_key)
            .map(|venues| {
                venues
                    .values()
                    .filter(|q| !self.is_stale(q, now))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get the previous price for the same venue (for delta detection)
    pub fn get_previous_price(&self, quote: &PriceQuote) -> Option<f64> {
        let pair_key = self.pair_key(quote);
        let dex_key = self.dex_key(quote);
        self.prices
            .get(&pair_key)
            .and_then(|venues| venues.get(&dex_key))
            .map(|q| q.price_usd)
    }

    fn pair_key(&self, quote: &PriceQuote) -> PairKey {
        // Group by (base_mint, quote_mint) so only same-pair pools are compared.
        // If quote_mint is empty (WebSocket feeds), use "unknown" to avoid
        // mixing with properly-labeled HTTP quotes.
        let qm = if quote.quote_mint.is_empty() {
            "unknown".to_string()
        } else {
            quote.quote_mint.clone()
        };
        (quote.base_mint.clone(), qm)
    }

    fn dex_key(&self, quote: &PriceQuote) -> String {
        match &quote.pool_address {
            Some(addr) => format!("{}:{}", quote.dex.as_str(), addr),
            None => quote.dex.as_str().to_string(),
        }
    }

    fn is_stale(&self, quote: &PriceQuote, now: DateTime<Utc>) -> bool {
        (now - quote.timestamp).num_seconds() > self.max_age_secs
    }
}
