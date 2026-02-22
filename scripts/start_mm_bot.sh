#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# start_mm_bot.sh — Starts the Market Maker Bot
# This script is COMPLETELY SEPARATE from start_live_sim.sh (directional bot).
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_DIR="$PROJECT_ROOT/logs/mm"
COMPOSE_FILE="$PROJECT_ROOT/docker-compose.mm.yml"

# Create isolated log directory for the MM bot
mkdir -p "$LOG_DIR"

echo "🏦 Starting Market Maker Bot (V6 Architecture)..."
echo "   Compose file: $COMPOSE_FILE"
echo "   Shadow Mode:  ${MM_SHADOW_MODE:-true}  (set MM_SHADOW_MODE=false to enable live quoting)"
echo ""

# Safety guard: confirm if shadow mode is being disabled
if [[ "${MM_SHADOW_MODE:-true}" == "false" ]]; then
    echo "⚠️  WARNING: SHADOW MODE IS DISABLED. Real money will be placed."
    read -p "   Type 'CONFIRM LIVE' to proceed: " CONFIRMATION
    if [[ "$CONFIRMATION" != "CONFIRM LIVE" ]]; then
        echo "❌ Aborted. Restart with MM_SHADOW_MODE=true for safe testing."
        exit 1
    fi
fi

# Build and start MM-specific containers only
cd "$PROJECT_ROOT"
docker compose -f "$COMPOSE_FILE" build
docker compose -f "$COMPOSE_FILE" up -d

echo ""
echo "✅ Market Maker Bot containers started."
echo "   Logs: docker compose -f docker-compose.mm.yml logs -f mm-engine"
echo "   Stop: docker compose -f docker-compose.mm.yml down"
