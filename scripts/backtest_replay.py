#!/usr/bin/env python3
"""
backtest_replay.py — Offline Backtester for HyperLiquidMM
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Replays historical tick data (CSV or Parquet) through a Python port of the
Rust grid logic + queue position estimator to simulate MM performance.

This is the Phase 2 "what-if" tool: test different parameters without touching
the live engine.

Usage:
    # Single coin, default params
    python3 backtest_replay.py --coin BTC --days 3

    # Parameter sweep
    python3 backtest_replay.py --coin BTC --days 3 --sweep

    # All coins
    python3 backtest_replay.py --all --days 5

    # Custom spread
    python3 backtest_replay.py --coin ETH --days 3 --spread 2.0

    # Custom parameters
    python3 backtest_replay.py --coin SOL --days 3 --spread 2.5 --max-inv 300

Output:
    - Per-run summary: fills, PnL, rebates, Sharpe, max drawdown
    - Parameter sweep: comparison table across parameter combos
    - Trade log: optional CSV of every simulated fill
"""

import os
import sys
import csv
import math
import glob
import json
import argparse
import logging
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from pathlib import Path
from collections import defaultdict

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [BACKTEST] %(levelname)s %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("backtest")

# ─── Tick data directories ──────────────────────────────────────────────────────
TICK_CSV_DIR = "data/ticks"
TICK_PARQUET_DIR = "data/parquet"
# Also check engine-relative paths
ALT_CSV_DIR = "backend/mm-engine-rs/data/ticks"

# ─── Constants (matching Rust defaults) ──────────────────────────────────────────
MAKER_REBATE_RATE = 0.0001   # 0.01% maker rebate per fill
TAKER_FEE_RATE = 0.00035     # 0.035% taker fee (for forced exits)
FILL_THRESHOLD = 0.70        # Queue position fill probability threshold
QUEUE_DEPTH_FACTOR = 2.0     # Assume we're in the middle of the queue


# ═══════════════════════════════════════════════════════════════════════════════
# 1. DATA STRUCTURES (Python port of Rust structs)
# ═══════════════════════════════════════════════════════════════════════════════

@dataclass
class MmAssetConfig:
    """Mirrors Rust MmAssetConfig."""
    asset: str = "BTC"
    tick_size: float = 0.1
    min_order_usd: float = 12.0
    max_inv_usd: float = 200.0
    base_spread_bps: float = 1.5
    atr_fraction: float = 0.002
    regime: str = "calm"


@dataclass
class GridQuote:
    """A single resting order in the quote grid."""
    side: str       # "bid" or "ask"
    layer: int      # 1, 2, or 3
    price: float
    size_usd: float


@dataclass
class QuoteGrid:
    """3-tier bid/ask quote grid."""
    bids: list = field(default_factory=list)
    asks: list = field(default_factory=list)

    def is_empty(self):
        return len(self.bids) == 0 and len(self.asks) == 0


@dataclass
class ShadowFill:
    """Record of a simulated fill."""
    coin: str
    side: str
    layer: int
    price: float
    size_usd: float
    fill_ts_ms: int
    maker_rebate_usd: float
    mid_at_fill: float = 0.0


@dataclass
class QueueEntry:
    """Tracks a resting shadow order for fill estimation."""
    price: float
    is_bid: bool
    size_usd: float
    volume_traded_through: float = 0.0
    placed_at_ms: int = 0


@dataclass
class TickData:
    """One row from the tick CSV."""
    timestamp_ms: int
    coin: str
    best_bid: float
    best_ask: float
    mid_price: float
    spread_bps: float


# ═══════════════════════════════════════════════════════════════════════════════
# 2. CORE MM LOGIC (Python port of Rust)
# ═══════════════════════════════════════════════════════════════════════════════

def snap_to_tick(price: float, tick_size: float) -> float:
    """Snap price to nearest valid tick. Mirrors Rust snap_to_tick."""
    if tick_size <= 0:
        return price
    return round(price / tick_size) * tick_size


