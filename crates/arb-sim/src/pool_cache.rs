//! Shared pool state cache for instant quotes without RPC calls.
//! Fed by WebSocket pool monitor updates and RPC fetches.
//! AmmQuoter checks this cache before making RPC calls.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;
use tracing::debug;

/// Cached reserves for a single pool.
#[derive(Debug, Clone)]
pub struct CachedReserves {
    pub pool_address: String,
    pub reserve_a: u64, // coin/token side
    pub reserve_b: u64, // pc/SOL side
    pub updated_at: Instant,
}

impl CachedReserves {
    /// Returns true if the cache entry is fresher than `max_age_ms`.
    pub fn is_fresh(&self, max_age_ms: u64) -> bool {
        self.updated_at.elapsed().as_millis() < max_age_ms as u128
    }
}

/// Thread-safe pool state cache keyed by pool_address.
pub type PoolStateCache = Arc<RwLock<HashMap<String, CachedReserves>>>;

/// Create a new empty cache.
pub fn new_pool_cache() -> PoolStateCache {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Update a pool's cached reserves.
pub async fn update_cache(
    cache: &PoolStateCache,
    pool_address: &str,
    reserve_a: u64,
    reserve_b: u64,
) {
    let mut map = cache.write().await;
    map.insert(
        pool_address.to_string(),
        CachedReserves {
            pool_address: pool_address.to_string(),
            reserve_a,
            reserve_b,
            updated_at: Instant::now(),
        },
    );
    debug!(
        "CACHE updated: pool={} reserves=({}, {})",
        &pool_address[..8.min(pool_address.len())],
        reserve_a,
        reserve_b
    );
}

/// Get cached reserves if fresh enough (within max_age_ms).
pub async fn get_cached(
    cache: &PoolStateCache,
    pool_address: &str,
    max_age_ms: u64,
) -> Option<CachedReserves> {
    let map = cache.read().await;
    map.get(pool_address)
        .filter(|c| c.is_fresh(max_age_ms))
        .cloned()
}

/// Remove stale entries older than max_age_ms.
pub async fn cleanup_stale(cache: &PoolStateCache, max_age_ms: u64) {
    let mut map = cache.write().await;
    let before = map.len();
    map.retain(|_, v| v.is_fresh(max_age_ms));
    let removed = before - map.len();
    if removed > 0 {
        debug!("CACHE cleanup: removed {} stale entries", removed);
    }
}
