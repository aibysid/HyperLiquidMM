// ─────────────────────────────────────────────────────────────────────────────
// market_maker.rs — Core Market Making Logic
//
// Phase 9F: Grid Laddering (3-tier bid/ask) + Inventory Skewing + Soft Exit
// Phase 9G: Regime Governor (ATR/Vol-based spread widening, C/F guard, latency)
// Phase 9I: Queue Position Estimator (Shadow Mode fill simulation)
// ─────────────────────────────────────────────────────────────────────────────
use std::collections::{HashMap, VecDeque};
use serde::{Deserialize, Serialize};

// ─── Asset Configuration (from Python Screener via Redis) ─────────────────────

/// Per-asset configuration published by the Python Screener every ~30s.
/// The Rust engine is stateless w.r.t. asset selection — it only quotes
/// what the screener tells it to quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MmAssetConfig {
    pub asset: String,
    /// Minimum price increment (tick size) for the asset.
    pub tick_size: f64,
    /// Minimum order size in USD notional.
    pub min_order_usd: f64,
    /// Maximum inventory we are willing to hold, in USD notional.
    pub max_inv_usd: f64,
    /// Base half-spread in basis points (e.g. 1.5 = 1.5bps per side).
    /// This is the L1 spread at neutral inventory.
    pub base_spread_bps: f64,
    /// 30-minute ATR as a fraction of price (e.g. 0.002 = 0.2%).
    /// Used by the Regime Governor to adjust spreads dynamically.
    pub atr_fraction: f64,
    /// Regime: "calm", "uncertain", or "halt"
    pub regime: String,
}

impl Default for MmAssetConfig {
    fn default() -> Self {
        Self {
            asset: "BTC".to_string(),
            tick_size: 0.1,
            min_order_usd: 10.0,   // Hyperliquid minimum ~$10
            max_inv_usd: 20.0,     // Tight cap for small accounts ($50 total)
            base_spread_bps: 1.5,
            atr_fraction: 0.002,
            regime: "calm".to_string(),
        }
    }
}

// ─── Grid Quote ───────────────────────────────────────────────────────────────

/// A single resting order in the quote grid.
#[derive(Debug, Clone)]
pub struct GridQuote {
    /// "bid" or "ask"
    pub side: &'static str,
    /// Layer: 1 = top of book (tightest), 2 = mid, 3 = deep
    pub layer: u8,
    pub price: f64,
    /// Order size in USD notional
    pub size_usd: f64,
    /// Exchange order ID, set after the order is placed
    pub oid: Option<u64>,
}

/// The full 3-tier quote grid for one asset.
#[derive(Debug, Clone, Default)]
pub struct QuoteGrid {
    pub bids: Vec<GridQuote>,
    pub asks: Vec<GridQuote>,
}

impl QuoteGrid {
    pub fn is_empty(&self) -> bool {
        self.bids.is_empty() && self.asks.is_empty()
    }
}

// ─── Phase 9F: Grid Laddering Engine ─────────────────────────────────────────

