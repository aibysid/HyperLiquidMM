#!/usr/bin/env python3
"""
analyze_ticks.py — DuckDB-powered tick data analysis
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Reads Parquet files directly — no server, no ETL.

Usage:
  python3 scripts/analyze_ticks.py                  # full analysis
  python3 scripts/analyze_ticks.py --coin BTC        # single coin
  python3 scripts/analyze_ticks.py --gate 1          # just gate 1 check
"""

import os
import sys
import argparse
import datetime
import duckdb

PARQUET_DIR = "data/parquet"
CSV_DIR     = "backend/mm-engine-rs/data/ticks"  # fallback if parquet not ready

# ─── DuckDB connection (in-memory, reads parquet directly) ────────────────────
con = duckdb.connect()

def parquet_glob(coin: str = "*") -> str:
    """Returns the glob pattern for parquet files for one or all coins."""
    return f"{PARQUET_DIR}/{coin}/*.parquet"

def csv_glob(coin: str = "*") -> str:
    return f"{CSV_DIR}/{coin}/*.csv"

def source_exists_parquet() -> bool:
    for root, dirs, files in os.walk(PARQUET_DIR):
        if any(f.endswith(".parquet") for f in files):
            return True
    return False

def get_source(coin: str = "*") -> tuple[str, str]:
    """Returns (source_glob, format_name) — prefers Parquet, falls back to CSV."""
    if source_exists_parquet():
        return parquet_glob(coin), "Parquet"
    else:
        # DuckDB can read CSV directly too — slower but works before conversion
        return csv_glob(coin), "CSV (convert to Parquet for 10x faster queries)"


# ─── Gate 1: Adverse Selection Baseline ───────────────────────────────────────
def gate1_adverse_selection(coin_filter: str = "*"):
    """
    Key question: Is our default 1.5bps spread wider than the adverse selection we face?
    Adverse selection proxy: did mid price move against our simulated fill direction
    in the 30 seconds after the tick?
    
    We compute: avg_spread_bps vs avg(|mid_t+30s - mid_t|) / mid_t * 10000
    If spread > adverse_selection_cost → we have edge.
    """
    src, fmt = get_source(coin_filter)
    print(f"\n{'━'*65}")
    print(f"GATE 1 — Adverse Selection Baseline  [{fmt}]")
    print('━'*65)

    try:
        result = con.execute(f"""
            WITH ticks AS (
                SELECT 
                    ts_ms, coin, mid, spread_bps,
                    LEAD(mid, 30) OVER (PARTITION BY coin ORDER BY ts_ms) AS mid_30s_later
                FROM read_parquet('{src}')
            ),
            per_coin AS (
                SELECT 
                    coin,
                    COUNT(*)                                    AS rows,
                    AVG(spread_bps)                             AS avg_spread_bps,
                    STDDEV(spread_bps)                          AS std_spread_bps,
                    MIN(spread_bps)                             AS min_spread_bps,
                    MAX(spread_bps)                             AS max_spread_bps,
                    -- Adverse selection: how much does mid move in 30 ticks?
                    AVG(ABS(mid_30s_later - mid) / mid * 10000) AS adv_sel_bps,
                    -- Edge = our spread - adverse selection cost
                    AVG(spread_bps) - AVG(ABS(mid_30s_later - mid) / mid * 10000) AS edge_bps
                FROM ticks
                WHERE mid_30s_later IS NOT NULL
                GROUP BY coin
                ORDER BY edge_bps DESC
            )
            SELECT 
                coin,
                rows,
                ROUND(avg_spread_bps, 4)  AS raw_spread_bps,
                ROUND(adv_sel_bps, 4)     AS adverse_sel_bps,
                ROUND(edge_bps, 4)        AS edge_bps,
                CASE WHEN edge_bps > 0 THEN '✅ Edge' ELSE '❌ Negative' END AS verdict,
                ROUND(avg_spread_bps / NULLIF(adv_sel_bps, 0), 2) AS spread_coverage_ratio
            FROM per_coin
        """).df()
        
        print(f"\n{'Coin':<12} {'Rows':>8} {'Raw Spread':>12} {'Adv.Sel.':>10} {'Edge':>8}  {'Verdict':<14} {'Coverage'}")
        print('─'*80)
        for _, r in result.iterrows():
            print(f"{r['coin']:<12} {r['rows']:>8,.0f} {r['raw_spread_bps']:>12.4f} "
                  f"{r['adverse_sel_bps']:>10.4f} {r['edge_bps']:>8.4f}  "
                  f"{r['verdict']:<14} {r['spread_coverage_ratio']:.2f}x")

        # Summary
        edge_coins = result[result['edge_bps'] > 0]
        print(f"\n  {len(edge_coins)}/{len(result)} coins show positive edge with default 1.5bps spread.")
        if len(result) > 0:
            min_edge = result['edge_bps'].min()
            print(f"  Weakest edge: {min_edge:.4f} bps → consider widening spread for these coins.")

    except Exception as e:
        print(f"  ⚠ Not enough data yet: {e}")
        print(f"  Run again after Gate 1 milestone (~3 hours from start).")


