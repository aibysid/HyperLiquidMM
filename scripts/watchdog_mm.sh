#!/bin/bash
# ─────────────────────────────────────────────────────────────────────────────
# watchdog_mm.sh — Market Maker Engine Watchdog (macOS dev)
#
# Runs the MM engine and auto-restarts it if it crashes.
# Logs each run's start/stop time and crash reason to logs/mm_watchdog.log
#
# Usage:
#   ./scripts/watchdog_mm.sh              # shadow mode (default)
#   MM_SHADOW_MODE=false ./scripts/watchdog_mm.sh   # live mode (DANGEROUS)
#
# To stop watchdog cleanly: Ctrl+C  (sends SIGTERM to engine + watchdog)
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ENGINE_DIR="$ROOT_DIR/backend/mm-engine-rs"
LOG_DIR="$ROOT_DIR/logs"
WATCHDOG_LOG="$LOG_DIR/mm_watchdog.log"
ENGINE_BINARY="$ENGINE_DIR/target/release/mm-engine-rs"

mkdir -p "$LOG_DIR"

# ── Safety gate: confirm before going live ────────────────────────────────────
SHADOW_MODE="${MM_SHADOW_MODE:-true}"
if [[ "$SHADOW_MODE" == "false" || "$SHADOW_MODE" == "0" ]]; then
    echo "⚠️  WARNING: You are about to start in LIVE trading mode."
    echo "   Real orders WILL be placed on Hyperliquid."
    echo ""
    read -p "   Type 'CONFIRM LIVE' to proceed: " confirmation
    if [[ "$confirmation" != "CONFIRM LIVE" ]]; then
        echo "Aborted."
        exit 0
    fi
fi

# ── Build binary if not present or source is newer ───────────────────────────
echo "Checking binary freshness..."
if [[ ! -f "$ENGINE_BINARY" ]] || \
   find "$ENGINE_DIR/src" -newer "$ENGINE_BINARY" -name "*.rs" | grep -q .; then
    echo "Building mm-engine-rs (release)..."
    (cd "$ENGINE_DIR" && cargo build --release 2>&1)
    echo "Build complete."
fi

# ── Watchdog loop ─────────────────────────────────────────────────────────────
ATTEMPT=0
MAX_RESTART_DELAY=300   # Max backoff: 5 minutes
ENGINE_PID=""

# Clean up engine on Ctrl+C / SIGTERM
cleanup() {
    echo ""
    echo "[WATCHDOG] Caught signal. Stopping engine (PID=$ENGINE_PID)..."
    if [[ -n "$ENGINE_PID" ]] && kill -0 "$ENGINE_PID" 2>/dev/null; then
        kill -TERM "$ENGINE_PID"
        wait "$ENGINE_PID" 2>/dev/null || true
    fi
    echo "[WATCHDOG] Engine stopped. Exiting."
    exit 0
}
trap cleanup INT TERM

echo "[WATCHDOG] Starting mm-engine-rs watchdog (shadow=$SHADOW_MODE)"
echo "[WATCHDOG] Logs → $WATCHDOG_LOG"
echo "[WATCHDOG] Press Ctrl+C to stop."
echo ""

while true; do
    ATTEMPT=$((ATTEMPT + 1))
    START_TIME=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

    echo "[WATCHDOG $START_TIME] Starting engine (attempt #$ATTEMPT)" | tee -a "$WATCHDOG_LOG"

    # Run engine — inherit stdout so logs go to terminal
    # .env is auto-loaded by binary via dotenvy
    (
        cd "$ENGINE_DIR"
        RUST_LOG=info \
        MM_SHADOW_MODE="$SHADOW_MODE" \
        MM_HARVEST_TICKS=true \
        exec "$ENGINE_BINARY"
    ) &
    ENGINE_PID=$!

    # Wait for engine to finish (crash or clean exit)
    wait "$ENGINE_PID"
    EXIT_CODE=$?
    END_TIME=$(date -u '+%Y-%m-%dT%H:%M:%SZ')

    echo "[WATCHDOG $END_TIME] Engine exited with code $EXIT_CODE (attempt #$ATTEMPT)" | tee -a "$WATCHDOG_LOG"

    # Exit code 0 = intentional stop (shouldn't happen in normal operation)
    if [[ $EXIT_CODE -eq 0 ]]; then
        echo "[WATCHDOG] Clean exit detected. Not restarting."
        break
    fi

    # Exponential backoff: 2s, 4s, 8s, ... up to MAX_RESTART_DELAY
    DELAY=$((2 ** (ATTEMPT < 8 ? ATTEMPT : 8)))
    DELAY=$((DELAY > MAX_RESTART_DELAY ? MAX_RESTART_DELAY : DELAY))

    echo "[WATCHDOG] Crashed (exit=$EXIT_CODE). Restarting in ${DELAY}s..." | tee -a "$WATCHDOG_LOG"
    echo "[WATCHDOG] Gap in tick data: $START_TIME → $END_TIME" | tee -a "$WATCHDOG_LOG"
    sleep "$DELAY"

    # Reset backoff if engine ran for > 5 minutes (stable session)
    # (we track this via simple heuristic: attempt counter resets)
    # TODO: track wall time and reset attempt if run_duration > 300s
done

echo "[WATCHDOG] Exiting."
