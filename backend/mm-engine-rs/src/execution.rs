// ─────────────────────────────────────────────────────────────────────────────
// execution.rs — Market Maker Execution Engine
//
// Phase 9E: Protective Halts & State Reconciliation
//   - cancel_all()         : Emergency cancel of every open order
//   - reconcile_state()    : REST diff after WS reconnect to catch dark fills
//   - check_global_stop()  : Portfolio drawdown cap enforcement
//   - check_ofi_halt()     : Order Flow Imbalance spike detection
//   - cancel_fill_ratio    : Sliding-window API ban prevention guard
// ─────────────────────────────────────────────────────────────────────────────
use std::collections::{HashMap, VecDeque, HashSet};
use serde::{Deserialize, Serialize};
use crate::risk::{RiskManager, RiskConfig};
use crate::exchange::{ExchangeClient, Position};
use crate::ingestor::MarketDataBuffer;

// ─── Engine Config ────────────────────────────────────────────────────────────

/// Configuration for the MM Engine, published per-asset by the Python screener.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MmEngineConfig {
    /// Maximum portfolio drawdown before global halt (fraction, e.g. 0.05 = 5%).
    pub global_halt_drawdown_pct: f64,
    /// Maximum Cancel-to-Fill ratio before widening quotes to avoid exchange ban.
    pub max_cancel_fill_ratio: f64,
    /// OFI threshold: if |taker_buy_vol - taker_sell_vol| / total_vol > this,
    /// cancel all bids (if negative) or asks (if positive) immediately.
    pub ofi_halt_threshold: f64,
    /// Whether the engine is in shadow mode (no real orders placed).
    pub shadow_mode: bool,
}

impl Default for MmEngineConfig {
    fn default() -> Self {
        Self {
            global_halt_drawdown_pct: 0.05, // 5% max daily drawdown
            max_cancel_fill_ratio: 50.0,    // 50 cancels per fill before throttling
            ofi_halt_threshold: 0.70,       // 70% buy or sell dominance triggers bid/ask cancel
            shadow_mode: true,              // **ALWAYS start in shadow mode**
        }
    }
}

// ─── Session Statistics ───────────────────────────────────────────────────────

/// Sliding-window session stats for Cancel-to-Fill ratio guard and drawdown.
#[derive(Debug, Default)]
pub struct SessionStats {
    pub total_cancels: u64,
    pub total_fills: u64,
    /// Realized + unrealized daily PnL. Negative = drawdown.
    pub daily_pnl_usd: f64,
    pub starting_balance: f64,
}

impl SessionStats {
    pub fn cancel_fill_ratio(&self) -> f64 {
        if self.total_fills == 0 {
            // If we've sent >50 cancels with 0 fills, that's already concerning
            return self.total_cancels as f64;
        }
        self.total_cancels as f64 / self.total_fills as f64
    }

    /// Returns drawdown as a positive fraction (0.05 = 5% loss).
    pub fn daily_drawdown_pct(&self) -> f64 {
        if self.starting_balance <= 0.0 { return 0.0; }
        let loss = -self.daily_pnl_usd.min(0.0); // only count losses
        loss / self.starting_balance
    }
}

// ─── OFI Calculator ───────────────────────────────────────────────────────────

/// Phase 9E: Order Flow Imbalance detector.
/// Tracks taker buy vs. taker sell volume in a rolling window.
/// A heavily negative OFI (sell dominance) should trigger preemptive bid cancellation.
#[derive(Debug, Default)]
pub struct OfiCalculator {
    /// Rolling window of (side: bool=buy, size_usd: f64) events
    window: VecDeque<(bool, f64)>,
    window_size: usize,
}

impl OfiCalculator {
    pub fn new(window_size: usize) -> Self {
        Self { window: VecDeque::new(), window_size }
    }

    /// Record a taker trade: `is_buy=true` for taker buy, `false` for taker sell.
    pub fn record(&mut self, is_buy: bool, size_usd: f64) {
        if self.window.len() >= self.window_size {
            self.window.pop_front();
        }
        self.window.push_back((is_buy, size_usd));
    }

