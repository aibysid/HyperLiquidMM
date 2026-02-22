#!/usr/bin/env python3
"""
mm_asset_screener.py — Phase 9H: Maker Edge Screener
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Runs in a loop (every 60s). Fetches live market data from Hyperliquid,
scores coins by Maker Edge, applies filters, and publishes per-coin
MmAssetConfig JSON to Redis for the Rust engine to consume.

Usage:
    python3 mm_asset_screener.py                  # Run in loop (production)
    python3 mm_asset_screener.py --once            # Run once and exit
    python3 mm_asset_screener.py --dry-run         # Print configs, don't publish
    python3 mm_asset_screener.py --dry-run --top 10  # Show top 10 only

Redis channel: mm:asset_config
MmAssetConfig JSON must exactly match the Rust struct in market_maker.rs.
"""

import os
import sys
import json
import math
import time
import signal
import logging
import argparse
import glob
from pathlib import Path
from datetime import datetime, timedelta, timezone
from collections import defaultdict

import requests

# ─── Constants ──────────────────────────────────────────────────────────────────

API_URL = "https://api.hyperliquid.xyz/info"

# Redis channel the Rust engine subscribes to
CHANNEL_ASSET_CONFIG = "mm:asset_config"
CHANNEL_ENGINE_STATUS = "mm:engine_status"

# Default screening parameters
DEFAULT_MIN_VOLUME_USD = 1_000_000      # $1M daily volume minimum (mid-tier sweet spot)
DEFAULT_MAX_COINS = 3                    # Max coins (limited by $50 account)
DEFAULT_CORRELATION_THRESHOLD = 0.85     # Price correlation cutoff
DEFAULT_MAX_PER_CLUSTER = 1              # Max coins from one correlated group (tight for small acct)
DEFAULT_MIN_SPREAD_BPS = 0.5             # Minimum viable spread (exchange floor)
DEFAULT_MAX_INV_USD = 20.0               # Max inventory per coin ($50 total / 3 coins ≈ $16, with buffer)
DEFAULT_MIN_ORDER_USD = 10.0             # Minimum order size (Hyperliquid floor ~$10)

# Tick data directories (relative to project root)
TICK_CSV_DIR = "data/ticks"              # From engine harvester (CSV)
TICK_PARQUET_DIR = "data/parquet"        # From csv_to_parquet.py

# Publish interval
LOOP_INTERVAL_SECS = 60

# ─── Logging ────────────────────────────────────────────────────────────────────

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [SCREENER] %(levelname)s %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("mm_screener")

# ─── Graceful shutdown ──────────────────────────────────────────────────────────

_shutdown = False

def _signal_handler(sig, frame):
    global _shutdown
    log.info("Received shutdown signal. Exiting after current cycle.")
    _shutdown = True

signal.signal(signal.SIGINT, _signal_handler)
signal.signal(signal.SIGTERM, _signal_handler)


# ═══════════════════════════════════════════════════════════════════════════════
# 1. DATA FETCHING
# ═══════════════════════════════════════════════════════════════════════════════