def compute_quote_grid(mid_price: float, config: MmAssetConfig,
                       inv_usd: float = 0.0, regime_multiplier: float = 1.0,
                       suppress_bids: bool = False,
                       suppress_asks: bool = False) -> QuoteGrid:
    """
    3-tier post-only quote grid. Exact port of Rust compute_quote_grid.

    Layer spreads: L1=1x, L2=2.5x, L3=5x
    Layer sizes:   L1=1x, L2=2x,   L3=3x
    Inventory skewing shifts entire grid (Soft Exit mechanism).
    """
    grid = QuoteGrid()

    if config.regime == "halt" or mid_price <= 0:
        return grid

    base_half_spread = mid_price * (config.base_spread_bps / 10_000.0)
    effective_spread = base_half_spread * regime_multiplier

    # Inventory skew
    inv_fraction = max(-1.0, min(1.0, inv_usd / config.max_inv_usd))
    skew_amount = inv_fraction * effective_spread * 1.5

    # Layer spreads and sizes
    spreads = [effective_spread, effective_spread * 2.5, effective_spread * 5.0]
    base_sz = max(config.min_order_usd, 12.0)
    sizes = [base_sz, base_sz * 2.0, base_sz * 3.0]

    for i, (sp, sz) in enumerate(zip(spreads, sizes)):
        layer = i + 1

        if not suppress_bids:
            bid_price = snap_to_tick(mid_price - sp - skew_amount, config.tick_size)
            if bid_price > 0:
                grid.bids.append(GridQuote("bid", layer, bid_price, sz))

        if not suppress_asks:
            ask_price = snap_to_tick(mid_price + sp - skew_amount, config.tick_size)
            if ask_price > mid_price * 0.9:
                grid.asks.append(GridQuote("ask", layer, ask_price, sz))

    return grid


class QueuePositionEstimator:
    """
    Estimates shadow fill probability. Port of Rust QueuePositionEstimator.

    Model: If volume traded at-or-through our price >= 2x our order size,
    we're likely filled (assumes we're in the middle of the queue).
    """

    def __init__(self):
        self.entries: dict[str, QueueEntry] = {}

    def register_order(self, key: str, price: float, is_bid: bool,
                       size_usd: float, ts_ms: int):
        self.entries[key] = QueueEntry(price, is_bid, size_usd, 0.0, ts_ms)

    def is_registered(self, key: str) -> bool:
        return key in self.entries

    def on_trade(self, key: str, trade_price: float, is_taker_buy: bool,
                 volume_usd: float):
        """Feed a taker trade. Accumulate volume if at-or-through our price."""
        entry = self.entries.get(key)
        if entry is None:
            return

        if entry.is_bid:
            # Our bid fills when taker sells at/below our bid
            through = (not is_taker_buy) and (trade_price <= entry.price)
        else:
            # Our ask fills when taker buys at/above our ask
            through = is_taker_buy and (trade_price >= entry.price)

        if through:
            entry.volume_traded_through += volume_usd

    def fill_probability(self, key: str) -> float:
        entry = self.entries.get(key)
        if entry is None:
            return 0.0
        return min(1.0, entry.volume_traded_through /
                   (QUEUE_DEPTH_FACTOR * entry.size_usd))

    def is_likely_filled(self, key: str, threshold: float = FILL_THRESHOLD) -> bool:
        return self.fill_probability(key) >= threshold

    def remove(self, key: str):
        self.entries.pop(key, None)


class RegimeGovernor:
    """Port of Rust RegimeGovernor. Computes spread multiplier from volatility."""

    def __init__(self):
        self.calm_atr_threshold = 0.0015
        self.chaotic_atr_threshold = 0.005
        self.max_funding_halt = 0.003
        self.current_multiplier = 1.0
        self.regime = "calm"

    def update_from_ticks(self, prices: list[float]) -> float:
        """
        Compute ATR-like volatility from recent mid prices and update multiplier.
        Uses absolute returns over the price window.
        """
        if len(prices) < 2:
            self.current_multiplier = 1.0
            self.regime = "calm"
            return 1.0

        # Average absolute return over the window
        returns = [abs(prices[i] - prices[i-1]) / prices[i-1]
                   for i in range(1, len(prices)) if prices[i-1] > 0]
        if not returns:
            return 1.0

        atr_fraction = sum(returns) / len(returns)

        if atr_fraction >= self.chaotic_atr_threshold:
            self.regime = "halt"
            self.current_multiplier = 4.0
            return 4.0
        elif atr_fraction >= self.calm_atr_threshold:
            t = (atr_fraction - self.calm_atr_threshold) / \
                (self.chaotic_atr_threshold - self.calm_atr_threshold)
            self.current_multiplier = 1.0 + t * 2.0
            self.regime = "uncertain" if self.current_multiplier > 1.5 else "calm"
        else:
            self.current_multiplier = 1.0
            self.regime = "calm"

        return self.current_multiplier