    /// Returns OFI as a fraction in [-1.0, +1.0].
    /// +1.0 = pure taker buy pressure, -1.0 = pure taker sell pressure.
    pub fn ofi_fraction(&self) -> f64 {
        if self.window.is_empty() { return 0.0; }
        
        // Require at least 20 trades before making an OFI halt judgment
        if self.window.len() < 20 { return 0.0; }

        let buy_vol: f64  = self.window.iter().filter(|(b, _)| *b).map(|(_, s)| s).sum();
        let sell_vol: f64 = self.window.iter().filter(|(b, _)| !b).map(|(_, s)| s).sum();
        let total = buy_vol + sell_vol;
        
        // Require at least $5,000 in tracked timeframe volume to matter
        if total <= 5_000.0 { return 0.0; }
        
        (buy_vol - sell_vol) / total
    }

    /// Returns true if sell pressure is dominant enough to warrant bid cancellation.
    pub fn should_cancel_bids(&self, threshold: f64) -> bool {
        self.ofi_fraction() < -threshold
    }

    /// Returns true if buy pressure is dominant enough to warrant ask cancellation.
    pub fn should_cancel_asks(&self, threshold: f64) -> bool {
        self.ofi_fraction() > threshold
    }
}

// ─── Internal Inventory State ─────────────────────────────────────────────────

/// The engine's internal snapshot of what it believes it holds.
/// This gets updated on fills and reconciled against REST state after a reconnect.
#[derive(Debug, Clone, Default)]
pub struct InternalInventory {
    /// Coin → net signed position in contracts (positive = long).
    pub positions: HashMap<String, f64>,
    /// Coin → OID → (price, side, size) for resting orders we placed.
    pub open_orders: HashMap<String, HashMap<u64, (f64, bool, f64)>>,
}

impl InternalInventory {
    /// Applies a fill event, updating the net position for a coin.
    pub fn apply_fill(&mut self, coin: &str, is_buy: bool, size: f64) {
        let pos = self.positions.entry(coin.to_string()).or_insert(0.0);
        if is_buy { *pos += size; } else { *pos -= size; }
    }

    /// Reconciles internal state against live REST positions.
    /// Returns a list of (coin, internal, live, delta) for auditing.
    pub fn reconcile(&mut self, live: &[Position]) -> Vec<(String, f64, f64, f64)> {
        let mut diffs = Vec::new();
        for pos in live {
            let signed = if pos.direction == "LONG" { pos.size } else { -pos.size };
            let internal = self.positions.get(&pos.coin).cloned().unwrap_or(0.0);
            let delta = signed - internal;
            if delta.abs() > 1e-8 {
                log::warn!(
                    "[RECONCILE] {} internal={:.6} live={:.6} delta={:.6} (dark fill detected)",
                    pos.coin, internal, signed, delta
                );
                diffs.push((pos.coin.clone(), internal, signed, delta));
                self.positions.insert(pos.coin.clone(), signed);
            }
        }
        diffs
    }
}

// ─── The Market Maker Execution Engine ───────────────────────────────────────

pub struct MmExecutionEngine {
    pub config: MmEngineConfig,
    pub exchange: Box<dyn ExchangeClient>,
    pub risk_manager: RiskManager,
    pub stats: SessionStats,
    pub ofi_trackers: HashMap<String, OfiCalculator>,
    pub inventory: InternalInventory,
    pub session_id: String,
    /// When true, the engine refuses to quote until manually cleared.
    pub halted: bool,
}

impl MmExecutionEngine {
    pub async fn new(
        config: MmEngineConfig,
        exchange: Box<dyn ExchangeClient>,
        session_id: String,
    ) -> Self {
        let balance = exchange.get_balance().await.unwrap_or(0.0);
        let risk_manager = RiskManager::new(RiskConfig::default(), balance);
        Self {
            config,
            exchange,
            risk_manager,
            stats: SessionStats { starting_balance: balance, ..Default::default() },
            ofi_trackers: HashMap::new(),
            inventory: InternalInventory::default(),
            session_id,
            halted: false,
        }
    }

    // ─── Phase 9E: Emergency Cancel-All ──────────────────────────────────────

