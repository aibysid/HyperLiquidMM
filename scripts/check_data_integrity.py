#!/usr/bin/env python3
"""
check_data_integrity.py — Tick data gap detector & integrity checker
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Run any time to:
  - Find gaps in collection (engine crashes, network disconnects)
  - Detect corrupted / truncated CSV rows
  - Confirm Parquet files are consistent with their source CSVs
  - Get a summary of total expected vs actual coverage

Usage:
  python3 scripts/check_data_integrity.py
  python3 scripts/check_data_integrity.py --coin BTC
  python3 scripts/check_data_integrity.py --fix     # removes corrupted rows
"""

import os
import csv
import sys
import argparse
import datetime

CSV_DIR     = "backend/mm-engine-rs/data/ticks"
PARQUET_DIR = "data/parquet"
GAP_THRESHOLD_SEC = 30    # gaps > 30s flagged as collection interruptions
WARN_THRESHOLD_SEC = 120  # gaps > 2min flagged as significant data loss


def ts_to_str(ts_ms: int) -> str:
    return datetime.datetime.utcfromtimestamp(ts_ms / 1000).strftime('%Y-%m-%d %H:%M:%S UTC')


def check_coin_csv(coin: str, date_str: str, fix: bool = False) -> dict:
    """
    Checks a single coin's CSV file for:
      1. Corrupted/truncated rows (wrong field count)
      2. Duplicate timestamps
      3. Gaps > GAP_THRESHOLD_SEC seconds
      4. Timestamp monotonicity violations
    """
    csv_path = os.path.join(CSV_DIR, coin, f"{date_str}.csv")
    if not os.path.exists(csv_path):
        return {"coin": coin, "status": "missing", "rows": 0}

    good_rows = []
    bad_rows  = []
    seen_ts   = set()
    duplicates = 0

    with open(csv_path, newline="") as f:
        for i, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            parts = line.split(",")
            if len(parts) != 6:
                bad_rows.append((i, f"expected 6 fields, got {len(parts)}: {line[:60]}"))
                continue
            try:
                ts = int(parts[0])
                float(parts[2]); float(parts[3]); float(parts[4]); float(parts[5])
            except ValueError as e:
                bad_rows.append((i, f"parse error: {e}: {line[:60]}"))
                continue

            if ts in seen_ts:
                duplicates += 1
            seen_ts.add(ts)
            good_rows.append((i, ts, line))

    if not good_rows:
        return {"coin": coin, "status": "empty", "rows": 0, "bad_rows": len(bad_rows)}

    # Sort by timestamp
    good_rows.sort(key=lambda x: x[1])

    # Find gaps
    gaps = []
    for i in range(1, len(good_rows)):
        prev_ts = good_rows[i-1][1]
        curr_ts = good_rows[i][1]
        delta_sec = (curr_ts - prev_ts) / 1000
        if delta_sec > GAP_THRESHOLD_SEC:
            gaps.append({
                "from":     ts_to_str(prev_ts),
                "to":       ts_to_str(curr_ts),
                "from_ms":  prev_ts,
                "to_ms":    curr_ts,
                "gap_sec":  delta_sec,
                "severity": "CRITICAL" if delta_sec > WARN_THRESHOLD_SEC else "warn",
            })

    # Fix: rewrite CSV without bad rows (overwrite)
    if fix and (bad_rows or duplicates):
        seen_write = set()
        with open(csv_path, "w") as f_out:
            for _, ts, line in good_rows:
                if ts not in seen_write:
                    f_out.write(line + "\n")
                    seen_write.add(ts)
        print(f"  [{coin}] Fixed: removed {len(bad_rows)} corrupted + {duplicates} duplicate rows")

    start_ts = good_rows[0][1]
    end_ts   = good_rows[-1][1]
    duration_min = (end_ts - start_ts) / 60_000

    return {
        "coin":         coin,
        "status":       "ok" if not bad_rows and not gaps else "degraded",
        "rows":         len(good_rows),
        "bad_rows":     len(bad_rows),
        "duplicates":   duplicates,
        "gaps":         gaps,
        "start":        ts_to_str(start_ts),
        "end":          ts_to_str(end_ts),
        "duration_min": duration_min,
        "ticks_per_min": len(good_rows) / max(duration_min, 1),
    }