# ═══════════════════════════════════════════════════════════════════════════════
# 3. BACKTEST SESSION
# ═══════════════════════════════════════════════════════════════════════════════

@dataclass
class BacktestResult:
    """Summary of one backtest run."""
    coin: str
    days: int
    ticks_processed: int
    total_fills: int
    bid_fills: int
    ask_fills: int
    total_volume_usd: float
    total_rebates_usd: float
    spread_pnl_usd: float       # PnL from bid-ask spread capture
    inventory_pnl_usd: float    # PnL from inventory mark-to-market
    total_pnl_usd: float
    max_drawdown_usd: float
    max_inventory_usd: float
    sharpe_ratio: float
    avg_fill_rate_per_hour: float
    halted_pct: float           # % of ticks where regime was "halt"
    # Parameters used
    base_spread_bps: float
    max_inv_usd: float
    min_order_usd: float

    def summary_line(self):
        return (f"{self.coin:>6s} | spread={self.base_spread_bps:.1f}bps | "
                f"fills={self.total_fills:>5d} (B:{self.bid_fills} A:{self.ask_fills}) | "
                f"vol=${self.total_volume_usd:>10,.0f} | "
                f"PnL=${self.total_pnl_usd:>8.2f} | "
                f"rebates=${self.total_rebates_usd:>6.2f} | "
                f"DD=${self.max_drawdown_usd:>7.2f} | "
                f"fills/hr={self.avg_fill_rate_per_hour:.1f} | "
                f"halted={self.halted_pct:.1f}%")