/// Computes the 3-tier post-only quote grid for one asset.
///
/// Layer sizing ratio: L1=1x, L2=2x, L3=3x (larger as we go deeper).
/// This means fills at L2/L3 weight more toward PnL, and we have more
/// size available for adverse selection recovery.
///
/// Inventory skewing shifts the ENTIRE grid:
///   - Long inventory  → lower bids (less eager to buy) + lower asks (faster to sell)
///   - Short inventory → higher asks + higher bids
///   Both become Soft Exits: we remain Maker but become more attractive on the
///   side we need to unwind.
pub fn compute_quote_grid(
    mid_price: f64,
    config: &MmAssetConfig,
    inv_usd: f64,          // signed: +long, -short
    regime_multiplier: f64, // from Regime Governor: 1.0 = calm, 2.0 = uncertain
    suppress_bids: bool,   // OFI: sell dominance detected
    suppress_asks: bool,   // OFI: buy dominance detected
) -> QuoteGrid {
    if config.regime == "halt" || mid_price <= 0.0 {
        return QuoteGrid::default();
    }

    let base_half_spread = mid_price * (config.base_spread_bps / 10_000.0);
    let effective_spread = base_half_spread * regime_multiplier;

    // Inventory skew: fraction of max inventory we currently hold.
    // Clamped to [-1, 1]. Skew shifts asks down / bids down when long.
    let inv_fraction = (inv_usd / config.max_inv_usd).clamp(-1.0, 1.0);
    let mut skew_amount  = inv_fraction * effective_spread * 1.5;

    // CAP SKEW TO PREVENT FEE LOSSES ON SOFT EXITS
    // Maker fees are 1.44 bps. Exits must be at least 1.5 bps away from mid to be profitable.
    let min_distance_from_mid = mid_price * (1.5 / 10_000.0);
    let max_allowed_skew = (effective_spread - min_distance_from_mid).max(0.0);
    skew_amount = skew_amount.clamp(-max_allowed_skew, max_allowed_skew);

    // Layer spreads: L2 = 2.5x L1, L3 = 5x L1
    let spreads = [
        effective_spread,
        effective_spread * 2.5,
        effective_spread * 5.0,
    ];

    // Layer sizes: L1 = min, L2 = 2x, L3 = 3x (but at least min_order_usd)
    let base_sz = config.min_order_usd.max(12.0);
    let sizes   = [base_sz, base_sz * 2.0, base_sz * 3.0];

    // Max layers: 1 for small accounts ($50), increase to 3 for $500+
    let max_layers: usize = std::env::var("MM_MAX_LAYERS")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(1);

    let mut grid = QuoteGrid::default();

    for (i, (&sp, &sz)) in spreads.iter().zip(sizes.iter()).enumerate().take(max_layers) {
        let layer = (i + 1) as u8;

        // Bids: placed below mid, shifted down when long (Soft Exit)
        if !suppress_bids {
            let bid_price = snap_to_tick(mid_price - sp - skew_amount, config.tick_size);
            if bid_price > 0.0 {
                grid.bids.push(GridQuote {
                    side: "bid",
                    layer,
                    price: bid_price,
                    size_usd: sz,
                    oid: None,
                });
            }
        }

        // Asks: placed above mid, shifted down when long (Soft Exit — sell cheaper)
        if !suppress_asks {
            let ask_price = snap_to_tick(mid_price + sp - skew_amount, config.tick_size);
            if ask_price > mid_price * 0.9 {
                // Sanity: ask must stay above 90% of mid (don't accidentally cross)
                grid.asks.push(GridQuote {
                    side: "ask",
                    layer,
                    price: ask_price,
                    size_usd: sz,
                    oid: None,
                });
            }
        }
    }

    grid
}

/// Snaps a price to the nearest valid tick size.
pub fn snap_to_tick(price: f64, tick_size: f64) -> f64 {
    if tick_size <= 0.0 { return price; }
    (price / tick_size).round() * tick_size
}

// ─── Phase 9G: Regime Governor ────────────────────────────────────────────────

/// The Regime Governor computes a single `spread_multiplier` that the grid
/// engine applies. It integrates:
///   1. ATR (volatility regime) — wider spreads in choppy markets
///   2. Cancel-to-Fill ratio   — widen when we're spamming cancels
///   3. Latency               — widen when P95 latency > 50ms
///   4. News/Funding extreme   — halt when funding rate is extreme
#[derive(Debug)]
pub struct RegimeGovernor {
    /// ATR multiplier thresholds: below `calm_atr` = calm, above `chaotic_atr` = halt
    pub calm_atr_threshold: f64,
    pub chaotic_atr_threshold: f64,
    /// Maximum funding rate before we stop quoting (e.g. 0.003 = 0.3% per 8h)
    pub max_funding_halt: f64,
    /// Current computed spread multiplier (1.0 = base, higher = wider)
    pub current_multiplier: f64,
    /// Current regime string for logging
    pub regime: String,
}

