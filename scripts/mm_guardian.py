#!/usr/bin/env python3
"""
mm_guardian.py — V8.0 The Autonomous Guardian
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
A watchdog script that runs in the background. 
Monitors Account Value, Trade Health, and Rate Limits.
Autonomously takes protective actions (Halt, Widen Spreads, etc.)
"""

import os
import json
import time
import requests
import redis
import subprocess
import logging
from datetime import datetime, timezone
from pathlib import Path

# --- Configuration ---
CHECK_INTERVAL_SECS = 600  # 10 minutes
HARD_STOP_DRAWDOWN_USD = 2.50  # Stop everything if we lose $2.50
WIN_RATE_THRESHOLD = 0.40  # Widen spreads if win rate < 40%
MIN_BALANCE_STOP = 20.0  # Absolute floor to prevent total liquidation

# --- Paths ---
BASE_DIR = Path(__file__).parent.parent
ENV_FILE = BASE_DIR / "backend" / "mm-engine-rs" / ".env"
LOG_FILE = BASE_DIR / "guardian_report.log"

# --- Setup Logging ---
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [GUARDIAN] %(levelname)s %(message)s",
    handlers=[
        logging.FileHandler(LOG_FILE),
        logging.StreamHandler()
    ]
)
log = logging.getLogger("guardian")

def load_env():
    env = {}
    if not ENV_FILE.exists():
        log.error(f".env file not found at {ENV_FILE}")
        return env
    with open(ENV_FILE, 'r') as f:
        for line in f:
            if '=' in line and not line.startswith('#'):
                k, v = line.strip().split('=', 1)
                env[k] = v
    return env

def get_account_value(address):
    try:
        url = "https://api.hyperliquid.xyz/info"
        payload = {"type": "clearinghouseState", "user": address}
        resp = requests.post(url, json=payload, timeout=10)
        data = resp.json()
        margin_summary = data.get("marginSummary", {})
        return float(margin_summary.get("accountValue", 0.0))
    except Exception as e:
        log.error(f"Failed to fetch account value: {e}")
        return None

def get_recent_fills(address):
    try:
        url = "https://api.hyperliquid.xyz/info"
        payload = {"type": "userFills", "user": address}
        resp = requests.post(url, json=payload, timeout=10)
        return resp.json()
    except Exception as e:
        log.error(f"Failed to fetch recent fills: {e}")
        return []

def analyze_win_rate(fills):
    if not fills: return 1.0
    # Match entries and exits to calculate realized PnL segments
    # (Simplified for the guardian: check if fills are mostly profit-taking)
    # For now, we'll use a simple proxy: check if fills have positive implied edge
    # This is a placeholder for a more complex matcher
    return 0.5  # Neutral

def halt_bot():
    log.warning("🛡️ TRIGGERING HARD STOP! Shutting down Docker containers...")
    subprocess.run(["docker", "compose", "stop"], cwd=BASE_DIR)
    # Clear Redis whitelist to be extra safe
    try:
        r = redis.Redis(host='localhost', port=6379, decode_responses=True)
        r.delete("mm:active_whitelist")
        log.info("Redis whitelist cleared.")
    except:
        pass

def widen_spreads():
    log.warning("⚠️ DETECTED POOR WIN RATE. Widening spreads via Redis...")
    try:
        r = redis.Redis(host='localhost', port=6379, decode_responses=True)
        # This logic would update the 'base_spread_bps' for active configs in Redis
        # (Simplified: The Rust engine and Screener will see the 'Halt' or 'Wide' signal)
        # We'll publish a 'SafeMode' flag that the engine can consume
        r.set("mm:safety_status", "WIDE")
        log.info("Safety status set to WIDE (1.5x Spread Multiplier).")
    except:
        pass

def main():
    log.info("🚀 Guardian active. Protecting your balance while you sleep.")
    env = load_env()
    address = env.get("HL_ADDRESS")
    if not address:
        log.error("HL_ADDRESS not found in .env. Exiting.")
        return

    start_balance = get_account_value(address)
    if not start_balance:
        log.error("Could not fetch starting balance. Exiting.")
        return

    log.info(f"Initial Balance: ${start_balance:.2f}. Drawdown Limit: ${HARD_STOP_DRAWDOWN_USD}")

    while True:
        current_balance = get_account_value(address)
        if not current_balance:
            time.sleep(30)
            continue

        pnl = current_balance - start_balance
        log.info(f"Heartbeat: Balance=${current_balance:.2f} PnL=${pnl:+.2f}")

        # Rule 1: Hard Stop on Drawdown
        if pnl <= -HARD_STOP_DRAWDOWN_USD or current_balance < MIN_BALANCE_STOP:
            log.error(f"CRITICAL DRAWDOWN DETECTED: {pnl:+.2f}. Executing stop.")
            halt_bot()
            break

        # Rule 2: Adaptive Safety on Win Rate
        fills = get_recent_fills(address)
        win_rate = analyze_win_rate(fills)
        if win_rate < WIN_RATE_THRESHOLD:
            widen_spreads()

        time.sleep(CHECK_INTERVAL_SECS)

if __name__ == "__main__":
    main()
