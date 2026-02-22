#!/usr/bin/env python3
"""
auto_rank_coins.py — Automated Coin Ranking & Whitelist Updater
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Runs the backtester on ALL coins with tick data, ranks them by a composite
score (fill balance, fill rate, spread PnL, drawdown), and outputs the
recommended PRIORITY_COINS whitelist.

This should be run daily (or after accumulating 24h+ of tick data) to keep
the whitelist current. Coins that stop performing well get dropped, and
new high-performers get added.

Usage:
    # Rank all coins and print recommendations
    python3 scripts/auto_rank_coins.py

    # Auto-update the screener's PRIORITY_COINS list
    python3 scripts/auto_rank_coins.py --update-screener

    # Show top N coins
    python3 scripts/auto_rank_coins.py --top 10
"""

import os
import sys
import re
import json
import argparse
import logging
from datetime import datetime, timezone

# Add parent dir to path for imports
sys.path.insert(0, os.path.join(os.path.dirname(__file__)))

from backtest_replay import (
    load_ticks, run_backtest, MmAssetConfig, discover_coins
)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [RANKER] %(levelname)s %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
log = logging.getLogger("auto_rank")

# ── Ranking Weights ──────────────────────────────────────────────────────────
# Higher = better. Composite score is a weighted sum of normalized metrics.
WEIGHT_FILL_BALANCE = 30     # How equal bid/ask fills are (0=one-sided, 1=perfect)
WEIGHT_FILL_RATE = 25        # Fills per hour (more = more opportunities)
WEIGHT_SPREAD_PNL = 20       # Realized spread capture profit
WEIGHT_REBATES = 10          # Maker rebates earned
WEIGHT_LOW_DRAWDOWN = 15     # Inverse of max drawdown (less risk = better)

# Minimum thresholds to consider a coin
MIN_FILLS = 5                # Need at least 5 fills to be rankable
MIN_TICKS = 500              # Need at least 500 ticks of data

# $50 account defaults
DEFAULT_SPREAD_BPS = 2.0
DEFAULT_MAX_INV = 20.0
DEFAULT_MIN_ORDER = 12.0


def compute_fill_balance(bid_fills: int, ask_fills: int) -> float:
    """
    Score 0.0-1.0 for fill balance. 1.0 = perfect balance.
    Formula: 1 - abs(bids - asks) / (bids + asks)
    """
    total = bid_fills + ask_fills
    if total == 0:
        return 0.0
    imbalance = abs(bid_fills - ask_fills) / total
    return round(1.0 - imbalance, 4)