impl RegimeGovernor {
    pub fn new() -> Self {
        Self {
            calm_atr_threshold:    0.0015, // <0.15% per interval = calm
            chaotic_atr_threshold: 0.005,  // >0.5% per interval = halt quoting
            max_funding_halt:      0.003,  // 0.3% per 8h = extreme
            current_multiplier: 1.0,
            regime: "calm".to_string(),
        }
    }

    /// Recomputes the spread multiplier and regime string.
    /// Returns true if the regime changed (useful for logging).
    pub fn update(
        &mut self,
        atr_fraction: f64,
        cancel_fill_ratio: f64,
        p95_latency_us: u64,
        funding_rate: f64,
    ) -> bool {
        let prev_regime = self.regime.clone();

        // ── Volatility component ──────────────────────────────────────────────
        let vol_mult = if atr_fraction >= self.chaotic_atr_threshold {
            self.regime = "halt".to_string();
            return self.regime != prev_regime;
        } else if atr_fraction >= self.calm_atr_threshold {
            // Linearly scale between 1.0 (calm) and 3.0 (chaotic)
            let t = (atr_fraction - self.calm_atr_threshold)
                  / (self.chaotic_atr_threshold - self.calm_atr_threshold);
            1.0 + t * 2.0
        } else {
            1.0
        };

        // ── Cancel-to-Fill component ──────────────────────────────────────────
        // If C/F > 50, linearly widen by up to 2x additional
        let cfr_mult = if cancel_fill_ratio > 100.0 {
            2.0
        } else if cancel_fill_ratio > 50.0 {
            1.0 + (cancel_fill_ratio - 50.0) / 50.0
        } else {
            1.0
        };

        // ── Latency component ─────────────────────────────────────────────────
        // P95 > 50ms = 1.5x, > 100ms = 2x
        let lat_mult = if p95_latency_us > 100_000 {
            2.0
        } else if p95_latency_us > 50_000 {
            1.5
        } else {
            1.0
        };

        // ── Funding halt ──────────────────────────────────────────────────────
        if funding_rate.abs() >= self.max_funding_halt {
            self.regime = "halt".to_string();
            return self.regime != prev_regime;
        }

        let combined = vol_mult * cfr_mult * lat_mult;
        self.current_multiplier = combined.clamp(1.0, 4.0);

        self.regime = if self.current_multiplier > 1.5 {
            "uncertain".to_string()
        } else {
            "calm".to_string()
        };

        self.regime != prev_regime
    }

    pub fn is_halt(&self) -> bool {
        self.regime == "halt"
    }

    pub fn spread_multiplier(&self) -> f64 {
        self.current_multiplier
    }
}

// ─── Phase 9I: Queue Position Estimator ──────────────────────────────────────

/// Estimates whether our resting Post-Only limit order is likely to have been
/// filled, based on how much volume has traded at-or-through our price since
/// we placed the order.
///
/// This is the key mechanism for Shadow Mode fill simulation — it lets us
/// realistically model PnL without actually placing orders on the exchange.
///
/// Model: If volume traded at our price level since placement >= our order size,
/// we consider ourselves "filled" with probability proportional to the volume ratio.
#[derive(Debug, Default)]
pub struct QueuePositionEstimator {
    /// Tracks per-asset: our resting price and cumulative volume at that level
    entries: HashMap<String, QueueEntry>,
}

#[derive(Debug, Clone)]
struct QueueEntry {
    /// Price level we are resting at
    pub price: f64,
    /// Side: true = bid, false = ask
    pub is_bid: bool,
    /// Our resting size in USD
    pub size_usd: f64,
    /// Cumulative volume (USD) traded at-or-through our price since placement
    pub volume_traded_through: f64,
    /// Timestamp when we placed the order
    pub placed_at_ms: u64,
}