# ─── Gate 2: Hourly Regime Profiling ──────────────────────────────────────────
def gate2_regime_profile(coin_filter: str = "*"):
    """Map spread volatility by hour — identifies which UTC hours are chaotic."""
    src, fmt = get_source(coin_filter)
    print(f"\n{'━'*65}")
    print(f"GATE 2 — Hourly Spread Regime Profile  [{fmt}]")
    print('━'*65)

    try:
        result = con.execute(f"""
            SELECT 
                hour,
                COUNT(DISTINCT coin)     AS active_coins,
                COUNT(*)                 AS total_ticks,
                ROUND(AVG(spread_bps), 4) AS avg_spread_bps,
                ROUND(STDDEV(spread_bps), 4) AS std_spread_bps,
                ROUND(MAX(spread_bps), 4)  AS max_spread_bps,
                CASE 
                    WHEN AVG(spread_bps) < 0.5  THEN 'CALM (1.0x)'
                    WHEN AVG(spread_bps) < 1.0  THEN 'UNCERTAIN (1.5x)'
                    ELSE                              'CHAOTIC (3.0x)'
                END AS regime
            FROM read_parquet('{src}')
            GROUP BY hour
            ORDER BY hour
        """).df()

        print(f"\n{'Hour (UTC)':<12} {'Ticks':>8} {'Avg Spread':>12} {'Std':>8} {'Max':>8}  Regime")
        print('─'*70)
        for _, r in result.iterrows():
            bar = '█' * min(int(r['avg_spread_bps'] * 20), 20)
            print(f"  {r['hour']:02d}:00       {r['total_ticks']:>8,.0f} {r['avg_spread_bps']:>12.4f} "
                  f"{r['std_spread_bps']:>8.4f} {r['max_spread_bps']:>8.4f}  {r['regime']}")

        print("\n  → These multipliers feed directly into RegimeGovernor.calm_atr_threshold")

    except Exception as e:
        print(f"  ⚠ Not enough data yet: {e}")