def run_backtest(coin: str, ticks: list[TickData], config: MmAssetConfig,
                 verbose: bool = False) -> BacktestResult:
    """
    Run a single backtest: replay ticks through the MM grid + queue estimator.

    This is a faithful port of the main.rs quoting loop.
    """
    estimator = QueuePositionEstimator()
    governor = RegimeGovernor()

    # State
    inventory_units = 0.0      # Net position in units of the asset
    inventory_usd = 0.0        # Mark-to-market value
    fills: list[ShadowFill] = []
    total_volume = 0.0
    total_rebates = 0.0
    spread_pnl = 0.0           # Realized PnL from matched bid/ask fills
    peak_pnl = 0.0
    max_drawdown = 0.0
    max_inventory = 0.0
    halted_ticks = 0

    # For Sharpe calculation
    hourly_pnls: list[float] = []
    current_hour_pnl = 0.0
    current_hour = -1

    # Rolling window of mid prices for regime governor
    mid_window: list[float] = []
    MID_WINDOW_SIZE = 300  # ~30 seconds at 100ms ticks

    # Track matched fills for spread PnL
    unmatched_bids: list[float] = []  # Prices of unmatched bid fills
    unmatched_asks: list[float] = []  # Prices of unmatched ask fills

    for tick_idx, tick in enumerate(ticks):
        mid = tick.mid_price
        if mid <= 0:
            continue

        # ── Regime Governor ────────────────────────────────────────────────
        mid_window.append(mid)
        if len(mid_window) > MID_WINDOW_SIZE:
            mid_window.pop(0)

        # Update regime every 100 ticks (~10 seconds)
        regime_mult = 1.0
        if tick_idx % 100 == 0 and len(mid_window) >= 10:
            regime_mult = governor.update_from_ticks(mid_window[-100:])

        if governor.regime == "halt":
            halted_ticks += 1
            # Cancel all existing orders when halted
            estimator.entries.clear()
            continue

        regime_mult = governor.current_multiplier

        # ── Inventory mark-to-market ───────────────────────────────────────
        inventory_usd = inventory_units * mid
        max_inventory = max(max_inventory, abs(inventory_usd))

        # ── Inventory cap: suppress orders on over-weighted side ──────────
        at_max_long = inventory_usd >= config.max_inv_usd
        at_max_short = inventory_usd <= -config.max_inv_usd

        # ── Compute grid ──────────────────────────────────────────────────
        grid = compute_quote_grid(
            mid, config, inventory_usd, regime_mult,
            suppress_bids=at_max_long,    # Don't bid if already max long
            suppress_asks=at_max_short,   # Don't ask if already max short
        )
        if grid.is_empty():
            continue

        # ── Register orders (if not already resting at this level) ────────
        for quote in grid.bids + grid.asks:
            key = f"{coin}_{quote.side}_L{quote.layer}"
            if not estimator.is_registered(key):
                estimator.register_order(
                    key, quote.price, quote.side == "bid",
                    quote.size_usd, tick.timestamp_ms
                )

        # ── Simulate trades at this tick ──────────────────────────────────
        # We model each tick as generating a small trade at the bid or ask
        # The spread itself implies volume at those levels
        bid_price = tick.best_bid
        ask_price = tick.best_ask

        if bid_price > 0:
            # Simulate taker sell hitting the bid
            trade_vol = config.min_order_usd * 0.5  # Conservative fill volume
            for layer in range(1, 4):
                key = f"{coin}_bid_L{layer}"
                estimator.on_trade(key, bid_price, False, trade_vol)
                key = f"{coin}_ask_L{layer}"
                estimator.on_trade(key, bid_price, False, trade_vol)

        if ask_price > 0:
            # Simulate taker buy hitting the ask
            trade_vol = config.min_order_usd * 0.5
            for layer in range(1, 4):
                key = f"{coin}_bid_L{layer}"
                estimator.on_trade(key, ask_price, True, trade_vol)
                key = f"{coin}_ask_L{layer}"
                estimator.on_trade(key, ask_price, True, trade_vol)

        # ── Check fills ───────────────────────────────────────────────────
        for quote in grid.bids:
            key = f"{coin}_{quote.side}_L{quote.layer}"
            if estimator.is_likely_filled(key):
                # HARD CAP: reject bid fill if already at max long inventory
                new_inv = inventory_usd + quote.size_usd
                if new_inv > config.max_inv_usd * 1.1:  # 10% tolerance
                    estimator.remove(key)  # Cancel this order
                    continue

                rebate = quote.size_usd * MAKER_REBATE_RATE
                fill = ShadowFill(
                    coin, quote.side, quote.layer, quote.price,
                    quote.size_usd, tick.timestamp_ms, rebate, mid
                )
                fills.append(fill)
                total_volume += quote.size_usd
                total_rebates += rebate

                # Update inventory (bought)
                units_bought = quote.size_usd / mid
                inventory_units += units_bought
                inventory_usd = inventory_units * mid  # Refresh immediately
                unmatched_bids.append(quote.price)

                # Match against unmatched asks for spread PnL
                if unmatched_asks:
                    ask_fill_price = unmatched_asks.pop(0)
                    spread_earned = (ask_fill_price - quote.price) / mid * quote.size_usd
                    spread_pnl += spread_earned

                current_hour_pnl += rebate
                estimator.remove(key)

                if verbose:
                    log.debug(f"  FILL: {coin} BID L{quote.layer} @ {quote.price:.4f} "
                              f"${quote.size_usd:.0f} | inv=${inventory_usd:.0f}")

        for quote in grid.asks:
            key = f"{coin}_{quote.side}_L{quote.layer}"
            if estimator.is_likely_filled(key):
                # HARD CAP: reject ask fill if already at max short inventory
                new_inv = inventory_usd - quote.size_usd
                if new_inv < -config.max_inv_usd * 1.1:  # 10% tolerance
                    estimator.remove(key)  # Cancel this order
                    continue

                rebate = quote.size_usd * MAKER_REBATE_RATE
                fill = ShadowFill(
                    coin, quote.side, quote.layer, quote.price,
                    quote.size_usd, tick.timestamp_ms, rebate, mid
                )
                fills.append(fill)
                total_volume += quote.size_usd
                total_rebates += rebate

                # Update inventory (sold)
                units_sold = quote.size_usd / mid
                inventory_units -= units_sold
                inventory_usd = inventory_units * mid  # Refresh immediately
                unmatched_asks.append(quote.price)

                # Match against unmatched bids for spread PnL
                if unmatched_bids:
                    bid_fill_price = unmatched_bids.pop(0)
                    spread_earned = (quote.price - bid_fill_price) / mid * quote.size_usd
                    spread_pnl += spread_earned

                current_hour_pnl += rebate
                estimator.remove(key)

                if verbose:
                    log.debug(f"  FILL: {coin} ASK L{quote.layer} @ {quote.price:.4f} "
                              f"${quote.size_usd:.0f} | inv=${inventory_usd:.0f}")

        # ── Hourly PnL tracking (for Sharpe) ──────────────────────────────
        tick_hour = tick.timestamp_ms // 3_600_000
        if current_hour == -1:
            current_hour = tick_hour
        elif tick_hour != current_hour:
            hourly_pnls.append(current_hour_pnl)
            current_hour_pnl = 0.0
            current_hour = tick_hour

        # ── Drawdown tracking ─────────────────────────────────────────────
        running_pnl = total_rebates + spread_pnl + (inventory_units * mid)
        peak_pnl = max(peak_pnl, running_pnl)
        drawdown = peak_pnl - running_pnl
        max_drawdown = max(max_drawdown, drawdown)

    # ── Final calculations ────────────────────────────────────────────────────
    # Append last hour
    if current_hour_pnl != 0:
        hourly_pnls.append(current_hour_pnl)

    # Final inventory PnL (mark remaining inventory to last mid price)
    final_mid = ticks[-1].mid_price if ticks else 0
    inventory_pnl = inventory_units * final_mid  # Unrealized

    # Sharpe ratio (annualized from hourly)
    sharpe = 0.0
    if len(hourly_pnls) >= 2:
        mean_pnl = sum(hourly_pnls) / len(hourly_pnls)
        std_pnl = (sum((p - mean_pnl) ** 2 for p in hourly_pnls) /
                   (len(hourly_pnls) - 1)) ** 0.5
        if std_pnl > 0:
            sharpe = (mean_pnl / std_pnl) * (8760 ** 0.5)  # Annualize

    # Duration
    duration_hours = 0
    if len(ticks) >= 2:
        duration_ms = ticks[-1].timestamp_ms - ticks[0].timestamp_ms
        duration_hours = max(1, duration_ms / 3_600_000)

    total_pnl = total_rebates + spread_pnl + inventory_pnl
    total_ticks = len(ticks)

    return BacktestResult(
        coin=coin,
        days=max(1, int(duration_hours / 24)),
        ticks_processed=total_ticks,
        total_fills=len(fills),
        bid_fills=sum(1 for f in fills if f.side == "bid"),
        ask_fills=sum(1 for f in fills if f.side == "ask"),
        total_volume_usd=total_volume,
        total_rebates_usd=total_rebates,
        spread_pnl_usd=spread_pnl,
        inventory_pnl_usd=inventory_pnl,
        total_pnl_usd=total_pnl,
        max_drawdown_usd=max_drawdown,
        max_inventory_usd=max_inventory,
        sharpe_ratio=sharpe,
        avg_fill_rate_per_hour=len(fills) / max(1, duration_hours),
        halted_pct=(halted_ticks / max(1, total_ticks)) * 100,
        base_spread_bps=config.base_spread_bps,
        max_inv_usd=config.max_inv_usd,
        min_order_usd=config.min_order_usd,
    )