    /// Cancels every resting order on the exchange. Returns number cancelled.
    /// This is the FIRST action on: WS reconnect, OFI spike, stall panic, drawdown breach.
    pub async fn cancel_all(&mut self) -> u64 {
        log::warn!("[EXEC] cancel_all() triggered [session={}]", self.session_id);
        if self.config.shadow_mode {
            log::info!("[EXEC] Shadow mode — cancel_all is a no-op.");
            return 0;
        }
        match self.exchange.cancel_all_orders().await {
            Ok(n) => {
                self.stats.total_cancels += n;
                log::warn!("[EXEC] cancel_all: {} orders cancelled.", n);
                n
            }
            Err(e) => {
                log::error!("[EXEC] cancel_all FAILED: {}. Manual intervention required!", e);
                // Even on failure, we mark as halted — do not quote in an unknown state
                self.halted = true;
                0
            }
        }
    }

    // ─── Phase 9E: State Reconciliation ──────────────────────────────────────

    /// Called immediately after a WebSocket reconnect.
    /// 1. Fires cancel_all (in case stale orders filled while dark)
    /// 2. Fetches live positions via REST clearinghouseState
    /// 3. Diffs against internal inventory to detect dark fills
    /// 4. Updates internal state and resumes quoting
    pub async fn reconcile_after_reconnect(&mut self) {
        log::warn!("[RECONCILE] WS reconnect detected. Starting state reconciliation...");

        // Step 1: Drop unknown orders before we know what happened
        self.cancel_all().await;

        // Step 2: Fetch live state from REST
        match self.exchange.get_positions().await {
            Ok(live_positions) => {
                log::info!("[RECONCILE] Live positions fetched: {} assets.", live_positions.len());

                // Step 3: Diff and update internal inventory
                let diffs = self.inventory.reconcile(&live_positions);
                if diffs.is_empty() {
                    log::info!("[RECONCILE] ✅ Inventory matches. No dark fills detected.");
                } else {
                    log::warn!("[RECONCILE] ⚠️  {} dark fill(s) detected and corrected.", diffs.len());
                    for (coin, internal, live, delta) in &diffs {
                        log::warn!("  {} internal={:.4} live={:.4} delta={:.4}", coin, internal, live, delta);
                    }
                }

                // Step 4: Update starting balance after reconcile
                if let Ok(bal) = self.exchange.get_balance().await {
                    log::info!("[RECONCILE] Current balance: ${:.2}", bal);
                    // Adjust daily PnL anchor if this is a fresh session
                    if self.stats.starting_balance <= 0.0 {
                        self.stats.starting_balance = bal;
                    }
                }

                // Resume quoting
                self.halted = false;
                log::info!("[RECONCILE] ✅ State reconciled. Quoting may resume.");
            }
            Err(e) => {
                log::error!("[RECONCILE] Failed to fetch live positions: {}. Staying halted.", e);
                self.halted = true;
            }
        }
    }

    // ─── Phase 9E: Global Portfolio Drawdown Stop ─────────────────────────────

    /// Checks if daily PnL has breached the configured drawdown cap.
    /// If breached: cancel_all → halt engine → return true.
    pub async fn check_global_drawdown_stop(&mut self) -> bool {
        let ddwn = self.stats.daily_drawdown_pct();
        if ddwn >= self.config.global_halt_drawdown_pct {
            log::error!(
                "🛑 [GLOBAL STOP] Daily drawdown {:.2}% >= cap {:.2}%. Halting all quoting.",
                ddwn * 100.0,
                self.config.global_halt_drawdown_pct * 100.0
            );
            self.cancel_all().await;
            self.halted = true;
            return true;
        }
        false
    }

    // ─── Phase 9E: OFI Protective Halt ───────────────────────────────────────

    /// Feeds a taker trade into the OFI calculator.
    /// Should be called for every trade event received from the WS `trades` channel.
    pub fn record_taker_trade(&mut self, coin: &str, is_buy: bool, price: f64, size: f64) {
        let size_usd = price * size;
        let tracker = self.ofi_trackers.entry(coin.to_string()).or_insert_with(|| OfiCalculator::new(200));
        tracker.record(is_buy, size_usd);
    }

    /// Returns true if the OFI is sufficiently one-sided to warrant cancelling bids.
    /// Called in the main quoting loop before placing new bid quotes.
    pub fn ofi_bids_blocked(&self, coin: &str) -> bool {
        if let Some(tracker) = self.ofi_trackers.get(coin) {
            tracker.should_cancel_bids(self.config.ofi_halt_threshold)
        } else {
            false
        }
    }

