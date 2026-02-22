# HyperLiquidMM — Getting Started: From Clone to Live

> **Audience:** A developer who just cloned `https://github.com/aibysid/HyperLiquidMM.git`  
> **Goal:** Walk through every step — from build to live quoting — with nothing skipped  
> **Time to Live:** ~7–10 days (5+ days of data collection required before enabling live mode)

---

## Table of Contents

1. [Prerequisites](#1-prerequisites)
2. [Clone & Build](#2-clone--build)
3. [Credential Setup](#3-credential-setup)
4. [Understanding the Architecture](#4-understanding-the-architecture)
5. [Phase A — Shadow Mode Launch](#5-phase-a--shadow-mode-launch)
6. [Phase B — Data Collection (Days 1–5)](#6-phase-b--data-collection-days-15)
7. [Phase C — Data Pipeline Setup](#7-phase-c--data-pipeline-setup)
8. [Phase D — Gate Evaluation (Spread Calibration)](#8-phase-d--gate-evaluation-spread-calibration)
9. [Phase E — Shadow Mode Validation](#9-phase-e--shadow-mode-validation)
10. [Phase F — Go-Live Checklist](#10-phase-f--go-live-checklist)
11. [Phase G — Live Trading](#11-phase-g--live-trading)
12. [Ongoing Operations](#12-ongoing-operations)
13. [Troubleshooting](#13-troubleshooting)

---

## 1. Prerequisites

### System Requirements

| Requirement | Minimum | Recommended |
|---|---|---|
| **OS** | macOS / Linux | Ubuntu 22.04+ / macOS Sonoma+ |
| **RAM** | 2 GB | 4 GB |
| **Disk** | 5 GB free | 20 GB (tick data grows ~200MB/day) |
| **CPU** | 2 cores | 4 cores |
| **Network** | Stable broadband | Low-latency connection to Hyperliquid |

### Software Dependencies

```bash
# ── Rust (the engine is written in Rust) ──────────────────────────────────
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable   # or pin: rustup install 1.82.0 && rustup default 1.82.0

# ── Python 3.10+ (data pipeline & analysis) ───────────────────────────────
python3 --version   # must be 3.10+
pip3 install pyarrow duckdb pandas   # for Parquet conversion & analysis

# ── Redis (inter-process communication) ───────────────────────────────────
# macOS:
brew install redis
# Ubuntu:
sudo apt install redis-server

# ── Docker (optional, for production deployment) ──────────────────────────
# Only needed if you plan to run via Docker Compose
docker --version && docker-compose --version
```

### Hyperliquid Account Setup

1. **Create a Hyperliquid account** at [app.hyperliquid.xyz](https://app.hyperliquid.xyz)
2. **Fund the account** — deposit USDC (minimum $50 for testing, $200+ recommended)
3. **Export your wallet credentials:**
   - **Wallet Address** (`0x...`) — your public address
   - **Private Key** (`0x...`) — from MetaMask/Rabby → Account Details → Export Private Key

> ⚠️ **CRITICAL:** The private key gives full control of your funds. Never share it, never commit it to git, never paste it in public channels.

---

## 2. Clone & Build

```bash
git clone https://github.com/aibysid/HyperLiquidMM.git
cd HyperLiquidMM/backend/mm-engine-rs

# Build the engine in release mode (first build takes ~3-5 minutes)
cargo build --release

# Verify the binary exists
ls -la target/release/mm-engine-rs
```

**Expected output:** A binary at `target/release/mm-engine-rs` (~15-25MB).

If the build fails, check:
- `rustup show` — ensure Rust is installed
- `pkg-config --libs openssl` — OpenSSL dev headers needed for TLS

---

## 3. Credential Setup

```bash
cd /path/to/HyperLiquidMM/backend/mm-engine-rs

# Create your .env from the template
cp .env.example .env

# Edit with your real credentials
nano .env   # or vim, code, etc.
```

Your `.env` should look like:
```
HL_ADDRESS=0xYourActualWalletAddress
HL_PRIVATE_KEY=0xYourActualPrivateKey
```

**Verify `.env` is gitignored:**
```bash
cat ../../.gitignore | grep ".env"
# Should show: .env
```

> 🔒 The `.gitignore` already excludes `.env`. Never remove this rule.

---

## 4. Understanding the Architecture

Before running anything, understand what the bot does:

```
┌──────────────────────────────────────────────────────────────────────┐
│                        mm-engine-rs                                  │
│                                                                      │
│  ┌─────────┐    ┌────────────┐    ┌──────────┐    ┌──────────────┐  │
│  │ Ingestor │───▶│  Execution │───▶│  Market  │───▶│   Exchange   │  │
│  │ (L2 WS)  │    │   Engine   │    │  Maker   │    │   Client     │  │
│  └─────────┘    └────────────┘    └──────────┘    └──────────────┘  │
│       │              │                  │                │           │
│       ▼              ▼                  ▼                ▼           │
│  ┌─────────┐    ┌────────────┐    ┌──────────┐    ┌──────────────┐  │
│  │  Tick    │    │   Risk     │    │  Regime  │    │  Sim / Live  │  │
│  │ Harvest  │    │  Manager   │    │ Governor │    │  Exchange    │  │
│  │ (CSV)    │    │ (Drawdown) │    │ (Spread) │    │              │  │
│  └─────────┘    └────────────┘    └──────────┘    └──────────────┘  │
│                                                                      │
│  ┌───────────────────────────────────────────────────────────────┐   │
│  │  Redis Bridge: ← MmAssetConfig (from Screener)               │   │
│  │                 → ShadowFills, EngineStatus (to Screener)     │   │
│  └───────────────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────┘
```

### Key Concepts

| Concept | What It Means |
|---|---|
| **Shadow Mode** | Bot simulates orders but never sends them to the exchange. Uses Queue Position Estimator to model fills based on real market data. **This is the default.** |
| **Grid Laddering** | 3-tier bid/ask quotes: L1 (tight, small), L2 (wider, 2x size), L3 (deep, 3x size) |
| **Inventory Skewing** | When holding long inventory → lower asks (sell faster), when short → raise bids (buy faster). This is the "Soft Exit" mechanism. |
| **Regime Governor** | Dynamically widens spreads based on ATR volatility, cancel/fill ratio, and P95 latency |
| **Queue Position Estimator** | In shadow mode: estimates whether your resting limit order would have been filled based on volume traded at/through your price |
| **Tick Harvesting** | Every L2 snapshot is saved to CSV for later analysis |

### Source File Map

| File | Responsibility |
|---|---|
| `main.rs` | Orchestrator: env config, component init, main 100ms quoting loop |
| `market_maker.rs` | Grid computation, inventory skewing, QueuePositionEstimator, ShadowSession |
| `execution.rs` | Cancel-all, OFI detector, drawdown stop, state reconciliation |
| `exchange.rs` | `ExchangeClient` trait: `SimExchange` (shadow) vs `LiveExchange` (real orders) |
| `ingestor.rs` | L2 WebSocket subscriber, trade buffer, stall detection, tick harvester |
| `publisher.rs` | Redis pub/sub bridge to Python screener |
| `signing.rs` | Hyperliquid EIP-712 signing (MsgPack hash → Agent struct → signature) |
| `risk.rs` | Daily drawdown cap, consecutive loss halt, kill switch |
| `monitor.rs` | Rolling window performance metrics (win rate, profit factor) |
| `persistence.rs` | Engine state serialization/deserialization |

---

## 5. Phase A — Shadow Mode Launch

This is your **Day 1** activity. The goal: get the engine running, connected to Hyperliquid's WebSocket, and collecting tick data.

### Option 1: Direct Cargo Run (Recommended for first launch)

```bash
cd /path/to/HyperLiquidMM/backend/mm-engine-rs

# Shadow mode is ON by default. No credentials needed for shadow.
RUST_LOG=info cargo run --release 2>&1 | tee ../../logs/mm_shadow_$(date +%Y%m%d).log
```

### Option 2: Watchdog Script (Recommended for multi-day collection)

```bash
cd /path/to/HyperLiquidMM
chmod +x scripts/watchdog_mm.sh

# This auto-restarts the engine on crashes with exponential backoff
./scripts/watchdog_mm.sh
```

### Option 3: Docker Compose (Production)

```bash
cd /path/to/HyperLiquidMM

# Start Redis + Engine in shadow mode
docker-compose up -d

# Check logs
docker-compose logs -f mm-engine
```

### What To Expect On First Launch

```
🏦 mm-engine-rs starting (V6 Final Architecture)…
  Shadow Mode:    ON (no real orders)
  Tick Harvester: ENABLED
  Redis URL:      redis://127.0.0.1:6380
📡 Fetching Hyperliquid universe and asset contexts…
  Got 30 coins.
[SCREENER] Redis unavailable. Using default configs.     ← Expected if Redis not running
✅ All systems active. Entering main quoting loop…
[LATENCY] Latency: avg=42µs, p95=89µs, too_slow=false
[SHADOW FILL] BTC bid L1 @ 97342.100000 | sz=$12.00 | rebate=$0.0012 | total_rebates=$0.0012
[SHADOW SESSION] Shadow PnL: $0.0048 | Volume: $48.00 | Rebates: $0.0048 | Fills: 4
```

✅ **Checklist — Phase A complete when:**
- [ ] Engine starts without errors
- [ ] You see `Got N coins` (should be ~30)
- [ ] You see `[SHADOW FILL]` messages appearing (within first 5-10 minutes)
- [ ] Tick CSV files are being created in `data/ticks/BTC/YYYY-MM-DD.csv`

### Verify Tick Data Is Collecting

```bash
# Check that CSV files exist after a few minutes
find data/ticks/ -name "*.csv" | head -10

# Check file sizes (should grow over time)
du -sh data/ticks/*/

# Peek at the CSV content
head -5 data/ticks/BTC/$(date +%Y-%m-%d).csv
# Expected format: timestamp_ms,coin,best_bid,best_ask,mid_price,spread_bps
```

---

## 6. Phase B — Data Collection (Days 1–5)

**This is the mandatory waiting period.** The engine must collect at least 5 full days of tick data before you can meaningfully calibrate spreads and pass the gates.

### Why 5 Days?

| Day | What You Get |
|---|---|
| Day 1 | First snapshot of spreads, volume patterns, regime transitions |
| Day 2 | Weekend vs. weekday comparison (if applicable) |
| Day 3 | Enough data for Gate 1 (adverse selection analysis) |
| Day 4 | Hourly regime profiling becomes statistically meaningful |
| Day 5 | Gate 5 (production confidence) — engine has proven stable |

### Daily Data Volume

| Metric | Value |
|---|---|
| Coins tracked | ~30 (top by volume) |
| Tick frequency | ~100ms per coin |
| CSV row size | ~80 bytes |
| Daily data per coin | ~6.5 MB |
| **Daily total** | **~192 MB** |
| **5-day total** | **~1 GB** |

### Monitoring During Collection

Run these checks daily:

```bash
# 1. Is the engine still running?
ps aux | grep mm-engine

# 2. How much data collected today?
du -sh data/ticks/*/$(date +%Y-%m-%d).csv | sort -h | tail -5

# 3. Any gaps? (engine crashes create gaps)
python3 scripts/check_data_integrity.py --date $(date +%Y-%m-%d)

# 4. Shadow PnL report (from engine logs)
grep "SHADOW SESSION" logs/mm_shadow_*.log | tail -5
```

### Handling Engine Restarts

If the engine crashes or you need to restart it:
- **Watchdog mode:** Auto-restarts with exponential backoff (2s → 4s → 8s → ... → 32s max)
- **Docker mode:** `restart: unless-stopped` policy handles this automatically
- **Manual restart:** Just run `cargo run --release` again — tick data appends to the same daily CSV
- **Data gaps:** The integrity checker (`check_data_integrity.py`) will flag any gaps > 60 seconds

---

## 7. Phase C — Data Pipeline Setup

After collecting 3+ days of data, set up the analysis pipeline.

### Step 1: Convert CSV → Parquet

Parquet provides 6-10x compression and dramatically faster query speeds.

```bash
cd /path/to/HyperLiquidMM

# Install dependencies
pip3 install pyarrow

# Convert yesterday's data
python3 scripts/csv_to_parquet.py --date yesterday

# Convert all historical data
python3 scripts/csv_to_parquet.py --all

# Verify output
ls -la data/parquet/
# Expected: COIN/YYYY-MM-DD.parquet files
du -sh data/parquet/   # Should be ~5-10x smaller than CSV
```

### Step 2: Verify Data Integrity

```bash
# Scan all CSVs for corruption or gaps
python3 scripts/check_data_integrity.py --all

# Fix corrupted rows (rewrites CSVs in-place)
python3 scripts/check_data_integrity.py --all --fix
```

**Expected output:**
```
BTC/2026-02-20.csv: 847,293 rows, 0 corrupted, 0 gaps > 60s ✅
ETH/2026-02-20.csv: 845,112 rows, 0 corrupted, 0 gaps > 60s ✅
SOL/2026-02-20.csv: 841,890 rows, 0 corrupted, 1 gap (3m12s at 14:22 UTC) ⚠️
```

A few small gaps (< 5 minutes) from restarts are normal and won't affect calibration.

### Step 3: Set Up Daily Automation (Optional)

Add a cron job to convert yesterday's data to Parquet every night:

```bash
# crontab -e
0 1 * * * cd /path/to/HyperLiquidMM && python3 scripts/csv_to_parquet.py --date yesterday >> logs/parquet_conversion.log 2>&1
```

---

## 8. Phase D — Gate Evaluation (Spread Calibration)

This is the core calibration phase. You must pass **all 5 gates** before going live.

### Install Analysis Dependencies

```bash
pip3 install duckdb pyarrow pandas
```

### Gate 1: Adverse Selection Analysis

**Question:** "At my default 1.5bps spread, do I have positive edge or am I getting adversely selected?"

```bash
python3 scripts/analyze_ticks.py --gate 1 --coin BTC --days 3
```

**What to look for:**
```
Gate 1: Adverse Selection Analysis — BTC (3 days)
─────────────────────────────────────────────────
Median spread:           1.2 bps
Mean spread:             1.8 bps
Default base_spread_bps: 1.5 bps

Spread > base_spread:    62% of ticks  → ✅ POSITIVE EDGE
Spread < base_spread:    38% of ticks

Verdict: At 1.5bps, you would be competitive but with edge
         62% of the time your quote is inside the market spread
```

**Decision matrix:**

| Result | Action |
|---|---|
| `Spread > base` 60%+ of the time | ✅ Good — proceed with current `base_spread_bps` |
| `Spread > base` 40-60% of the time | ⚠️ Marginal — consider widening to 2.0 bps |
| `Spread > base` < 40% of the time | ❌ Negative edge — widen to 2.5–3.0 bps or skip this coin |

**Run for all major coins:**
```bash
for coin in BTC ETH SOL HYPE DOGE; do
    echo "=== $coin ===" 
    python3 scripts/analyze_ticks.py --gate 1 --coin $coin --days 3
    echo ""
done
```

### Gate 2: Hourly Regime Profiling

**Question:** "Which hours have chaotic spreads that I should avoid or widen spreads for?"

```bash
python3 scripts/analyze_ticks.py --gate 2 --coin BTC --days 5
```

**What to look for:**
```
Gate 2: Hourly Regime Profile — BTC
────────────────────────────────────
Hour(UTC)  Median_Spread  Volatility   Regime        Spread_Mult
  00          1.1 bps       Low        calm           1.0x
  01          1.0 bps       Low        calm           1.0x
  ...
  13          2.8 bps       High       uncertain      2.0x   ← US market open
  14          3.1 bps       High       uncertain      2.0x
  15          1.8 bps       Med        calm           1.0x
  ...
  20          4.2 bps       Very High  halt           —      ← News hour
```

**Action items:**
- Hours marked `halt` → Engine should NOT quote (Regime Governor handles this)
- Hours marked `uncertain` → Engine auto-widens spreads (Regime Governor handles this)
- Verify the Regime Governor thresholds in `market_maker.rs` match your data:
  - `calm_atr_threshold: 0.0015` (< 0.15% per interval = calm)
  - `chaotic_atr_threshold: 0.005` (> 0.5% per interval = halt)

### Gate 3: Per-Coin Spread Calibration

**Question:** "What should `base_spread_bps` be for each coin?"

```bash
python3 scripts/analyze_ticks.py --gate 3 --days 5
```

**Expected output:**
```
Gate 3: Spread Calibration Recommendations
──────────────────────────────────────────
Coin    Median_Spread  P25_Spread   Recommended_Base   Current_Default  Action
BTC        1.2 bps       0.8 bps      1.5 bps            1.5 bps       ✅ Keep
ETH        1.4 bps       1.0 bps      1.5 bps            1.5 bps       ✅ Keep
SOL        2.1 bps       1.5 bps      2.0 bps            1.5 bps       ⚠️ Widen
HYPE       3.8 bps       2.5 bps      3.5 bps            1.5 bps       ⚠️ Widen
DOGE       2.5 bps       1.8 bps      2.5 bps            1.5 bps       ⚠️ Widen
```

**Rule of thumb:** Set `base_spread_bps` to approximately the **25th percentile** of observed spreads. This ensures you're inside the market spread ~75% of the time — enough to get fills, but not so tight that adverse selection eats your edge.

### Gate 4: Shadow PnL Positive

**Question:** "Is the shadow simulation showing positive PnL?"

```bash
# Check the engine logs for the latest shadow session report
grep "SHADOW SESSION" logs/mm_shadow_*.log | tail -20
```

**What to look for:**
```
[SHADOW SESSION] Shadow PnL: $0.1824 | Volume: $1,560.00 | Rebates: $0.1560 | Fills: 130
```

**Pass criteria:**
- [ ] Shadow PnL is positive over 3+ consecutive days
- [ ] Fill rate is reasonable (at least 10+ fills/hour across all coins)
- [ ] No single coin is dominating losses (diversification check)

### Gate 5: Production Stability (5 Days)

**Question:** "Has the engine run stably for 5 full days without manual intervention?"

**Pass criteria:**
- [ ] Engine has been running for 5+ calendar days
- [ ] No unrecoverable crashes (watchdog restarts are OK)
- [ ] Data integrity checks pass with < 1% gap time
- [ ] No `🛑 GLOBAL STOP` events in logs
- [ ] No `🚨 STALL PANIC` events lasting > 5 minutes

```bash
# Check for panics/stops
grep -c "GLOBAL STOP\|STALL PANIC" logs/mm_shadow_*.log
# Should be 0 or very few

# Check uptime
grep "mm-engine-rs starting" logs/mm_shadow_*.log | wc -l
# Number of restarts — fewer is better, but a few are acceptable
```

---

## 9. Phase E — Shadow Mode Validation

Before going live, do a final comprehensive validation:

### Validation Checklist

```bash
# ── 1. Engine Health ──────────────────────────────────────────────────────
# Is it running right now?
ps aux | grep mm-engine

# Latest latency report
grep "LATENCY" logs/mm_shadow_*.log | tail -1
# Expected: p95 < 50ms (50,000µs)

# ── 2. Data Quality ──────────────────────────────────────────────────────
python3 scripts/check_data_integrity.py --all
# Expected: All coins show < 1% gap time

# ── 3. Spread Calibration Complete ────────────────────────────────────────
python3 scripts/analyze_ticks.py --gate 3 --days 5
# Expected: Clear recommendation for each coin's base_spread_bps

# ── 4. Shadow PnL Trajectory ──────────────────────────────────────────────
grep "SHADOW SESSION" logs/mm_shadow_*.log | tail -20
# Expected: Consistently positive, growing over time

# ── 5. Safety Systems Test ────────────────────────────────────────────────
# Verify stall detection works: temporarily kill your internet for 45 seconds
# Expected in logs: "🚨 NETWORK STALL DETECTED" followed by "WS appears recovered"
```

### Update Spread Parameters

Based on Gate 3 results, you may need to update the default `MmAssetConfig` values. 

Currently these are set in `market_maker.rs`:
```rust
impl Default for MmAssetConfig {
    fn default() -> Self {
        Self {
            base_spread_bps: 1.5,  // ← Update based on Gate 3
            max_inv_usd: 200.0,    // ← Max inventory per coin
            min_order_usd: 12.0,   // ← HL minimum ~$10
            // ...
        }
    }
}
```

**For production,** the Python Screener (not yet implemented) will publish per-coin configs via Redis, overriding these defaults. Until the screener is built, these defaults apply to all coins.

If you need per-coin spreads NOW without the screener, edit the defaults and rebuild:
```bash
# After editing market_maker.rs
cargo build --release
```

---

## 10. Phase F — Go-Live Checklist

**Do not proceed unless ALL boxes are checked.**

### Pre-Flight Checklist

```
GATE STATUS (all must be ✅):

[ ] Gate 1: Adverse selection — positive edge confirmed for target coins
[ ] Gate 2: Regime profiling — chaotic hours identified, governor thresholds tuned
[ ] Gate 3: Spread calibration — base_spread_bps set per coin from data
[ ] Gate 4: Shadow PnL — positive for 3+ consecutive days
[ ] Gate 5: Stability — 5+ days continuous operation, no unrecoverable crashes

INFRASTRUCTURE:

[ ] .env contains real HL_ADDRESS and HL_PRIVATE_KEY
[ ] Account has sufficient USDC balance ($200+ recommended)
[ ] Redis is running (for screener bridge, even if screener not yet built)
[ ] Monitoring setup: logs being captured, alerts configured
[ ] Internet connection is stable (wired preferred over WiFi)
[ ] You will be available to monitor for the first 2 hours after going live

SAFETY VERIFICATION:

[ ] Global drawdown stop is set (default 5% — review in execution.rs)
[ ] OFI halt threshold is reasonable (default 70%)
[ ] Cancel/Fill ratio guard is active (default 50:1)
[ ] Kill switch script works: scripts/kill_switch.sh (if it exists in hypersim)
```

---

## 11. Phase G — Live Trading

### Starting Live Mode

**⚠️ This sends real orders to Hyperliquid with real money.**

```bash
cd /path/to/HyperLiquidMM/backend/mm-engine-rs

# Method 1: Direct (will prompt for confirmation)
MM_SHADOW_MODE=false RUST_LOG=info cargo run --release

# Method 2: Via start script (includes safety prompt)
cd /path/to/HyperLiquidMM
./scripts/start_mm_bot.sh live

# Method 3: Docker (edit docker-compose.yml first)
# Change: MM_SHADOW_MODE=false
docker-compose up -d
```

**The engine will print:**
```
⚠️  LIVE MODE — real orders will be placed!
Shadow Mode: ⚠️  LIVE!
```

### First 2 Hours — Active Monitoring

Stay at your terminal and watch for:

```bash
# In a separate terminal, tail the logs
tail -f logs/mm_live_$(date +%Y%m%d).log | grep -E "LIVE QUOTE|FILL|CANCEL|STOP|PANIC|ERROR"
```

| Log Pattern | Meaning | Action |
|---|---|---|
| `[LIVE QUOTE] BTC mid=97342 bids=3 asks=3` | Normal quoting | ✅ None |
| `[EXEC] cancel_all() triggered` | Emergency cancel fired | ⚠️ Check why |
| `🛑 [GLOBAL STOP]` | Drawdown limit hit | 🔴 Engine halted — investigate before restarting |
| `🚨 STALL PANIC` | No WS data for 30s | ⚠️ Check internet — engine will auto-recover |
| `[RECONCILE] ⚠️ dark fill(s)` | Fills happened during WS outage | Review position |

### Emergency Stop

If anything looks wrong:

```bash
# Option 1: Kill the engine process
pkill -f mm-engine-rs

# Option 2: Cancel all orders via API (if engine is unresponsive)
# You'll need to do this manually through the Hyperliquid web UI:
# app.hyperliquid.xyz → Portfolio → Cancel All Orders

# Option 3: Docker
docker-compose down
```

---

## 12. Ongoing Operations

### Daily Routine

```bash
# ── Morning (check overnight performance) ─────────────────────────────────
# 1. Check engine is running
ps aux | grep mm-engine

# 2. Check overnight PnL
grep "SHADOW SESSION\|daily_pnl" logs/mm_*.log | tail -5

# 3. Convert yesterday's ticks to Parquet
python3 scripts/csv_to_parquet.py --date yesterday

# 4. Check data integrity
python3 scripts/check_data_integrity.py --date yesterday

# ── Weekly (recalibrate) ──────────────────────────────────────────────────
# 5. Re-run Gate 3 with latest data
python3 scripts/analyze_ticks.py --gate 3 --days 7

# 6. Compare recommended spreads against current settings
# If spreads have shifted significantly, update and rebuild
```

### Log Rotation

Logs grow quickly with `RUST_LOG=info`. Set up rotation:

```bash
# Manual: compress old logs
gzip logs/mm_shadow_2026-02-2*.log

# Automated: add to crontab
0 2 * * * find /path/to/HyperLiquidMM/logs -name "*.log" -mtime +7 -exec gzip {} \;
```

### Disk Management

```bash
# Check tick data disk usage
du -sh data/ticks/

# Remove old CSVs after Parquet conversion (keep 7 days of CSV, Parquet is permanent)
find data/ticks/ -name "*.csv" -mtime +7 -delete

# Check Parquet storage
du -sh data/parquet/
```

---

## 13. Troubleshooting

### Engine Won't Start

| Error | Fix |
|---|---|
| `HL_ADDRESS must be set in live mode` | Add `HL_ADDRESS` to `.env` (or run in shadow mode) |
| `HL_PRIVATE_KEY must be set in live mode` | Add `HL_PRIVATE_KEY` to `.env` |
| `LiveExchange init failed` | Check internet connection, verify API endpoint is reachable |
| `error[E0433]: could not find...` | Run `cargo clean && cargo build --release` |
| Port 6380 in use | Another Redis instance — change `REDIS_URL` env var |

### No Shadow Fills Appearing

- **Wait 10+ minutes** — fills depend on market activity at your quote prices
- Check that `base_spread_bps` isn't too wide (e.g., 10 bps = almost never fills)
- Check that coins are actually selected: look for `Got N coins` in startup logs
- Verify the QueuePositionEstimator is receiving trades: `grep "on_trade" logs/*` should NOT appear (it's not logged by default)

### Data Gaps

```bash
# Identify gaps
python3 scripts/check_data_integrity.py --date YYYY-MM-DD --coin BTC

# Common causes:
# 1. Engine restart → normal, gap = restart duration
# 2. WS disconnect → engine auto-reconnects (gap should be < 60s)
# 3. Hyperliquid maintenance → check their Discord for announcements
```

### High Cancel/Fill Ratio Warning

```
⚠️ C/F ratio 62.0 > limit.
```

This means you're cancelling 62 orders for every fill. The Regime Governor will auto-widen spreads. If it persists:
- Widen `base_spread_bps` by 0.5 bps
- Increase `max_cancel_fill_ratio` in `execution.rs` (default: 50)
- Check if a specific coin is causing most cancels

### Stall Panic Won't Clear

```
🚨 STALL PANIC: cancel_all
```

If this happens repeatedly:
1. Check internet connection
2. Check if Hyperliquid WS is down (their Discord)
3. If your IP is rate-limited, wait 5 minutes
4. Restart the engine: `pkill -f mm-engine && sleep 5 && cargo run --release`

---

## Quick Reference Card

```
┌─────────────────────────────────────────────────────────────────┐
│                    IMPORTANT COMMANDS                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  BUILD:     cargo build --release                               │
│  SHADOW:    RUST_LOG=info cargo run --release                   │
│  LIVE:      MM_SHADOW_MODE=false RUST_LOG=info cargo run --rel  │
│  STOP:      pkill -f mm-engine-rs                               │
│  WATCHDOG:  ./scripts/watchdog_mm.sh                            │
│                                                                 │
│  CONVERT:   python3 scripts/csv_to_parquet.py --all             │
│  ANALYZE:   python3 scripts/analyze_ticks.py --gate 3 --days 5  │
│  INTEGRITY: python3 scripts/check_data_integrity.py --all       │
│                                                                 │
│  LOGS:      tail -f logs/mm_shadow_$(date +%Y%m%d).log          │
│  DISK:      du -sh data/ticks/ data/parquet/                    │
│  STATUS:    ps aux | grep mm-engine                             │
│                                                                 │
│  ENV VARS:                                                      │
│    MM_SHADOW_MODE  = true (default) / false (live)              │
│    MM_HARVEST_TICKS = true (default) / false                    │
│    REDIS_URL       = redis://127.0.0.1:6380 (default)           │
│    RUST_LOG        = info / debug / warn                        │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## Timeline Summary

```
Day 0:    Clone → Build → Launch Shadow Mode
Day 1-2:  Collecting tick data (engine running, just monitoring)
Day 3:    First analysis: Run Gate 1 & 2 (adverse selection + regime)
Day 4:    Run Gate 3 (spread calibration), update parameters if needed
Day 5:    Gate 5 passes (5-day stability). Full gate review.
Day 5-6:  Final shadow validation. Rebuild with tuned parameters.
Day 7:    Go-live checklist → Enable live mode → Monitor actively for 2h
Day 7+:   Daily monitoring, weekly recalibration
```

> **Remember:** The bot is designed to be safe by default. Shadow mode is always ON unless you explicitly set `MM_SHADOW_MODE=false`. There's no way to accidentally trade real money.
