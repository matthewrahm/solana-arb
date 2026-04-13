use arb_types::PriceQuote;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// In-memory price graph: tracks the latest price per DEX for each token.
/// Keyed by base_mint, each entry holds the latest quote per DEX venue.
pub struct PriceGraph {
    /// mint -> (dex_key -> PriceQuote)
    prices: HashMap<String, HashMap<String, PriceQuote>>,
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
    /// Returns all current (non-stale) quotes for this token.
    pub fn update(&mut self, quote: PriceQuote) -> Vec<&PriceQuote> {
        let mint = quote.base_mint.clone();
        let dex_key = self.dex_key(&quote);

        let venue_map = self.prices.entry(mint.clone()).or_default();
        venue_map.insert(dex_key, quote);

        self.get_quotes(&mint)
    }

    /// Get all non-stale quotes for a token
    pub fn get_quotes(&self, mint: &str) -> Vec<&PriceQuote> {
        let now = Utc::now();
        self.prices
            .get(mint)
            .map(|venues| {
                venues
                    .values()
                    .filter(|q| !self.is_stale(q, now))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn dex_key(&self, quote: &PriceQuote) -> String {
        // Unique key per DEX + pool to handle multiple pools on same DEX
        match &quote.pool_address {
            Some(addr) => format!("{}:{}", quote.dex.as_str(), addr),
            None => quote.dex.as_str().to_string(),
        }
    }

    fn is_stale(&self, quote: &PriceQuote, now: DateTime<Utc>) -> bool {
        (now - quote.timestamp).num_seconds() > self.max_age_secs
    }
}