# ═══════════════════════════════════════════════════════════════════════════════
# 4. DATA LOADING
# ═══════════════════════════════════════════════════════════════════════════════

def load_ticks(coin: str, days: int, project_root: str = ".") -> list[TickData]:
    """
    Load tick data from CSV or Parquet files.
    Tries multiple directory paths. Returns list of TickData sorted by timestamp.
    """
    ticks = []
    files_found = 0

    # Build list of dates to load
    dates = []
    for d in range(days):
        date = datetime.now(timezone.utc) - timedelta(days=d)
        dates.append(date.strftime("%Y-%m-%d"))

    # Try directories in order of preference
    dirs_to_try = [
        os.path.join(project_root, TICK_PARQUET_DIR, coin),
        os.path.join(project_root, TICK_CSV_DIR, coin),
        os.path.join(project_root, ALT_CSV_DIR, coin),
    ]

    for tick_dir in dirs_to_try:
        if not os.path.isdir(tick_dir):
            continue

        for date_str in dates:
            # Try Parquet
            parquet_path = os.path.join(tick_dir, f"{date_str}.parquet")
            if os.path.exists(parquet_path):
                loaded = _load_parquet(parquet_path, coin)
                ticks.extend(loaded)
                files_found += 1
                log.debug(f"  Loaded {len(loaded)} ticks from {parquet_path}")
                continue

            # Try CSV
            csv_path = os.path.join(tick_dir, f"{date_str}.csv")
            if os.path.exists(csv_path):
                loaded = _load_csv(csv_path, coin)
                ticks.extend(loaded)
                files_found += 1
                log.debug(f"  Loaded {len(loaded)} ticks from {csv_path}")

        if files_found > 0:
            break  # Found data in this directory, don't try others

    if not ticks:
        log.warning(f"No tick data found for {coin} in any of: {dirs_to_try}")
        return []

    # Sort by timestamp
    ticks.sort(key=lambda t: t.timestamp_ms)
    log.info(f"Loaded {len(ticks):,} ticks for {coin} from {files_found} files "
             f"({dates[-1]} to {dates[0]})")

    return ticks