def fetch_meta_and_ctx():
    """
    Fetches metaAndAssetCtxs from Hyperliquid REST API.
    Returns list of dicts: [{ name, tick_size, daily_volume, mid_price,
                              spread_bps, funding_rate, open_interest }]
    """
    resp = requests.post(API_URL, json={"type": "metaAndAssetCtxs"}, timeout=15)
    resp.raise_for_status()
    data = resp.json()

    if not isinstance(data, list) or len(data) < 2:
        raise ValueError(f"Unexpected API response shape: {type(data)}")

    universe = data[0].get("universe", [])
    ctxs = data[1] if isinstance(data[1], list) else []

    coins = []
    for i, asset in enumerate(universe):
        if i >= len(ctxs):
            break

        ctx = ctxs[i]
        name = asset.get("name", "")
        if not name:
            continue

        # Parse tick size from szDecimals (Hyperliquid uses szDecimals for size,
        # but price tick is derived differently — we use the actual price precision)
        sz_decimals = asset.get("szDecimals", 0)

        # Market data from ctx
        mid_price = _parse_float(ctx.get("midPx", ctx.get("markPx", "0")))
        daily_volume = _parse_float(ctx.get("dayNtlVlm", "0"))
        funding_rate = _parse_float(ctx.get("funding", "0"))
        open_interest = _parse_float(ctx.get("openInterest", "0"))
        oracle_px = _parse_float(ctx.get("oraclePx", "0"))

        # Compute spread from prevDayPx and markPx if available
        mark_px = _parse_float(ctx.get("markPx", "0"))

        # Tick size: Hyperliquid perps have price precision that varies.
        # We approximate tick size from the price level.
        tick_size = _estimate_tick_size(mid_price)

        # Spread: We'll compute from the l2Book later if available.
        # For screening, use a rough proxy: tick_size / mid_price * 10000 bps
        # This will be refined with historical tick data below.
        estimated_spread_bps = (tick_size / mid_price * 10_000) if mid_price > 0 else 999.0

        coins.append({
            "name": name,
            "tick_size": tick_size,
            "sz_decimals": sz_decimals,
            "mid_price": mid_price,
            "daily_volume": daily_volume,
            "funding_rate": funding_rate,
            "open_interest": open_interest,
            "oracle_px": oracle_px,
            "estimated_spread_bps": estimated_spread_bps,
        })

    log.info(f"Fetched {len(coins)} assets from Hyperliquid universe.")
    return coins


def fetch_l2_spreads(coin_names, max_coins=50):
    """
    Fetches current L2 book for a subset of coins to get actual bid-ask spreads.
    Returns dict: { coin: spread_bps }.
    """
    spreads = {}
    for coin in coin_names[:max_coins]:
        try:
            resp = requests.post(
                API_URL,
                json={"type": "l2Book", "coin": coin},
                timeout=5,
            )
            resp.raise_for_status()
            book = resp.json()

            levels = book.get("levels", [])
            if len(levels) >= 2:
                bids = levels[0]
                asks = levels[1]
                if bids and asks:
                    best_bid = _parse_float(bids[0].get("px", "0"))
                    best_ask = _parse_float(asks[0].get("px", "0"))
                    if best_bid > 0 and best_ask > 0:
                        mid = (best_bid + best_ask) / 2
                        spread_bps = (best_ask - best_bid) / mid * 10_000
                        spreads[coin] = spread_bps
        except Exception as e:
            log.debug(f"L2 fetch failed for {coin}: {e}")
            continue

        # Small delay to avoid rate limiting
        time.sleep(0.05)

    return spreads


def _parse_float(val):
    """Safely parse a string/number to float."""
    if isinstance(val, (int, float)):
        return float(val)
    try:
        return float(str(val))
    except (ValueError, TypeError):
        return 0.0


def _estimate_tick_size(mid_price):
    """
    Estimate the price tick size based on the price level.
    Hyperliquid tick sizes vary by asset, this is a reasonable approximation.
    """
    if mid_price <= 0:
        return 0.01
    elif mid_price >= 10_000:
        return 0.1          # BTC-class
    elif mid_price >= 1_000:
        return 0.01         # ETH-class
    elif mid_price >= 100:
        return 0.01
    elif mid_price >= 10:
        return 0.001
    elif mid_price >= 1:
        return 0.0001
    elif mid_price >= 0.01:
        return 0.000001
    else:
        return 0.00000001   # Micro-cap memecoins


# ═══════════════════════════════════════════════════════════════════════════════
# 2. MAKER EDGE SCORING
# ═══════════════════════════════════════════════════════════════════════════════