# ─── Gate 3: Spread Calibration (OLS) ─────────────────────────────────────────
def gate3_spread_calibration(coin_filter: str = "*"):
    """
    Fits optimal half-spread per coin using realized spread as the target.
    Recommendation: set base_spread_bps to max(min_viable, realized_spread * 1.2)
    """
    src, fmt = get_source(coin_filter)
    print(f"\n{'━'*65}")
    print(f"GATE 3 — Spread Calibration Recommendations  [{fmt}]")
    print('━'*65)

    try:
        result = con.execute(f"""
            WITH stats AS (
                SELECT 
                    coin,
                    COUNT(*)                         AS rows,
                    AVG(spread_bps)                  AS raw_spread_bps,
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY spread_bps) AS p95_spread,
                    PERCENTILE_CONT(0.50) WITHIN GROUP (ORDER BY spread_bps) AS p50_spread
                FROM read_parquet('{src}')
                GROUP BY coin
                HAVING COUNT(*) > 10000
            )
            SELECT
                coin,
                rows,
                ROUND(raw_spread_bps, 4)          AS avg_spread,
                ROUND(p50_spread, 4)               AS p50_spread,
                ROUND(p95_spread, 4)               AS p95_spread,
                -- Recommended: set our half-spread to p95 of raw exchange spread
                -- This means we're wider ~95% of the time = we're almost never the
                -- tightest quote, but we capture most of the spread when vol spikes
                ROUND(GREATEST(p95_spread * 0.8, 1.0), 2) AS recommended_half_spread_bps
            FROM stats
            ORDER BY recommended_half_spread_bps DESC
        """).df()

        if result.empty:
            print("  ⚠ Need >10,000 rows/coin. Run again after Gate 3 (~1.5 days).")
            return

        print(f"\n{'Coin':<12} {'Rows':>9} {'Avg Spread':>12} {'P50':>8} {'P95':>8}  {'Recommended':>14}")
        print('─'*70)
        for _, r in result.iterrows():
            print(f"  {r['coin']:<10} {r['rows']:>9,.0f} {r['avg_spread']:>12.4f} "
                  f"{r['p50_spread']:>8.4f} {r['p95_spread']:>8.4f}  "
                  f"{r['recommended_half_spread_bps']:>14.2f} bps")

        print("\n  → Copy 'Recommended' column into MmAssetConfig.base_spread_bps per coin")
        print("  → Python Screener will publish these to the Rust engine via Redis")

    except Exception as e:
        print(f"  ⚠ Error: {e}")


# ─── Summary ──────────────────────────────────────────────────────────────────
def summary():
    src, fmt = get_source()
    print(f"\n{'━'*65}")
    print(f"DATASET SUMMARY  [{fmt}]")
    print('━'*65)
    try:
        result = con.execute(f"""
            SELECT
                COUNT(DISTINCT coin)  AS coins,
                COUNT(*)              AS total_rows,
                ROUND(MIN(ts_ms)/1000)  AS start_unix,
                ROUND(MAX(ts_ms)/1000)  AS end_unix,
                ROUND((MAX(ts_ms) - MIN(ts_ms)) / 60000.0, 1) AS duration_min,
                ROUND(AVG(spread_bps), 4) AS avg_spread_bps
            FROM read_parquet('{src}')
        """).fetchone()

        coins, rows, start_u, end_u, dur_min, avg_sprd = result
        start_dt = datetime.datetime.utcfromtimestamp(start_u).strftime('%Y-%m-%d %H:%M UTC')
        end_dt   = datetime.datetime.utcfromtimestamp(end_u).strftime('%Y-%m-%d %H:%M UTC')
        print(f"  Coins       : {coins}")
        print(f"  Total rows  : {rows:,}")
        print(f"  Window      : {start_dt} → {end_dt}  ({dur_min:.0f} min)")
        print(f"  Avg spread  : {avg_sprd:.4f} bps")
    except Exception as e:
        print(f"  ⚠ {e}")


# ─── Main ─────────────────────────────────────────────────────────────────────
def main():
    parser = argparse.ArgumentParser(description="Analyze MM tick data with DuckDB")
    parser.add_argument("--coin",  default="*",   help="Filter to specific coin (e.g. BTC)")
    parser.add_argument("--gate",  type=int,       help="Run only a specific gate (1, 2, or 3)")
    args = parser.parse_args()

    coin = args.coin if args.coin != "*" else "*"

    if not source_exists_parquet():
        # Fall back to CSV analysis (DuckDB reads CSV too, just slower)
        print("⚠  No Parquet files found. Falling back to reading CSVs directly.")
        print("   Run 'python3 scripts/csv_to_parquet.py --all' to convert first.")
        print()

    if args.gate == 1:
        gate1_adverse_selection(coin)
    elif args.gate == 2:
        gate2_regime_profile(coin)
    elif args.gate == 3:
        gate3_spread_calibration(coin)
    else:
        summary()
        gate1_adverse_selection(coin)
        gate2_regime_profile(coin)
        gate3_spread_calibration(coin)

    print(f"\n{'━'*65}")
    print("Tip: run ad-hoc SQL with:")
    print(f"  duckdb -c \"SELECT coin, avg(spread_bps) FROM read_parquet('{parquet_glob()}') GROUP BY coin\"")
    print('━'*65)


if __name__ == "__main__":
    main()