def _load_csv(path: str, coin: str) -> list[TickData]:
    """Load tick data from a harvested CSV file."""
    ticks = []
    try:
        with open(path, 'r') as f:
            for line in f:
                parts = line.strip().split(',')
                if len(parts) < 6:
                    continue
                try:
                    ts = int(parts[0])
                    bid = float(parts[2])
                    ask = float(parts[3])
                    mid = float(parts[4])
                    spread = float(parts[5])
                    if mid > 0 and bid > 0 and ask > 0:
                        ticks.append(TickData(ts, coin, bid, ask, mid, spread))
                except (ValueError, IndexError):
                    continue
    except Exception as e:
        log.warning(f"Failed to read CSV {path}: {e}")
    return ticks


def _load_parquet(path: str, coin: str) -> list[TickData]:
    """Load tick data from a Parquet file using DuckDB."""
    ticks = []
    try:
        import duckdb
        con = duckdb.connect()
        rows = con.execute(
            f"SELECT timestamp_ms, best_bid, best_ask, mid_price, spread_bps "
            f"FROM read_parquet('{path}') "
            f"WHERE mid_price > 0 AND best_bid > 0 AND best_ask > 0 "
            f"ORDER BY timestamp_ms"
        ).fetchall()
        for row in rows:
            ticks.append(TickData(int(row[0]), coin, row[1], row[2], row[3], row[4]))
    except Exception as e:
        log.warning(f"Failed to read Parquet {path}: {e}")
    return ticks


def discover_coins(project_root: str = ".") -> list[str]:
    """Find all coins that have tick data."""
    coins = set()
    for base_dir in [TICK_CSV_DIR, TICK_PARQUET_DIR, ALT_CSV_DIR]:
        full_dir = os.path.join(project_root, base_dir)
        if os.path.isdir(full_dir):
            for entry in os.listdir(full_dir):
                entry_path = os.path.join(full_dir, entry)
                if os.path.isdir(entry_path):
                    # Check if directory has data files
                    files = glob.glob(os.path.join(entry_path, "*.csv")) + \
                            glob.glob(os.path.join(entry_path, "*.parquet"))
                    if files:
                        coins.add(entry)
    return sorted(coins)


# ═══════════════════════════════════════════════════════════════════════════════
# 5. PARAMETER SWEEP
# ═══════════════════════════════════════════════════════════════════════════════

def run_parameter_sweep(coin: str, ticks: list[TickData],
                        tick_size: float = 0.1) -> list[BacktestResult]:
    """
    Sweep over key parameters to find optimal settings.

    Sweeps:
      - base_spread_bps: [0.5, 1.0, 1.5, 2.0, 3.0, 5.0]
      - max_inv_usd: [100, 200, 500]
      - min_order_usd: [12, 24] (L1 size)
    """
    spread_values = [0.5, 1.0, 1.5, 2.0, 3.0, 5.0]
    inv_values = [100.0, 200.0, 500.0]
    size_values = [12.0, 24.0]

    results = []
    total = len(spread_values) * len(inv_values) * len(size_values)
    run = 0

    log.info(f"Starting parameter sweep: {total} combinations for {coin}")

    for spread in spread_values:
        for max_inv in inv_values:
            for min_sz in size_values:
                run += 1
                config = MmAssetConfig(
                    asset=coin,
                    tick_size=tick_size,
                    min_order_usd=min_sz,
                    max_inv_usd=max_inv,
                    base_spread_bps=spread,
                )

                result = run_backtest(coin, ticks, config)
                results.append(result)

                if run % 6 == 0 or run == total:
                    log.info(f"  [{run}/{total}] spread={spread}bps inv=${max_inv} "
                             f"sz=${min_sz} → fills={result.total_fills} "
                             f"PnL=${result.total_pnl_usd:.2f}")

    # Sort by PnL descending
    results.sort(key=lambda r: r.total_pnl_usd, reverse=True)
    return results