def compute_maker_edge(spread_bps, tick_size, mid_price, daily_volume):
    """
    Maker Edge Score — ranks how profitable it is to provide liquidity.

    Components:
      1. spread_value: wider natural spread = more gross profit per fill.
         Uses a smooth curve so even tight spreads (0.1-1bps) get non-zero scores,
         while wider spreads (3-8bps) get proportionally rewarded.
      2. tick_granularity: smaller tick/spread ratio = can place precise quotes
         inside the spread. If tick_bps ≈ spread_bps, there's no room.
      3. volume_factor: log10(volume) — enough flow to get fills, capped benefit.
         Mid-tier ($1M-$100M) is the sweet spot for a small MM.
      4. competition_discount: ultra-high volume coins face HFT competition.
         Discount BTC/ETH-class volumes to prefer mid-tier.

    Returns: float score (higher = better for market making)
    """
    if mid_price <= 0 or spread_bps <= 0:
        return 0.0

    tick_bps = (tick_size / mid_price) * 10_000

    # 1. Spread value: smooth curve — even 0.5bps gives ~0.4, while 5bps gives ~3.5
    #    Formula: spread * (1 - e^(-spread/2)) → saturates gently
    spread_value = spread_bps * (1.0 - math.exp(-spread_bps / 2.0))

    # 2. Tick granularity: can we quote inside the spread?
    #    ratio close to 0 = very granular = good
    #    ratio close to 1 = tick ≈ spread = can't improve price = bad
    tick_ratio = min(1.0, tick_bps / spread_bps) if spread_bps > 0 else 1.0
    granularity_factor = max(0.1, 1.0 - tick_ratio)  # Floor at 0.1 (never zero)

    # 3. Volume factor (log scale, floor at $100K)
    volume_factor = math.log10(max(daily_volume, 100_000))

    # 4. Competition discount: penalize ultra-high volume where HFTs dominate
    #    $1B+ volume → 0.5x multiplier, $100M → 0.8x, $10M → 1.0x, $1M → 0.9x
    if daily_volume > 500_000_000:
        competition = 0.5
    elif daily_volume > 100_000_000:
        competition = 0.7
    elif daily_volume > 50_000_000:
        competition = 0.85
    elif daily_volume > 1_000_000:
        competition = 1.0  # Sweet spot: enough volume, less competition
    else:
        competition = 0.8  # Too low volume: fills are rare

    score = spread_value * granularity_factor * volume_factor * competition
    return round(score, 4)


# ═══════════════════════════════════════════════════════════════════════════════
# 3. FILTERS
# ═══════════════════════════════════════════════════════════════════════════════

def filter_dead_coins(coins, min_volume=DEFAULT_MIN_VOLUME_USD):
    """Remove coins with daily notional volume below threshold."""
    before = len(coins)
    result = [c for c in coins if c["daily_volume"] >= min_volume]
    removed = before - len(result)
    if removed > 0:
        log.info(f"Dead coin filter: removed {removed} coins (volume < ${min_volume/1e6:.0f}M)")
    return result