def rank_coins(project_root=".", days=1, spread=DEFAULT_SPREAD_BPS,
               max_inv=DEFAULT_MAX_INV, min_order=DEFAULT_MIN_ORDER):
    """
    Backtest all coins, compute composite ranking score, return sorted list.
    """
    coins = discover_coins(project_root)
    if not coins:
        log.error("No coins with tick data found.")
        return []

    log.info(f"Found {len(coins)} coins with tick data. Running backtests...")

    results = []
    for coin in coins:
        ticks = load_ticks(coin, days, project_root)
        if len(ticks) < MIN_TICKS:
            log.debug(f"  {coin}: skipped ({len(ticks)} ticks < {MIN_TICKS})")
            continue

        config = MmAssetConfig(
            asset=coin,
            tick_size=0.0001,  # Will be approximate; OK for ranking
            min_order_usd=min_order,
            max_inv_usd=max_inv,
            base_spread_bps=spread,
        )

        result = run_backtest(coin, ticks, config)
        if result.total_fills < MIN_FILLS:
            log.debug(f"  {coin}: skipped ({result.total_fills} fills < {MIN_FILLS})")
            continue

        results.append(result)

    if not results:
        log.error("No coins passed minimum thresholds.")
        return []

    # ── Normalize each metric to 0-1 range ────────────────────────────────
    max_fill_rate = max(r.avg_fill_rate_per_hour for r in results) or 1
    max_spread_pnl = max(r.spread_pnl_usd for r in results) or 0.01
    max_rebates = max(r.total_rebates_usd for r in results) or 0.001
    max_drawdown = max(r.max_drawdown_usd for r in results) or 1

    ranked = []
    for r in results:
        balance = compute_fill_balance(r.bid_fills, r.ask_fills)
        fill_rate_norm = r.avg_fill_rate_per_hour / max_fill_rate
        spread_pnl_norm = max(0, r.spread_pnl_usd) / max_spread_pnl
        rebates_norm = r.total_rebates_usd / max_rebates
        drawdown_norm = 1.0 - (r.max_drawdown_usd / max_drawdown) if max_drawdown > 0 else 0

        composite = (
            WEIGHT_FILL_BALANCE * balance +
            WEIGHT_FILL_RATE * fill_rate_norm +
            WEIGHT_SPREAD_PNL * spread_pnl_norm +
            WEIGHT_REBATES * rebates_norm +
            WEIGHT_LOW_DRAWDOWN * drawdown_norm
        )

        ranked.append({
            "coin": r.coin,
            "composite_score": round(composite, 2),
            "fill_balance": balance,
            "fills": r.total_fills,
            "bid_fills": r.bid_fills,
            "ask_fills": r.ask_fills,
            "fill_rate": round(r.avg_fill_rate_per_hour, 1),
            "spread_pnl": round(r.spread_pnl_usd, 4),
            "rebates": round(r.total_rebates_usd, 4),
            "max_drawdown": round(r.max_drawdown_usd, 2),
            "total_pnl": round(r.total_pnl_usd, 4),
            "max_inventory": round(r.max_inventory_usd, 2),
        })

    ranked.sort(key=lambda x: x["composite_score"], reverse=True)
    return ranked


def print_ranking_table(ranked, top_n=15):
    """Pretty-print the composite ranking."""
    print()
    print("=" * 110)
    print(f" COIN RANKING — {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M UTC')}")
    print("=" * 110)
    print(f" {'#':>2}  {'Coin':<8}  {'Score':>6}  {'Balance':>7}  "
          f"{'Fills':>6}  {'B/A':>7}  {'Rate/hr':>7}  "
          f"{'SpreadPnL':>10}  {'Rebates':>8}  {'MaxDD':>7}  {'Verdict':<12}")
    print("-" * 110)

    for i, r in enumerate(ranked[:top_n], 1):
        # Verdict based on composite score + fill balance
        if r["composite_score"] >= 60 and r["fill_balance"] >= 0.8:
            verdict = "★ PRIORITY"
        elif r["composite_score"] >= 40 and r["fill_balance"] >= 0.6:
            verdict = "✓ Good"
        elif r["fill_balance"] < 0.5:
            verdict = "⚠ Imbalanced"
        else:
            verdict = "○ Marginal"

        print(f" {i:>2}  {r['coin']:<8}  {r['composite_score']:>6.1f}  "
              f"{r['fill_balance']:>6.1%}  "
              f"{r['fills']:>6}  {r['bid_fills']}B/{r['ask_fills']}A  "
              f"{r['fill_rate']:>7.1f}  "
              f"${r['spread_pnl']:>9.4f}  ${r['rebates']:>7.4f}  "
              f"${r['max_drawdown']:>6.2f}  {verdict}")

    print("-" * 110)

    # Summary
    priority = [r for r in ranked if r["composite_score"] >= 60 and r["fill_balance"] >= 0.8]
    if priority:
        names = [r["coin"] for r in priority]
        print(f"\n ★ Recommended PRIORITY_COINS: {{{', '.join(repr(n) for n in names)}}}")
    print()