# ═══════════════════════════════════════════════════════════════════════════════
# 6. REPORTING
# ═══════════════════════════════════════════════════════════════════════════════

def print_result(result: BacktestResult):
    """Pretty-print a single backtest result."""
    print()
    print("=" * 80)
    print(f" BACKTEST RESULT — {result.coin}")
    print("=" * 80)
    print(f" Period:           {result.days} days ({result.ticks_processed:,} ticks)")
    print(f" Parameters:       spread={result.base_spread_bps:.1f}bps  "
          f"max_inv=${result.max_inv_usd:.0f}  min_order=${result.min_order_usd:.0f}")
    print("-" * 80)
    print(f" Total Fills:      {result.total_fills:>8d}  "
          f"(bids={result.bid_fills}, asks={result.ask_fills})")
    print(f" Fill Rate:        {result.avg_fill_rate_per_hour:>8.1f} fills/hour")
    print(f" Total Volume:     ${result.total_volume_usd:>10,.2f}")
    print(f" Maker Rebates:    ${result.total_rebates_usd:>10.4f}")
    print(f" Spread PnL:       ${result.spread_pnl_usd:>10.4f}")
    print(f" Inventory PnL:    ${result.inventory_pnl_usd:>10.4f} (unrealized)")
    print(f" ─────────────────────────────────────────────────")
    print(f" TOTAL PnL:        ${result.total_pnl_usd:>10.4f}")
    print(f" Max Drawdown:     ${result.max_drawdown_usd:>10.4f}")
    print(f" Max Inventory:    ${result.max_inventory_usd:>10.2f}")
    print(f" Sharpe Ratio:     {result.sharpe_ratio:>10.2f}  (annualized)")
    print(f" Halted:           {result.halted_pct:>9.1f}%")
    print("=" * 80)


def print_sweep_table(results: list[BacktestResult]):
    """Pretty-print parameter sweep comparison table."""
    print()
    print("=" * 110)
    print(" PARAMETER SWEEP RESULTS (sorted by PnL)")
    print("=" * 110)
    print(f" {'#':>2}  {'Spread':>7}  {'MaxInv':>7}  {'MinSz':>6}  "
          f"{'Fills':>6}  {'Volume':>10}  {'Rebates':>8}  {'SpreadPnL':>10}  "
          f"{'TotalPnL':>9}  {'MaxDD':>8}  {'Sharpe':>7}  {'Fills/hr':>8}")
    print("-" * 110)

    for i, r in enumerate(results[:20], 1):
        marker = " ★" if i == 1 else "  "
        print(f"{marker}{i:>2}  {r.base_spread_bps:>6.1f}bp  "
              f"${r.max_inv_usd:>5.0f}  ${r.min_order_usd:>4.0f}  "
              f"{r.total_fills:>6d}  ${r.total_volume_usd:>9,.0f}  "
              f"${r.total_rebates_usd:>7.4f}  ${r.spread_pnl_usd:>9.4f}  "
              f"${r.total_pnl_usd:>8.4f}  ${r.max_drawdown_usd:>7.4f}  "
              f"{r.sharpe_ratio:>7.2f}  {r.avg_fill_rate_per_hour:>7.1f}")

    print("-" * 110)
    if results:
        best = results[0]
        print(f" ★ BEST: spread={best.base_spread_bps:.1f}bps, "
              f"max_inv=${best.max_inv_usd:.0f}, min_order=${best.min_order_usd:.0f}")
        print(f"   → PnL=${best.total_pnl_usd:.4f}, "
              f"Sharpe={best.sharpe_ratio:.2f}, "
              f"fills/hr={best.avg_fill_rate_per_hour:.1f}")
    print()