def filter_correlated(scored_coins, threshold=DEFAULT_CORRELATION_THRESHOLD,
                      max_per_cluster=DEFAULT_MAX_PER_CLUSTER):
    """
    Lightweight correlation filter using price-level clustering.

    Since we don't have historical price data for all coins at screening time,
    we use a proxy: group coins by price magnitude order (coins at similar price
    levels tend to be correlated — e.g., mid-cap alts around $1–$10).

    For a more robust filter, we'd use rolling returns correlation from tick data.
    This is a reasonable Phase 1 approximation.

    Also groups by known correlation families (e.g., memecoins, L1s, DeFi tokens).
    """
    # Known correlation families (manually curated, expand as needed)
    FAMILIES = {
        "meme": {"DOGE", "SHIB", "PEPE", "WIF", "BONK", "FLOKI", "MEME", "MYRO",
                 "TURBO", "MOG", "NEIRO", "PNUT", "GOAT", "POPCAT", "BRETT", "SPX",
                 "MOODENG", "FARTCOIN"},
        "l1_alt": {"ADA", "DOT", "AVAX", "NEAR", "APT", "SUI", "SEI", "INJ",
                   "TIA", "FTM", "ALGO", "ATOM", "ICP"},
        "defi": {"UNI", "AAVE", "MKR", "COMP", "CRV", "SUSHI", "SNX", "DYDX",
                 "PENDLE", "JUP", "RAY"},
        "l2": {"ARB", "OP", "STRK", "MANTA", "BLAST", "SCROLL", "ZK", "METIS"},
        "ai": {"FET", "RENDER", "TAO", "ONDO", "WLD", "ARKM", "AI16Z", "VIRTUAL",
               "GRIFFAIN"},
    }

    # Build reverse lookup
    coin_to_family = {}
    for family, members in FAMILIES.items():
        for member in members:
            coin_to_family[member] = family

    # Group by family
    family_groups = defaultdict(list)
    ungrouped = []

    for coin in scored_coins:
        name = coin["name"]
        family = coin_to_family.get(name)
        if family:
            family_groups[family].append(coin)
        else:
            ungrouped.append(coin)

    # From each family, keep only top N by maker_edge score
    result = list(ungrouped)  # Ungrouped coins always pass
    total_removed = 0
    for family, members in family_groups.items():
        # Already sorted by score (scored_coins is sorted)
        kept = members[:max_per_cluster]
        removed = members[max_per_cluster:]
        result.extend(kept)
        total_removed += len(removed)
        if removed:
            removed_names = [c["name"] for c in removed]
            log.debug(f"Correlation filter [{family}]: kept {len(kept)}, "
                      f"removed {removed_names}")

    if total_removed > 0:
        log.info(f"Correlation filter: removed {total_removed} redundant coins "
                 f"from {len(family_groups)} families")

    # Re-sort by score
    result.sort(key=lambda c: c.get("maker_edge", 0), reverse=True)
    return result


# ═══════════════════════════════════════════════════════════════════════════════
# 4. SPREAD CALIBRATION FROM TICK DATA
# ═══════════════════════════════════════════════════════════════════════════════

def compute_spread_config(coin_name, project_root=".", lookback_days=5):
    """
    If historical tick data exists for this coin, compute optimal base_spread_bps
    from the 25th percentile of observed spreads (inside the market ~75% of time).

    Falls back to the live market spread × 0.8 if no tick data.
    """
    # Try Parquet first (faster), then CSV
    parquet_dir = os.path.join(project_root, TICK_PARQUET_DIR, coin_name)
    csv_dir = os.path.join(project_root, TICK_CSV_DIR, coin_name)

    spread_values = []

    # Look for recent files (last N days)
    for days_ago in range(lookback_days):
        date_str = (datetime.now(timezone.utc) - timedelta(days=days_ago)).strftime("%Y-%m-%d")

        # Try Parquet
        parquet_file = os.path.join(parquet_dir, f"{date_str}.parquet")
        if os.path.exists(parquet_file):
            try:
                spreads = _read_spreads_from_parquet(parquet_file)
                spread_values.extend(spreads)
                continue
            except Exception as e:
                log.debug(f"Parquet read failed for {coin_name}/{date_str}: {e}")

        # Try CSV
        csv_file = os.path.join(csv_dir, f"{date_str}.csv")
        if os.path.exists(csv_file):
            try:
                spreads = _read_spreads_from_csv(csv_file)
                spread_values.extend(spreads)
            except Exception as e:
                log.debug(f"CSV read failed for {coin_name}/{date_str}: {e}")

    if not spread_values:
        return None  # No historical data — caller uses live estimate

    # P25 of observed spreads: inside the market ~75% of the time
    spread_values.sort()
    p25_idx = max(0, int(len(spread_values) * 0.25))
    p25_spread = spread_values[p25_idx]

    # Minimum viable spread: at least 0.5 bps (exchange minimum)
    calibrated = max(DEFAULT_MIN_SPREAD_BPS, round(p25_spread, 2))

    log.debug(f"  {coin_name}: {len(spread_values)} ticks, "
              f"P25={p25_spread:.2f}bps → base_spread={calibrated:.2f}bps")

    return calibrated


