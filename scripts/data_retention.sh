#!/bin/bash
# ─────────────────────────────────────────────────────────────────────────────
# data_retention.sh — Sliding window data cleanup for HyperLiquidMM
#
# Retention policy:
#   - Raw CSV ticks:   7 days (only needed until converted to Parquet)
#   - Parquet files:   30 days rolling (enough for weekly recalibration)
#   - Compressed logs: 14 days
#
# Usage:
#   ./scripts/data_retention.sh              # Dry run (show what would be deleted)
#   ./scripts/data_retention.sh --execute    # Actually delete
#
# Cron setup (run daily at 3 AM):
#   0 3 * * * cd /path/to/HyperLiquidMM && ./scripts/data_retention.sh --execute >> logs/retention.log 2>&1
# ─────────────────────────────────────────────────────────────────────────────

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CSV_DIR="$PROJECT_ROOT/data/ticks"
ALT_CSV_DIR="$PROJECT_ROOT/backend/mm-engine-rs/data/ticks"
PARQUET_DIR="$PROJECT_ROOT/data/parquet"
LOG_DIR="$PROJECT_ROOT/logs"

CSV_RETENTION_DAYS=7
PARQUET_RETENTION_DAYS=30
LOG_RETENTION_DAYS=14

EXECUTE=false
if [[ "${1:-}" == "--execute" ]]; then
    EXECUTE=true
fi

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo " HyperLiquidMM Data Retention — $(date '+%Y-%m-%d %H:%M:%S')"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo " Mode: $(if $EXECUTE; then echo 'EXECUTE (deleting files)'; else echo 'DRY RUN (preview only)'; fi)"
echo ""

# ── Current disk usage ──────────────────────────────────────────────────────
echo "Current disk usage:"
for dir in "$CSV_DIR" "$ALT_CSV_DIR" "$PARQUET_DIR" "$LOG_DIR"; do
    if [ -d "$dir" ]; then
        size=$(du -sh "$dir" 2>/dev/null | cut -f1)
        echo "  $dir: $size"
    fi
done
echo ""

# ── CSV cleanup (>7 days) ──────────────────────────────────────────────────
csv_count=0
for dir in "$CSV_DIR" "$ALT_CSV_DIR"; do
    if [ -d "$dir" ]; then
        files=$(find "$dir" -name "*.csv" -mtime +$CSV_RETENTION_DAYS 2>/dev/null || true)
        if [ -n "$files" ]; then
            count=$(echo "$files" | wc -l | tr -d ' ')
            csv_count=$((csv_count + count))
            echo "CSV files older than ${CSV_RETENTION_DAYS} days in $dir: $count"
            if $EXECUTE; then
                echo "$files" | xargs rm -f
                echo "  ✅ Deleted $count CSV files"
            else
                echo "$files" | head -5
                if [ "$count" -gt 5 ]; then
                    echo "  ... and $((count - 5)) more"
                fi
            fi
        fi
    fi
done
if [ "$csv_count" -eq 0 ]; then
    echo "CSV: No files older than ${CSV_RETENTION_DAYS} days"
fi

# ── Parquet cleanup (>30 days) ─────────────────────────────────────────────
if [ -d "$PARQUET_DIR" ]; then
    parquet_files=$(find "$PARQUET_DIR" -name "*.parquet" -mtime +$PARQUET_RETENTION_DAYS 2>/dev/null || true)
    if [ -n "$parquet_files" ]; then
        count=$(echo "$parquet_files" | wc -l | tr -d ' ')
        echo ""
        echo "Parquet files older than ${PARQUET_RETENTION_DAYS} days: $count"
        if $EXECUTE; then
            echo "$parquet_files" | xargs rm -f
            echo "  ✅ Deleted $count Parquet files"
        else
            echo "$parquet_files" | head -5
        fi
    else
        echo "Parquet: No files older than ${PARQUET_RETENTION_DAYS} days"
    fi
fi

# ── Log cleanup (>14 days, compress first) ─────────────────────────────────
if [ -d "$LOG_DIR" ]; then
    # Compress uncompressed logs older than 2 days
    uncomp=$(find "$LOG_DIR" -name "*.log" -mtime +2 2>/dev/null || true)
    if [ -n "$uncomp" ]; then
        count=$(echo "$uncomp" | wc -l | tr -d ' ')
        echo ""
        echo "Uncompressed logs older than 2 days: $count"
        if $EXECUTE; then
            echo "$uncomp" | xargs gzip -f 2>/dev/null || true
            echo "  ✅ Compressed $count log files"
        fi
    fi

    # Delete compressed logs older than retention period
    old_logs=$(find "$LOG_DIR" -name "*.log.gz" -mtime +$LOG_RETENTION_DAYS 2>/dev/null || true)
    if [ -n "$old_logs" ]; then
        count=$(echo "$old_logs" | wc -l | tr -d ' ')
        echo "Compressed logs older than ${LOG_RETENTION_DAYS} days: $count"
        if $EXECUTE; then
            echo "$old_logs" | xargs rm -f
            echo "  ✅ Deleted $count compressed logs"
        fi
    else
        echo "Logs: No compressed logs older than ${LOG_RETENTION_DAYS} days"
    fi
fi

echo ""
if ! $EXECUTE; then
    echo "This was a DRY RUN. Use --execute to actually delete files."
fi
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