# ═══════════════════════════════════════════════════════════════════════════════
# 7. MAIN
# ═══════════════════════════════════════════════════════════════════════════════

def main():
    parser = argparse.ArgumentParser(
        description="Offline Backtester for HyperLiquidMM — replay tick data through MM logic"
    )
    parser.add_argument("--coin", type=str, default=None,
                        help="Coin to backtest (e.g., BTC, ETH)")
    parser.add_argument("--all", action="store_true",
                        help="Backtest all coins with available data")
    parser.add_argument("--days", type=int, default=3,
                        help="Days of data to load (default: 3)")
    parser.add_argument("--spread", type=float, default=1.5,
                        help="Base spread in bps (default: 1.5)")
    parser.add_argument("--max-inv", type=float, default=200.0,
                        help="Max inventory USD (default: 200)")
    parser.add_argument("--min-order", type=float, default=12.0,
                        help="Min order size USD (default: 12)")
    parser.add_argument("--sweep", action="store_true",
                        help="Run parameter sweep instead of single backtest")
    parser.add_argument("--verbose", action="store_true",
                        help="Print every fill")
    parser.add_argument("--project-root", type=str, default=".",
                        help="Project root directory")
    parser.add_argument("--export-fills", type=str, default=None,
                        help="Export fill log to CSV file")
    args = parser.parse_args()

    print()
    print("━" * 60)
    print(" HyperLiquidMM Offline Backtester")
    print("━" * 60)

    # Determine coins to test
    if args.all:
        coins = discover_coins(args.project_root)
        if not coins:
            log.error("No coins with tick data found. Run the engine in shadow mode first.")
            sys.exit(1)
        log.info(f"Found tick data for {len(coins)} coins: {', '.join(coins)}")
    elif args.coin:
        coins = [args.coin]
    else:
        log.error("Specify --coin COIN or --all")
        sys.exit(1)

    # Run backtests
    all_results = []

    for coin in coins:
        log.info(f"Loading tick data for {coin}...")
        ticks = load_ticks(coin, args.days, args.project_root)

        if not ticks:
            log.warning(f"No data for {coin}, skipping.")
            continue

        if args.sweep:
            # Estimate tick size from data
            mid = ticks[len(ticks)//2].mid_price
            if mid >= 10000:
                tick_size = 0.1
            elif mid >= 1000:
                tick_size = 0.01
            elif mid >= 10:
                tick_size = 0.001
            elif mid >= 1:
                tick_size = 0.0001
            else:
                tick_size = 0.000001

            results = run_parameter_sweep(coin, ticks, tick_size)
            all_results.extend(results)
            print_sweep_table(results)
        else:
            config = MmAssetConfig(
                asset=coin,
                tick_size=_estimate_tick(ticks),
                min_order_usd=args.min_order,
                max_inv_usd=args.max_inv,
                base_spread_bps=args.spread,
            )

            log.info(f"Running backtest: {coin} | spread={args.spread}bps | "
                     f"max_inv=${args.max_inv} | {len(ticks):,} ticks")

            result = run_backtest(coin, ticks, config, verbose=args.verbose)
            all_results.append(result)
            print_result(result)

    # Multi-coin summary
    if len(coins) > 1 and not args.sweep:
        print()
        print("=" * 100)
        print(" MULTI-COIN SUMMARY")
        print("=" * 100)
        for r in all_results:
            print(f"  {r.summary_line()}")
        print("-" * 100)
        total_pnl = sum(r.total_pnl_usd for r in all_results)
        total_vol = sum(r.total_volume_usd for r in all_results)
        total_fills = sum(r.total_fills for r in all_results)
        print(f"  PORTFOLIO | fills={total_fills} | vol=${total_vol:,.0f} | "
              f"PnL=${total_pnl:.4f}")
        print()


def _estimate_tick(ticks: list[TickData]) -> float:
    """Estimate tick size from mid price."""
    if not ticks:
        return 0.1
    mid = ticks[len(ticks)//2].mid_price
    if mid >= 10000:
        return 0.1
    elif mid >= 1000:
        return 0.01
    elif mid >= 10:
        return 0.001
    elif mid >= 1:
        return 0.0001
    else:
        return 0.000001


if __name__ == "__main__":
    main()