def update_screener_whitelist(ranked, screener_path, top_n=5):
    """
    Auto-update PRIORITY_COINS in mm_asset_screener.py based on ranking.
    Only coins with composite_score >= 60 AND fill_balance >= 0.8 qualify.
    """
    qualified = [r["coin"] for r in ranked
                 if r["composite_score"] >= 60 and r["fill_balance"] >= 0.8][:top_n]

    if not qualified:
        log.warning("No coins qualified for priority status. Keeping existing list.")
        return False

    new_set = "{" + ", ".join(f'"{c}"' for c in qualified) + "}"
    new_line = f'PRIORITY_COINS = {new_set}       # Auto-updated {datetime.now(timezone.utc).strftime("%Y-%m-%d")}\n'

    # Read screener file
    with open(screener_path, 'r') as f:
        content = f.read()

    # Replace the PRIORITY_COINS line
    pattern = r'^PRIORITY_COINS\s*=\s*\{[^}]*\}.*$'
    new_content, count = re.subn(pattern, new_line.rstrip(), content, count=1, flags=re.MULTILINE)

    if count == 0:
        log.error("Could not find PRIORITY_COINS line in screener file.")
        return False

    with open(screener_path, 'w') as f:
        f.write(new_content)

    log.info(f"Updated PRIORITY_COINS to: {new_set}")
    return True


def publish_priority_to_redis(ranked, redis_url, top_n=5):
    """
    Publish qualified priority coins to Redis key mm:priority_coins.
    The screener reads this key each cycle to boost proven performers.
    """
    qualified = [r["coin"] for r in ranked
                 if r["composite_score"] >= 60 and r["fill_balance"] >= 0.8][:top_n]

    if not qualified:
        log.warning("No coins qualified for priority. Not updating Redis.")
        return False

    try:
        import redis as redis_lib
        r = redis_lib.Redis.from_url(redis_url, decode_responses=True)
        r.ping()

        payload = json.dumps({
            "coins": qualified,
            "updated_at": datetime.now(timezone.utc).isoformat(),
            "scores": {c["coin"]: c["composite_score"] for c in ranked
                       if c["coin"] in qualified},
        })
        r.set("mm:priority_coins", payload)
        log.info(f"Published priority coins to Redis: {qualified}")
        return True
    except Exception as e:
        log.error(f"Redis publish failed: {e}")
        return False


def main():
    parser = argparse.ArgumentParser(
        description="Rank coins by backtester performance and update whitelist"
    )
    parser.add_argument("--days", type=int, default=1,
                        help="Days of tick data to backtest (default: 1)")
    parser.add_argument("--top", type=int, default=15,
                        help="Show top N coins (default: 15)")
    parser.add_argument("--spread", type=float, default=DEFAULT_SPREAD_BPS,
                        help=f"Spread in bps (default: {DEFAULT_SPREAD_BPS})")
    parser.add_argument("--max-inv", type=float, default=DEFAULT_MAX_INV,
                        help=f"Max inventory USD (default: {DEFAULT_MAX_INV})")
    parser.add_argument("--update-screener", action="store_true",
                        help="Auto-update PRIORITY_COINS in mm_asset_screener.py")
    parser.add_argument("--redis-url", type=str,
                        default=os.environ.get("REDIS_URL", "redis://127.0.0.1:6380"),
                        help="Redis URL to publish priority coins")
    parser.add_argument("--project-root", type=str, default=".",
                        help="Project root directory")
    args = parser.parse_args()

    print()
    print("━" * 60)
    print(" HyperLiquidMM — Automated Coin Ranker")
    print("━" * 60)

    ranked = rank_coins(
        project_root=args.project_root,
        days=args.days,
        spread=args.spread,
        max_inv=args.max_inv,
    )

    if not ranked:
        return

    print_ranking_table(ranked, args.top)

    if args.update_screener:
        screener_path = os.path.join(
            args.project_root, "scripts", "mm_asset_screener.py"
        )
        if os.path.exists(screener_path):
            update_screener_whitelist(ranked, screener_path)
        else:
            log.error(f"Screener not found at {screener_path}")

    # Always publish to Redis when available
    publish_priority_to_redis(ranked, args.redis_url)


if __name__ == "__main__":
    main()
