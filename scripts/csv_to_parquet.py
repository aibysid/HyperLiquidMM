#!/usr/bin/env python3
"""
csv_to_parquet.py — Daily tick data converter
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Converts the previous day's CSV tick files to Parquet.

Run manually or via cron:
  # Run every day at 00:05 UTC
  5 0 * * * cd /path/to/scratch && python3 scripts/csv_to_parquet.py >> logs/csv_to_parquet.log 2>&1

Usage:
  python3 scripts/csv_to_parquet.py              # converts yesterday
  python3 scripts/csv_to_parquet.py --date 2026-02-22   # specific date
  python3 scripts/csv_to_parquet.py --all        # converts all available dates
"""

import os
import sys
import csv
import time
import argparse
import datetime
import pyarrow as pa
import pyarrow.parquet as pq

CSV_DIR     = "backend/mm-engine-rs/data/ticks"
PARQUET_DIR = "data/parquet"

# Schema: enforced at conversion time — catches corrupt CSV rows early
SCHEMA = pa.schema([
    pa.field("ts_ms",      pa.int64()),    # Unix millisecond timestamp
    pa.field("coin",       pa.string()),   # Asset name e.g. "BTC"
    pa.field("bid",        pa.float64()),  # Best bid price
    pa.field("ask",        pa.float64()),  # Best ask price
    pa.field("mid",        pa.float64()),  # Mid price = (bid+ask)/2
    pa.field("spread_bps", pa.float64()),  # Spread in basis points
    # Derived columns added at conversion time
    pa.field("hour",       pa.int8()),     # 0-23 UTC hour (for hourly regime analysis)
    pa.field("date",       pa.string()),   # YYYY-MM-DD (for partitioning)
])


def read_csv(path: str, coin: str, date_str: str) -> list[dict]:
    """Reads a single CSV file and returns a list of validated row dicts."""
    rows = []
    skipped = 0
    with open(path, newline="") as f:
        reader = csv.reader(f)
        for row in reader:
            if len(row) < 6:
                skipped += 1
                continue
            try:
                ts_ms      = int(row[0])
                bid        = float(row[2])
                ask        = float(row[3])
                mid        = float(row[4])
                spread_bps = float(row[5])
                hour       = datetime.datetime.utcfromtimestamp(ts_ms / 1000).hour
                rows.append({
                    "ts_ms":      ts_ms,
                    "coin":       coin,
                    "bid":        bid,
                    "ask":        ask,
                    "mid":        mid,
                    "spread_bps": spread_bps,
                    "hour":       hour,
                    "date":       date_str,
                })
            except (ValueError, IndexError):
                skipped += 1

    if skipped > 0:
        print(f"    ⚠  {coin}: skipped {skipped} malformed rows")
    return rows


def convert_date(date_str: str) -> dict:
    """Converts all coin CSVs for a given date to one Parquet file per coin."""
    results = {}
    os.makedirs(PARQUET_DIR, exist_ok=True)

    coins_converted = 0
    total_rows = 0
    total_in_bytes = 0
    total_out_bytes = 0

    for coin in sorted(os.listdir(CSV_DIR)):
        csv_path = os.path.join(CSV_DIR, coin, f"{date_str}.csv")
        if not os.path.exists(csv_path):
            continue

        out_dir = os.path.join(PARQUET_DIR, coin)
        os.makedirs(out_dir, exist_ok=True)
        out_path = os.path.join(out_dir, f"{date_str}.parquet")

        if os.path.exists(out_path):
            print(f"  ✓ {coin}: already converted, skipping")
            continue

        t0 = time.perf_counter()
        rows = read_csv(csv_path, coin, date_str)
        if not rows:
            print(f"  ✗ {coin}: empty CSV, skipping")
            continue

        # Build PyArrow table
        table = pa.table(
            {
                "ts_ms":      pa.array([r["ts_ms"]      for r in rows], type=pa.int64()),
                "coin":       pa.array([r["coin"]        for r in rows], type=pa.string()),
                "bid":        pa.array([r["bid"]         for r in rows], type=pa.float64()),
                "ask":        pa.array([r["ask"]         for r in rows], type=pa.float64()),
                "mid":        pa.array([r["mid"]         for r in rows], type=pa.float64()),
                "spread_bps": pa.array([r["spread_bps"]  for r in rows], type=pa.float64()),
                "hour":       pa.array([r["hour"]        for r in rows], type=pa.int8()),
                "date":       pa.array([r["date"]        for r in rows], type=pa.string()),
            },
            schema=SCHEMA,
        )

        pq.write_table(
            table,
            out_path,
            compression="zstd",     # Best ratio for financial tick data (better than snappy)
            compression_level=6,
            row_group_size=50_000,  # Enables efficient range scans by DuckDB/Polars
        )

        elapsed_ms = (time.perf_counter() - t0) * 1000
        in_bytes   = os.path.getsize(csv_path)
        out_bytes  = os.path.getsize(out_path)
        ratio      = in_bytes / out_bytes

        print(f"  ✓ {coin:<12} {len(rows):>8,} rows  "
              f"{in_bytes/1024:>6.0f}KB → {out_bytes/1024:>5.0f}KB  "
              f"({ratio:.1f}x)  {elapsed_ms:.0f}ms")

        coins_converted += 1
        total_rows      += len(rows)
        total_in_bytes  += in_bytes
        total_out_bytes += out_bytes
        results[coin]    = out_path

    if coins_converted > 0:
        overall_ratio = total_in_bytes / total_out_bytes if total_out_bytes else 0
        print(f"\n  Total: {coins_converted} coins, {total_rows:,} rows, "
              f"{total_in_bytes/1024:.0f}KB → {total_out_bytes/1024:.0f}KB "
              f"({overall_ratio:.1f}x compression)")

    return results


def main():
    parser = argparse.ArgumentParser(description="Convert MM tick CSVs to Parquet")
    group  = parser.add_mutually_exclusive_group()
    group.add_argument("--date", help="Specific date YYYY-MM-DD")
    group.add_argument("--all",  action="store_true", help="Convert all available dates")
    args = parser.parse_args()

    if args.all:
        # Find all unique dates across all coins
        all_dates = set()
        for coin in os.listdir(CSV_DIR):
            coin_dir = os.path.join(CSV_DIR, coin)
            if os.path.isdir(coin_dir):
                for fname in os.listdir(coin_dir):
                    if fname.endswith(".csv"):
                        all_dates.add(fname[:-4])  # strip .csv
        dates = sorted(all_dates)
    elif args.date:
        dates = [args.date]
    else:
        # Default: yesterday UTC
        yesterday = (datetime.datetime.utcnow() - datetime.timedelta(days=1)).strftime("%Y-%m-%d")
        dates = [yesterday]

    for date_str in dates:
        print(f"\n{'='*60}")
        print(f"Converting date: {date_str}")
        print('='*60)
        convert_date(date_str)

    print("\nDone. Query with:")
    print("  python3 scripts/analyze_ticks.py")
    print("  OR: duckdb -c \"SELECT * FROM 'data/parquet/BTC/*.parquet' LIMIT 5\"")


if __name__ == "__main__":
    main()