def _read_spreads_from_csv(csv_path, max_rows=500_000):
    """Read spread_bps column from a tick CSV file."""
    spreads = []
    with open(csv_path, 'r') as f:
        for i, line in enumerate(f):
            if i >= max_rows:
                break
            parts = line.strip().split(',')
            if len(parts) >= 6:
                try:
                    spread = float(parts[5])
                    if 0 < spread < 100:  # Sanity: 0–100 bps
                        spreads.append(spread)
                except (ValueError, IndexError):
                    continue
    return spreads


def _read_spreads_from_parquet(parquet_path):
    """Read spread_bps column from a Parquet file using DuckDB."""
    try:
        import duckdb
        con = duckdb.connect()
        result = con.execute(
            f"SELECT spread_bps FROM read_parquet('{parquet_path}') "
            f"WHERE spread_bps > 0 AND spread_bps < 100"
        ).fetchall()
        return [row[0] for row in result]
    except Exception:
        return []


# ═══════════════════════════════════════════════════════════════════════════════
# 5. CONFIG BUILDING
# ═══════════════════════════════════════════════════════════════════════════════

def build_configs(ranked_coins, live_spreads, project_root="."):
    """
    Build Vec<MmAssetConfig> matching the Rust struct exactly.

    Fields: asset, tick_size, min_order_usd, max_inv_usd,
            base_spread_bps, atr_fraction, regime
    """
    configs = []
    for coin in ranked_coins:
        name = coin["name"]
        mid_price = coin["mid_price"]
        funding_rate = coin["funding_rate"]

        # ── base_spread_bps ────────────────────────────────────────────────
        # Priority: historical tick data > L2 live spread > estimated spread
        calibrated_spread = compute_spread_config(name, project_root)

        if calibrated_spread is not None:
            base_spread_bps = calibrated_spread
            spread_source = "tick_data"
        elif name in live_spreads:
            # Use live L2 spread × 0.8 (quote inside the market)
            base_spread_bps = max(DEFAULT_MIN_SPREAD_BPS,
                                  round(live_spreads[name] * 0.8, 2))
            spread_source = "live_l2"
        else:
            # Fallback to estimated
            base_spread_bps = max(DEFAULT_MIN_SPREAD_BPS,
                                  round(coin["estimated_spread_bps"] * 0.8, 2))
            spread_source = "estimated"

        # Cap spread at 10 bps — wider than this, fills are too rare
        base_spread_bps = min(base_spread_bps, 10.0)

        # ── regime ─────────────────────────────────────────────────────────
        regime = "calm"
        if abs(funding_rate) > 0.0005:     # |funding| > 0.05%
            regime = "uncertain"
        if abs(funding_rate) > 0.001:      # |funding| > 0.1%
            regime = "halt"

        # ── max_inv_usd ───────────────────────────────────────────────────
        # $50 account: tight inventory caps. ~$15-20 per coin max.
        max_inv = DEFAULT_MAX_INV_USD  # $20 — keeps total exposure ≤ $50 across 3 coins

        # ── atr_fraction ──────────────────────────────────────────────────
        # Rough proxy from spread: higher spread → higher volatility
        atr_fraction = round(base_spread_bps / 10_000 * 1.5, 6)

        config = {
            "asset": name,
            "tick_size": coin["tick_size"],
            "min_order_usd": DEFAULT_MIN_ORDER_USD,
            "max_inv_usd": max_inv,
            "base_spread_bps": base_spread_bps,
            "atr_fraction": atr_fraction,
            "regime": regime,
        }
        configs.append(config)

        log.debug(f"  {name:>8s}: spread={base_spread_bps:.1f}bps ({spread_source}) "
                  f"regime={regime} max_inv=${max_inv:.0f}")

    return configs


# ═══════════════════════════════════════════════════════════════════════════════
# 6. REDIS PUBLISHING
# ═══════════════════════════════════════════════════════════════════════════════