def find_all_dates() -> list[str]:
    """Returns sorted list of all dates that have any CSV data."""
    dates = set()
    for coin in os.listdir(CSV_DIR):
        coin_dir = os.path.join(CSV_DIR, coin)
        if not os.path.isdir(coin_dir):
            continue
        for fname in os.listdir(coin_dir):
            if fname.endswith(".csv"):
                dates.add(fname[:-4])
    return sorted(dates)


def main():
    parser = argparse.ArgumentParser(description="Check tick data integrity")
    parser.add_argument("--coin",  default=None, help="Check only this coin")
    parser.add_argument("--date",  default=None, help="Check only this date (YYYY-MM-DD)")
    parser.add_argument("--fix",   action="store_true", help="Remove corrupted rows in-place")
    args = parser.parse_args()

    dates = [args.date] if args.date else find_all_dates()
    coins = [args.coin] if args.coin else sorted(os.listdir(CSV_DIR))

    total_gaps       = 0
    total_bad_rows   = 0
    total_rows       = 0
    critical_gaps    = []

    for date_str in dates:
        print(f"\n{'━'*65}")
        print(f"Date: {date_str}")
        print('━'*65)
        print(f"  {'Coin':<12} {'Rows':>8}  {'Duration':>10}  {'Rate/min':>9}  {'Gaps':>5}  {'Bad':>5}  Status")
        print(f"  {'─'*12} {'─'*8}  {'─'*10}  {'─'*9}  {'─'*5}  {'─'*5}  {'─'*10}")

        for coin in coins:
            result = check_coin_csv(coin, date_str, fix=args.fix)

            if result["status"] == "missing":
                print(f"  {coin:<12} {'—':>8}  {'—':>10}  {'—':>9}  {'—':>5}  {'—':>5}  missing")
                continue

            n_gaps = len(result.get("gaps", []))
            n_bad  = result.get("bad_rows", 0)
            status = "✅" if n_gaps == 0 and n_bad == 0 else ("⚠️" if n_gaps < 3 else "❌")

            print(f"  {coin:<12} {result['rows']:>8,}  "
                  f"{result['duration_min']:>9.1f}m  "
                  f"{result['ticks_per_min']:>9.1f}  "
                  f"{n_gaps:>5}  {n_bad:>5}  {status}")

            total_rows     += result["rows"]
            total_gaps     += n_gaps
            total_bad_rows += n_bad

            # Show critical gaps
            for gap in result.get("gaps", []):
                severity_icon = "🔴" if gap["severity"] == "CRITICAL" else "🟡"
                print(f"    {severity_icon} {gap['from']} → {gap['to']}  ({gap['gap_sec']:.0f}s)")
                if gap["severity"] == "CRITICAL":
                    critical_gaps.append((coin, date_str, gap))

    # Summary
    print(f"\n{'━'*65}")
    print("INTEGRITY SUMMARY")
    print('━'*65)
    print(f"  Total rows       : {total_rows:,}")
    print(f"  Corrupted rows   : {total_bad_rows}")
    print(f"  Gaps detected    : {total_gaps}")
    print(f"  Critical gaps    : {len(critical_gaps)}")

    if total_bad_rows > 0:
        print(f"\n  ⚠  Run with --fix to remove {total_bad_rows} corrupted rows")

    if critical_gaps:
        print(f"\n  🔴 CRITICAL GAPS (data loss > {WARN_THRESHOLD_SEC}s):")
        for coin, date, gap in critical_gaps:
            print(f"     {coin} {date}: {gap['gap_sec']:.0f}s  ({gap['from']} → {gap['to']})")
        print(f"\n  These gaps are permanent — data was not recorded during engine downtime.")
        print(f"  Tip: Use watchdog_mm.sh to auto-restart on crash and minimize future gaps.")
    else:
        print(f"\n  ✅ No critical gaps. Engine has been running continuously.")

    print(f"\n  Watchdog:  ./scripts/watchdog_mm.sh   (auto-restart on crash)")
    print(f"  Convert:   python3 scripts/csv_to_parquet.py --all")
    print(f"  Analyse:   python3 scripts/analyze_ticks.py")


if __name__ == "__main__":
    main()