    /// Returns true if the OFI is sufficiently one-sided to warrant cancelling asks.
    pub fn ofi_asks_blocked(&self, coin: &str) -> bool {
        if let Some(tracker) = self.ofi_trackers.get(coin) {
            tracker.should_cancel_asks(self.config.ofi_halt_threshold)
        } else {
            false
        }
    }

    // ─── Phase 9G: Cancel-to-Fill Ratio Guard ────────────────────────────────

    /// Returns true if the C/F ratio has spiked past the configured limit.
    /// Signals the Regime Governor to force-widen quotes and hold them,
    /// reducing API spam before Hyperliquid imposes a soft ban.
    pub fn is_cancel_fill_ratio_breached(&self) -> bool {
        self.stats.cancel_fill_ratio() > self.config.max_cancel_fill_ratio
    }

    // ─── Helpers ─────────────────────────────────────────────────────────────

    pub fn is_shadow_mode(&self) -> bool {
        self.config.shadow_mode
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    pub async fn get_balance(&self) -> f64 {
        self.exchange.get_balance().await.unwrap_or(0.0)
    }

    pub async fn get_positions(&self) -> Vec<Position> {
        self.exchange.get_positions().await.unwrap_or_default()
    }

    /// PHASE 9L: Pre-Flight Margin Check
    /// Returns true if the account equity can safely cover the requested notional at 10x leverage.
    pub async fn has_sufficient_margin(&self, total_notional_usd: f64) -> bool {
        if self.config.shadow_mode { return true; }

        match self.exchange.get_balance().await {
            Ok(balance) => {
                // Rule of Thumb: We want current account value to be at least 10% of total
                // intended exposure (10x gross leverage cap for this specific grid).
                let min_required = total_notional_usd / 10.0;
                
                if balance < min_required {
                    log::warn!(
                        "[MARGIN GUARD] Grid placement blocked. Required Notional: ${:.2}. 10x Margin Required: ${:.2}. Available Equity: ${:.2}",
                        total_notional_usd, min_required, balance
                    );
                    return false;
                }
                true
            }
            Err(e) => {
                log::error!("[MARGIN GUARD] Could not fetch balance: {:?}. Skipping for safety.", e);
                false
            }
        }
    }

    /// PHASE 9M: Orphaned Position Flusher
    /// Detects positions for coins that are NO LONGER in the active screener whitelist
    /// and closes them using a Taker (Market) order to immediately free up margin.
    pub async fn flush_orphaned_positions(&mut self, active_coins: &HashSet<String>) {
        if self.config.shadow_mode { return; }
        log::info!("[FLUSH] Checking for orphaned positions...");

        let live_positions = match self.exchange.get_positions().await {
            Ok(pos) => pos,
            Err(e) => {
                log::error!("[FLUSH] Failed to fetch live positions: {:?}", e);
                return;
            }
        };

        if live_positions.is_empty() {
            log::info!("[FLUSH] No open positions to check.");
            return;
        }

        // Fetch all market mids so we can place competitive Maker orders
        let mids = match self.exchange.get_all_mids().await {
            Ok(m) => m,
            Err(e) => {
                log::error!("[FLUSH] Failed to fetch all mids: {:?}", e);
                return;
            }
        };

        log::info!("[FLUSH] Analyzing {} live positions against active whitelist.", live_positions.len());
        for pos in live_positions {
            if !active_coins.contains(&pos.coin) && pos.size.abs() > 1e-8 {
                let current_px = mids.get(&pos.coin).cloned().unwrap_or(pos.entry_price);
                
                log::warn!(
                    "[FLUSH] Orphaned position detected: {} ({:.4} coins). Closing via Maker order @ ${:.6}...",
                    pos.coin, pos.size, current_px
                );

                // Step 1: Clear any existing stale orders for this orphaned coin
                let _ = self.exchange.cancel_coin_orders(&pos.coin).await;

                // Step 2: Place a Post-Only (Maker) order at the mid price
                let direction = if pos.direction == "LONG" { "SHORT" } else { "LONG" };
                match self.exchange.open_order(
                    &pos.coin,
                    direction,
                    pos.size,
                    current_px, 
                    1.0, 0.0, 0.0, true // POST_ONLY = true
                ).await {
                    Ok(_) => log::info!("[FLUSH] Successfully placed maker-close for {}.", pos.coin),
                    Err(e) => log::error!("[FLUSH] Failed to place maker-close for {}: {:?}", pos.coin, e),
                }
            }
        }
    }
}