def publish_to_redis(redis_url, configs):
    """
    Publish Vec<MmAssetConfig> as JSON to the mm:asset_config Redis channel.
    This is what the Rust engine's MmScreenerSubscriber listens on.
    """
    try:
        import redis as redis_lib
        r = redis_lib.Redis.from_url(redis_url, decode_responses=True)
        r.ping()
        payload = json.dumps(configs)
        receivers = r.publish(CHANNEL_ASSET_CONFIG, payload)
        log.info(f"Published {len(configs)} configs to Redis "
                 f"({len(payload)} bytes, {receivers} subscriber(s))")
        return True
    except Exception as e:
        log.warning(f"Redis publish failed: {e}")
        return False


def subscribe_engine_status(redis_url):
    """
    Subscribe to engine status channel for monitoring.
    Runs in the main thread between publish cycles.
    Returns latest status dict or None.
    """
    try:
        import redis as redis_lib
        r = redis_lib.Redis.from_url(redis_url, decode_responses=True)
        # Use a non-blocking GET on a key (the publisher also SETs)
        status = r.get("mm:latest_engine_status")
        if status:
            return json.loads(status)
    except Exception:
        pass
    return None


# ═══════════════════════════════════════════════════════════════════════════════
# 7. MAIN ORCHESTRATOR
# ═══════════════════════════════════════════════════════════════════════════════

def run_screening_cycle(args, project_root="."):
    """
    Single screening cycle:
      1. Fetch market data
      2. Filter dead coins
      3. Get live L2 spreads for candidates
      4. Score by Maker Edge
      5. Filter correlated
      6. Build configs
      7. Publish (or print if dry-run)

    Returns: list of MmAssetConfig dicts
    """
    # 1. Fetch universe
    coins = fetch_meta_and_ctx()

    # 2. Filter dead coins
    coins = filter_dead_coins(coins, min_volume=args.min_volume)

    if not coins:
        log.warning("No coins passed the volume filter!")
        return []

    # 3. Get live L2 spreads for better scoring
    coin_names = [c["name"] for c in coins]
    log.info(f"Fetching L2 spreads for {min(len(coin_names), 50)} candidate coins...")
    live_spreads = fetch_l2_spreads(coin_names, max_coins=50)
    log.info(f"Got live spreads for {len(live_spreads)} coins.")

    # Update coin spread estimates with live data where available
    for coin in coins:
        if coin["name"] in live_spreads:
            coin["estimated_spread_bps"] = live_spreads[coin["name"]]

    # 4. Score by Maker Edge
    for coin in coins:
        coin["maker_edge"] = compute_maker_edge(
            spread_bps=coin["estimated_spread_bps"],
            tick_size=coin["tick_size"],
            mid_price=coin["mid_price"],
            daily_volume=coin["daily_volume"],
        )

    # Sort by score descending
    coins.sort(key=lambda c: c["maker_edge"], reverse=True)

    # 5. Filter correlated
    coins = filter_correlated(coins,
                              threshold=args.correlation_threshold,
                              max_per_cluster=args.max_per_cluster)

    # 6. Take top N
    selected = coins[:args.top]

    # 7. Build configs
    configs = build_configs(selected, live_spreads, project_root)

    return configs


def print_scoring_table(coins, configs):
    """Pretty-print the scoring results for dry-run mode."""
    print()
    print("=" * 90)
    print(f" MAKER EDGE SCREENER — {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M UTC')}")
    print("=" * 90)
    print(f" {'#':>2}  {'Coin':<8}  {'Price':>10}  {'Volume':>12}  "
          f"{'Spread':>8}  {'Score':>8}  {'Config Spread':>14}  {'Regime':<10}")
    print("-" * 90)

    for i, config in enumerate(configs, 1):
        # Find matching coin data
        coin = next((c for c in coins if c["name"] == config["asset"]), {})
        vol_str = f"${coin.get('daily_volume', 0)/1e6:.1f}M"
        print(f" {i:>2}  {config['asset']:<8}  "
              f"${coin.get('mid_price', 0):>9.2f}  "
              f"{vol_str:>12}  "
              f"{coin.get('estimated_spread_bps', 0):>7.2f}bp  "
              f"{coin.get('maker_edge', 0):>8.2f}  "
              f"{config['base_spread_bps']:>13.2f}bp  "
              f"{config['regime']:<10}")

    print("-" * 90)
    print(f" Total: {len(configs)} coins selected")
    print()


