#!/bin/bash
# ─────────────────────────────────────────────────────────────────────────────
# cron_ranker.sh — Automated Coin Ranking Cron
#
# Runs the auto-ranker every RANK_INTERVAL_HOURS (default: 6h).
# Backtests all coins, updates PRIORITY_COINS in the screener,
# and rebuilds the screener container if the list changed.
#
# Runs as a Docker sidecar — see docker-compose.yml mm-ranker service.
# ─────────────────────────────────────────────────────────────────────────────

set -euo pipefail

INTERVAL_HOURS="${RANK_INTERVAL_HOURS:-6}"
INTERVAL_SECS=$((INTERVAL_HOURS * 3600))
TICK_DIR="/app/data/ticks"
SCRIPTS_DIR="/app/scripts"
LOG_PREFIX="[RANKER]"

echo "$LOG_PREFIX Starting automated coin ranker (every ${INTERVAL_HOURS}h)"
echo "$LOG_PREFIX Tick data: $TICK_DIR"
echo "$LOG_PREFIX Waiting 5 minutes for initial tick data to accumulate..."

# Wait 5 min on first boot so there's enough tick data
sleep 300

while true; do
    echo ""
    echo "$LOG_PREFIX ═══ Ranking cycle at $(date -u '+%Y-%m-%d %H:%M UTC') ═══"

    # Count available tick files
    TICK_COUNT=$(find "$TICK_DIR" -name "*.csv" 2>/dev/null | wc -l)
    echo "$LOG_PREFIX Found $TICK_COUNT tick CSV files"

    if [ "$TICK_COUNT" -lt 1 ]; then
        echo "$LOG_PREFIX No tick data yet. Skipping this cycle."
        sleep $INTERVAL_SECS
        continue
    fi

    # Run the auto-ranker
    echo "$LOG_PREFIX Running backtester on all coins..."
    cd /app
    python3 scripts/auto_rank_coins.py \
        --days 1 \
        --top 10 \
        --max-inv 20 \
        --spread 2.0 \
        --update-screener \
        --redis-url "${REDIS_URL:-redis://mm-redis:6379}" \
        --project-root /app \
        2>&1 | while read line; do echo "$LOG_PREFIX $line"; done

    RESULT=$?
    if [ $RESULT -eq 0 ]; then
        echo "$LOG_PREFIX ✅ Ranking complete. Screener PRIORITY_COINS updated."
    else
        echo "$LOG_PREFIX ⚠️ Ranking failed (exit code $RESULT). Will retry next cycle."
    fi

    echo "$LOG_PREFIX Next ranking in ${INTERVAL_HOURS}h. Sleeping..."
    sleep $INTERVAL_SECS
done