impl QueuePositionEstimator {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    /// Returns true if an order is currently registered for this key.
    pub fn is_registered(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Register a new shadow order.
    pub fn register_order(&mut self, coin: &str, price: f64, is_bid: bool, size_usd: f64, now_ms: u64) {
        self.entries.insert(coin.to_string(), QueueEntry {
            price,
            is_bid,
            size_usd,
            volume_traded_through: 0.0,
            placed_at_ms: now_ms,
        });
    }

    /// Feed a taker trade. If the trade is at-or-through our resting price,
    /// accumulate its volume.
    pub fn on_trade(&mut self, coin: &str, trade_price: f64, is_taker_buy: bool, volume_usd: f64) {
        if let Some(entry) = self.entries.get_mut(coin) {
            let through = if entry.is_bid {
                // Our bid fills when a Taker sells at or below our bid price
                !is_taker_buy && trade_price <= entry.price
            } else {
                // Our ask fills when a Taker buys at or above our ask price
                is_taker_buy && trade_price >= entry.price
            };
            if through {
                entry.volume_traded_through += volume_usd;
            }
        }
    }

    /// Returns estimated fill probability [0, 1] for a coin's shadow order.
    /// Fill probability = min(1, volume_through / (2 * size_usd))
    /// We assume we are in the middle of the queue (hence 2x).
    pub fn fill_probability(&self, coin: &str) -> f64 {
        match self.entries.get(coin) {
            None => 0.0,
            Some(e) => (e.volume_traded_through / (2.0 * e.size_usd)).min(1.0),
        }
    }

    /// Returns true if the order is statistically likely to have been filled.
    pub fn is_likely_filled(&self, coin: &str, threshold: f64) -> bool {
        self.fill_probability(coin) >= threshold
    }

    /// Removes a tracked order (after fill or cancel).
    pub fn remove(&mut self, coin: &str) {
        self.entries.remove(coin);
    }
}

// ─── Fill Tracker (for Shadow Mode PnL accounting) ────────────────────────────

/// Records shadow fills for PnL simulation in Phase 9I.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowFill {
    pub coin: String,
    pub side: String,  // "bid" or "ask"
    pub layer: u8,
    pub price: f64,
    pub size_usd: f64,
    pub fill_ts_ms: u64,
    pub maker_rebate_usd: f64,
}

impl ShadowFill {
    pub fn new(coin: &str, q: &GridQuote, ts_ms: u64) -> Self {
        let maker_rebate = q.size_usd * 0.0001; // 0.01% maker rebate
        Self {
            coin: coin.to_string(),
            side: q.side.to_string(),
            layer: q.layer,
            price: q.price,
            size_usd: q.size_usd,
            fill_ts_ms: ts_ms,
            maker_rebate_usd: maker_rebate,
        }
    }
}

/// Tracks session-level shadow-mode performance.
#[derive(Debug, Default)]
pub struct ShadowSession {
    pub fills: Vec<ShadowFill>,
    pub total_volume_usd: f64,
    pub total_rebates_usd: f64,
    pub total_pnl_usd: f64,
}

impl ShadowSession {
    pub fn record_fill(&mut self, fill: ShadowFill) {
        self.total_volume_usd  += fill.size_usd;
        self.total_rebates_usd += fill.maker_rebate_usd;
        self.total_pnl_usd     += fill.maker_rebate_usd;
        log::info!(
            "[SHADOW FILL] {} {} L{} @ {:.6} | sz=${:.2} | rebate=${:.4} | total_rebates=${:.4}",
            fill.coin, fill.side, fill.layer, fill.price,
            fill.size_usd, fill.maker_rebate_usd, self.total_rebates_usd
        );
        self.fills.push(fill);
    }

    pub fn report(&self) -> String {
        format!(
            "Shadow PnL: ${:.4} | Volume: ${:.2} | Rebates: ${:.4} | Fills: {}",
            self.total_pnl_usd, self.total_volume_usd, self.total_rebates_usd, self.fills.len()
        )
    }
}