def main():
    parser = argparse.ArgumentParser(
        description="Phase 9H: Maker Edge Screener for HyperLiquidMM"
    )
    parser.add_argument("--dry-run", action="store_true",
                        help="Print configs to stdout, don't publish to Redis")
    parser.add_argument("--once", action="store_true",
                        help="Run once and exit (don't loop)")
    parser.add_argument("--top", type=int, default=DEFAULT_MAX_COINS,
                        help=f"Max coins to select (default: {DEFAULT_MAX_COINS})")
    parser.add_argument("--min-volume", type=float, default=DEFAULT_MIN_VOLUME_USD,
                        help=f"Min daily volume in USD (default: {DEFAULT_MIN_VOLUME_USD/1e6:.0f}M)")
    parser.add_argument("--correlation-threshold", type=float,
                        default=DEFAULT_CORRELATION_THRESHOLD,
                        help=f"Correlation cutoff (default: {DEFAULT_CORRELATION_THRESHOLD})")
    parser.add_argument("--max-per-cluster", type=int,
                        default=DEFAULT_MAX_PER_CLUSTER,
                        help=f"Max coins per correlated group (default: {DEFAULT_MAX_PER_CLUSTER})")
    parser.add_argument("--redis-url", type=str,
                        default=os.environ.get("REDIS_URL", "redis://127.0.0.1:6380"),
                        help="Redis connection URL")
    parser.add_argument("--project-root", type=str, default=".",
                        help="Project root directory (for tick data)")
    args = parser.parse_args()

    log.info("=" * 60)
    log.info("Phase 9H: Maker Edge Screener starting")
    log.info(f"  Mode:        {'dry-run' if args.dry_run else 'once' if args.once else 'loop'}")
    log.info(f"  Max coins:   {args.top}")
    log.info(f"  Min volume:  ${args.min_volume/1e6:.0f}M")
    log.info(f"  Redis URL:   {args.redis_url}")
    log.info(f"  Project root: {os.path.abspath(args.project_root)}")
    log.info("=" * 60)

    cycle = 0
    while not _shutdown:
        cycle += 1
        log.info(f"─── Screening cycle #{cycle} ───")

        try:
            configs = run_screening_cycle(args, args.project_root)

            if not configs:
                log.warning("No configs generated this cycle.")
            else:
                if args.dry_run:
                    # Fetch coins again for display (we don't persist them from the cycle)
                    coins = fetch_meta_and_ctx()
                    coins = filter_dead_coins(coins, min_volume=args.min_volume)
                    for c in coins:
                        live_spread = next((cfg["base_spread_bps"]
                                           for cfg in configs if cfg["asset"] == c["name"]), None)
                        c["maker_edge"] = compute_maker_edge(
                            c["estimated_spread_bps"], c["tick_size"],
                            c["mid_price"], c["daily_volume"])
                    print_scoring_table(coins, configs)
                    print("JSON output:")
                    print(json.dumps(configs, indent=2))
                else:
                    publish_to_redis(args.redis_url, configs)

                    # Log summary
                    coin_list = ", ".join(c["asset"] for c in configs[:10])
                    if len(configs) > 10:
                        coin_list += f" (+{len(configs) - 10} more)"
                    log.info(f"Selected: {coin_list}")

        except requests.exceptions.RequestException as e:
            log.error(f"API request failed: {e}")
        except Exception as e:
            log.error(f"Screening cycle failed: {e}", exc_info=True)

        if args.once or args.dry_run:
            break

        log.info(f"Sleeping {LOOP_INTERVAL_SECS}s until next cycle...")
        # Interruptible sleep
        for _ in range(LOOP_INTERVAL_SECS):
            if _shutdown:
                break
            time.sleep(1)

    log.info("Screener stopped.")


if __name__ == "__main__":
    main()
