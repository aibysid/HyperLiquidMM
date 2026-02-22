# Market Maker Bot — Architecture & Operations Guide

> **Version:** V6 Final Architecture  
> **Status:** Shadow Mode Active — data collection in progress  
> **Last Updated:** 2026-02-22

---

## Table of Contents

1. [Overview](#1-overview)
2. [Architecture](#2-architecture)
3. [Repository Structure](#3-repository-structure)
4. [How It Works — Phase by Phase](#4-how-it-works--phase-by-phase)
5. [Configuration Reference](#5-configuration-reference)
6. [Running the Bot](#6-running-the-bot)
7. [Data Pipeline](#7-data-pipeline)
8. [Safety Systems](#8-safety-systems)
9. [Monitoring & Observability](#9-monitoring--observability)
10. [Going Live — Gate Checklist](#10-going-live--gate-checklist)
11. [Troubleshooting](#11-troubleshooting)

---

## 1. Overview

This is a **pure liquidity-provision (market making) bot** for [Hyperliquid](https://hyperliquid.xyz) perpetual futures. It is entirely separate from the directional scalping engine (`engine-rs`) — different binary, different Redis instance, different Docker stack.

**What it does:**
- Subscribes to live L2 order book data for **30 top coins** via Hyperliquid WebSocket
- Computes a **3-tier bid/ask quote grid** using inventory skewing and regime detection
- In **Shadow Mode** (default): simulates fills using a Queue Position Estimator, accumulates paper PnL
- In **Live Mode**: places real Post-Only limit orders, earns maker rebates (~0.01%)
- Harvests every L2 book snapshot to CSV → Parquet for spread calibration

**What it is NOT:**
- Not a directional bot (no trend following, no stop-loss, no TP)
- Not a high-frequency arbitrage bot
- Not a CEX/DEX arbitrage system

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                    MARKET MAKER BOT (V6)                            │
│                                                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │              mm-engine-rs  (Rust, async/tokio)              │   │
│  │                                                             │   │
│  │  ┌──────────────┐   ┌──────────────┐   ┌───────────────┐  │   │
│  │  │  ingestor.rs │   │  execution.rs│   │market_maker.rs│  │   │
│  │  │              │   │              │   │               │  │   │
│  │  │ • WS connect │   │ • cancel_all │   │ • 3-tier grid │  │   │
│  │  │ • l2Book sub │   │ • reconcile  │   │ • inv. skew   │  │   │
│  │  │ • trades sub │   │ • drawdown   │   │ • regime gov  │  │   │
│  │  │ • stall watch│   │ • OFI halt   │   │ • queue est.  │  │   │
│  │  │ • tick CSV   │   │ • C/F ratio  │   │ • shadow PnL  │  │   │
│  │  └──────┬───────┘   └──────┬───────┘   └───────┬───────┘  │   │
│  │         │                  │                    │           │   │
│  │         └──────────────────┴────────────────────┘           │   │
│  │                         main.rs                             │   │
│  │                    (100ms quoting loop)                     │   │
│  └──────────────────────────┬──────────────────────────────────┘   │
│                             │                                       │
│         ┌───────────────────┼───────────────────┐                  │
│         ▼                   ▼                   ▼                  │
│  ┌─────────────┐   ┌──────────────┐   ┌──────────────────┐        │
│  │  mm-redis   │   │  Hyperliquid │   │  data/ticks/     │        │
│  │  port 6380  │   │  WebSocket   │   │  COIN/DATE.csv   │        │
│  │             │◄──│  (live data) │   │  (tick harvest)  │        │
│  │ mm:asset_   │   │  wss://api.  │   └──────────────────┘        │
│  │ config      │   │  hyperliquid │                                │
│  │ mm:shadow_  │   │  .xyz/ws     │                                │
│  │ fills       │   └──────────────┘                                │
│  └──────┬──────┘                                                   │
│         │                                                           │
│         ▼                                                           │
│  ┌─────────────┐                                                   │
│  │ mm-screener │  (Python — not yet implemented)                   │
│  │             │  • Ranks coins by edge                            │
│  │             │  • Publishes MmAssetConfig                        │
│  └─────────────┘                                                   │
└─────────────────────────────────────────────────────────────────────┘
```

### Component Responsibilities

| Component | Language | Role |
|---|---|---|
| `mm-engine-rs` | Rust (tokio) | Core engine: WS ingestor, quoting loop, risk gates, tick harvester |
| `mm-redis` | Redis 7 | IPC between engine and Python screener |
| `mm-screener` | Python (planned) | Asset selection, per-coin config publisher |
| `csv_to_parquet.py` | Python | Daily converter: CSV → zstd Parquet |
| `analyze_ticks.py` | Python + DuckDB | Spread calibration analysis (Gates 1-3) |
| `check_data_integrity.py` | Python | Gap detection, corruption scan |
| `watchdog_mm.sh` | Bash | Auto-restart on crash (dev/macOS) |

---

## 3. Repository Structure

```
scratch/
├── backend/
│   └── mm-engine-rs/           # Rust MM engine
│       ├── Cargo.toml
│       ├── Dockerfile
│       ├── .env                 # ⚠️ NEVER commit — HL credentials
│       ├── .dockerignore
│       └── src/
│           ├── main.rs          # Entry point, quoting loop, task orchestration
│           ├── market_maker.rs  # Grid math, Regime Governor, Queue Estimator
│           ├── execution.rs     # cancel_all, reconcile, OFI, drawdown guard
│           ├── ingestor.rs      # WebSocket, L2 book, tick CSV harvester
│           ├── exchange.rs      # ExchangeClient trait, SimExchange, LiveExchange
│           ├── publisher.rs     # Redis IPC bridge (screener ↔ engine)
│           ├── signing.rs       # EIP-712 signing for Hyperliquid
│           ├── risk.rs          # RiskManager circuit breakers
│           ├── monitor.rs       # PerformanceMonitor
│           └── persistence.rs   # EngineState save/load
│
├── scripts/
│   ├── start_mm_bot.sh          # Production start (Docker, requires confirmation for live)
│   ├── watchdog_mm.sh           # Dev start (auto-restart on crash, macOS)
│   ├── csv_to_parquet.py        # Daily CSV → Parquet converter
│   ├── analyze_ticks.py         # DuckDB-powered spread calibration
│   └── check_data_integrity.py  # Gap detection & corruption scan
│
├── data/
│   ├── mm_ticks/                # Host-mounted volume (survives Docker rebuilds)
│   └── parquet/                 # Converted Parquet files (COIN/DATE.parquet)
│
├── logs/
│   └── mm/                      # Engine logs (mounted from container)
│
└── docker-compose.mm.yml        # MM stack (isolated from directional bot)
```

---

## 4. How It Works — Phase by Phase

### Phase 9A: Structural Partitioning ✅
The MM engine is a completely separate Rust binary from the directional scalping bot:
- Different package: `mm-engine-rs`
- No shared code with `engine-rs`
- No optimizer, backtester, or directional strategy code

### Phase 9B: L2 WebSocket Ingestion ✅
On startup, `ingestor.rs`:
1. Fetches the Hyperliquid universe via REST (`/info` → `metaAndAssetCtxs`)
2. Selects the **top 30 assets by 24h volume**
3. Opens a WebSocket to `wss://api.hyperliquid.xyz/ws`
4. Subscribes to `l2Book` (for quoting) and `trades` (for OFI) for all 30 coins
5. Runs a **Stall Watcher** — if no WS message for 30s, sets `StallPanicFlag`
6. Reconnects with exponential backoff on disconnect

### Phase 9C: Tick Data Harvester ✅
Every L2 book snapshot is appended to a daily CSV:
```
data/ticks/BTC/2026-02-22.csv
timestamp_ms, coin, bid, ask, mid, spread_bps
1771733346055, BTC, 68020.0, 68021.0, 68020.5, 0.147
```
- Controlled by `MM_HARVEST_TICKS=true` (default: on)
- ~192 MB/day across 30 coins as CSV
- ~25 MB/day after Parquet conversion (8x compression)

### Phase 9D: Latency Auditor ✅
`LatencyAuditor` tracks processing latency using a 10,000-sample ring buffer. P95 latency is logged every 30 seconds and fed into the Regime Governor to widen spreads when the engine is too slow.

### Phase 9E: Protective Halts & State Reconciliation ✅

**Stall Panic:** If no WS message for 30s:
1. `cancel_all()` fires — cancels every resting order via REST
2. WS reconnects automatically
3. `reconcile_after_reconnect()` compares REST positions vs internal inventory
4. Dark fills are detected and logged
5. Quoting resumes

**Global Drawdown Stop:** Every 60s:
- Daily PnL is checked against `global_halt_drawdown_pct` (default: 5%)
- If breached: `cancel_all()` → `halted = true`
- Engine stays halted until manually restarted

**OFI (Order Flow Imbalance) Halt:** Per-tick:
- 200-trade rolling window of taker buy vs. sell volume
- If sell dominance > 70% → bid side is suppressed
- If buy dominance > 70% → ask side is suppressed

### Phase 9F: 3-Tier Grid Laddering ✅

For each active coin each 100ms tick:
```
L1 (tightest):  price = mid ± 1.5bps  |  size = $12 (min)
L2 (mid):       price = mid ± 3.75bps |  size = $24 (2x)
L3 (deep):      price = mid ± 7.5bps  |  size = $36 (3x)
```

**Inventory Skewing (Soft Exit):**
When we accumulate inventory, the entire grid shifts to accelerate unwinding while staying Maker:
```
Long inventory  → bid prices drop (less eager to buy) + ask prices also drop (sell cheaper)
Short inventory → ask prices rise + bid prices also rise
```

### Phase 9G: Regime Governor ✅

Computes a single `spread_multiplier` ∈ [1.0x, 4.0x] from:
1. **ATR fraction** (realized volatility) → calm < 0.15% < uncertain < 0.5% = halt
2. **Cancel-to-Fill ratio** → if > 50, widen to reduce API activity
3. **P95 latency** → if > 50ms: 1.5x, if > 100ms: 2.0x
4. **Funding rate** → if |funding| > 0.3% per 8h: halt quoting

This multiplier is applied to every layer of the grid.

### Phase 9H: Python Screener Bridge ✅

Redis channels:
- `mm:asset_config` — Python → Rust: `Vec<MmAssetConfig>` JSON every 30s
  - Contains per-coin: `base_spread_bps`, `max_inv_usd`, `atr_fraction`, `regime`
- `mm:shadow_fills` — Rust → Python: every shadow fill event
- `mm:engine_status` — Rust → Python: heartbeat + session PnL

Without Redis/screener running: engine uses flat defaults (1.5bps for all coins).

### Phase 9I: Queue Position Estimator (Shadow Mode) ✅

In shadow mode, the engine simulates whether our resting limit orders would have been filled:

1. Registers a shadow order at our computed grid price
2. Only feeds **new taker trades** (timestamp watermark prevents double-counting)
3. Estimates fill probability: `vol_traded_through / (2 × order_size)`
4. At 70% probability threshold → records as a `ShadowFill`
5. Updates internal inventory to simulate the position change

### Phase 9J: Live Quoting ⚠️ (stub — pending calibration)

In live mode, the engine:
1. Uses `LiveExchange` instead of `SimExchange`
2. Calls `exchange.open_order()` with `post_only: true` for each grid level
3. Cancels and replaces orders when price moves > 1 tick from the grid target
4. Tracks open order OIDs in `InternalInventory`

**This is not yet wired** — requires completing the spread calibration (Gates 1-5) first.

---

## 5. Configuration Reference

### `.env` file (`backend/mm-engine-rs/.env`)

```bash
# Hyperliquid credentials (required for live mode only)
HL_ADDRESS=0xYOUR_WALLET_ADDRESS
HL_PRIVATE_KEY=0xYOUR_PRIVATE_KEY

# Optional — defaults shown
# REDIS_URL=redis://127.0.0.1:6380
```

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `MM_SHADOW_MODE` | `true` | `true` = no real orders. `false` = live trading |
| `MM_HARVEST_TICKS` | `true` | Write tick data to CSV. Set `false` to disable |
| `REDIS_URL` | `redis://127.0.0.1:6380` | Redis for screener IPC |
| `RUST_LOG` | `info` | Log verbosity: `error`, `warn`, `info`, `debug` |
| `HL_ADDRESS` | — | Hyperliquid wallet address (live mode only) |
| `HL_PRIVATE_KEY` | — | Hyperliquid private key (live mode only) |

### Engine Config (`execution.rs` → `MmEngineConfig`)

| Parameter | Default | Description |
|---|---|---|
| `global_halt_drawdown_pct` | `0.05` | 5% daily loss cap before engine halts |
| `max_cancel_fill_ratio` | `50.0` | Max cancels per fill before widening spreads |
| `ofi_halt_threshold` | `0.70` | 70% side dominance triggers bid/ask suppression |
| `shadow_mode` | `true` | Mirrors `MM_SHADOW_MODE` env var |

### Per-Asset Config (`market_maker.rs` → `MmAssetConfig`)

Published by the Python screener via Redis. Defaults used when screener is not running:

| Parameter | Default | Description |
|---|---|---|
| `base_spread_bps` | `1.5` | Half-spread per side in basis points |
| `max_inv_usd` | `200.0` | Max inventory before full skewing kicks in |
| `min_order_usd` | `12.0` | Minimum order size (L1 size) |
| `atr_fraction` | `0.002` | Expected ATR as fraction of price (0.2%) |
| `regime` | `"calm"` | `"calm"`, `"uncertain"`, or `"halt"` |

---

## 6. Running the Bot

### Prerequisites

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Python deps (for data tools)
pip3 install pyarrow duckdb polars pandas --break-system-packages

# Docker (for production)
# https://docs.docker.com/get-docker/
```

### Option A: Dev Mode (macOS, watchdog auto-restart)

```bash
cd /path/to/scratch

# Start with auto-restart on crash
./scripts/watchdog_mm.sh

# The watchdog:
# 1. Builds binary if needed
# 2. Starts engine in shadow mode
# 3. Auto-restarts on crash with exponential backoff
# 4. Logs gaps to logs/mm_watchdog.log
# 5. Ctrl+C to stop cleanly
```

### Option B: Direct Cargo Run (dev, manual)

```bash
cd backend/mm-engine-rs
RUST_LOG=info cargo run --release
```

### Option C: Docker (production, recommended)

```bash
# Shadow mode (safe default)
./scripts/start_mm_bot.sh

# This starts:
#   mm-redis    (port 6380)
#   mm-engine   (shadow mode, connected to HL WS)
#   mm-screener (when implemented)

# View logs
docker logs -f mm_engine

# Stop
docker compose -f docker-compose.mm.yml down
```

### Switching to Live Trading

**Only do this after all 5 gates are cleared.**

```bash
# Method 1: Docker
MM_SHADOW_MODE=false ./scripts/start_mm_bot.sh
# → Requires typing "CONFIRM LIVE" at the prompt

# Method 2: Environment variable (dev)
MM_SHADOW_MODE=false ./scripts/watchdog_mm.sh
```

---

## 7. Data Pipeline

### Collection (continuous, automatic)

```
Hyperliquid WS
    │  L2 book updates (every ~650ms per coin)
    ▼
ingestor.rs → harvest_tick_to_csv()
    │
    ▼
data/ticks/COIN/YYYY-MM-DD.csv
    fields: ts_ms, coin, bid, ask, mid, spread_bps
```

### Daily Conversion (run each morning)

```bash
# Convert previous day's CSVs to Parquet
python3 scripts/csv_to_parquet.py
# → data/parquet/COIN/YYYY-MM-DD.parquet (zstd, 6-10x compression)

# Or convert everything at once
python3 scripts/csv_to_parquet.py --all
```

### Analysis (run after Gate 1 data is present)

```bash
# Full calibration analysis (all gates)
python3 scripts/analyze_ticks.py

# Single coin
python3 scripts/analyze_ticks.py --coin BTC

# Specific gate
python3 scripts/analyze_ticks.py --gate 1

# Ad-hoc SQL with DuckDB
duckdb -c "SELECT coin, avg(spread_bps), count(*) 
           FROM read_parquet('data/parquet/*/*.parquet') 
           GROUP BY coin ORDER BY avg(spread_bps)"
```

### Integrity Check (run anytime)

```bash
python3 scripts/check_data_integrity.py

# Fix corrupted rows in-place
python3 scripts/check_data_integrity.py --fix
```

---

## 8. Safety Systems

### 8.1 Shadow Mode (default: ON)

`MM_SHADOW_MODE=true` means:
- `SimExchange` is used — no API calls to Hyperliquid
- `cancel_all()` is a no-op (logs only)
- Orders are simulated via Queue Position Estimator
- Shadow fills are recorded in `ShadowSession`

### 8.2 Stall Panic

Triggered when no WS message received for **30 seconds**:
1. `cancel_all()` — removes all resting orders before we are "dark"  
2. WS reconnects with backoff
3. `reconcile_after_reconnect()` — REST diff to catch any fills that happened

### 8.3 Global Drawdown Stop

- Checked every 60 seconds
- If `daily_pnl < -(starting_balance × 5%)` → engine halts
- `cancel_all()` is called, engine sets `halted = true`
- **Requires manual restart to resume**

### 8.4 OFI (Order Flow Imbalance) Halt

- 200-trade rolling window per coin
- `OFI = (buy_vol - sell_vol) / total_vol` → range [-1, +1]
- If `OFI < -0.70` (strong sell pressure) → bid quotes suppressed
- If `OFI > +0.70` (strong buy pressure) → ask quotes suppressed
- Auto-recovers when OFI normalizes

### 8.5 Cancel-to-Fill Ratio Guard

- Tracked in `SessionStats`  
- If `total_cancels / total_fills > 50` → Regime Governor widens spreads
- Prevents exchange rate-limiting / soft bans from excessive cancel activity

### 8.6 Start Script Safety Gate

`scripts/start_mm_bot.sh` requires typing `"CONFIRM LIVE"` before starting with `MM_SHADOW_MODE=false`. Docker has `restart: unless-stopped` but the confirmation gate prevents accidental live starts.

---

## 9. Monitoring & Observability

### Log Patterns to Watch

```bash
# Normal operation (shadow mode)
[SHADOW FILL] BTC bid L1 @ 68020.5 | sz=$12.00 | rebate=$0.0012 | total_rebates=$X.XX

# Regime change (spread widening)
[REGIME] ATR spike detected. Multiplier: 1.0x → 2.3x

# OFI protection firing
[OFI] Sell dominance 78.3% > threshold. Bids suppressed for BTC.

# Stall panic (engine missed WS heartbeat)
🚨 STALL PANIC: cancel_all.
[STALL] WS back. Reconciling.
[RECONCILE] ✅ Inventory matches. No dark fills detected.

# Global stop triggered
🛑 [GLOBAL STOP] Daily drawdown 5.12% >= cap 5.00%. Halting all quoting.

# Latency report
[LATENCY] P95: 12.4ms | P99: 28.1ms | samples: 9483
```

### Session PnL (Shadow Mode)

Reported every 60 seconds:
```
[SHADOW SESSION] Shadow PnL: $X.XX | Volume: $XXX | Rebates: $X.XX | Fills: NNN
```

### Redis Monitoring (when screener is active)

```bash
redis-cli -p 6380 subscribe mm:shadow_fills mm:engine_status
```

---

## 10. Going Live — Gate Checklist

Complete these 5 gates **in order** before setting `MM_SHADOW_MODE=false`.

### Gate 1 — Adverse Selection Baseline (~3–6 hours of data)
```bash
python3 scripts/analyze_ticks.py --gate 1
```
- [ ] Identify coins with positive edge: `avg_spread_bps > adverse_selection_bps`
- [ ] Remove coins with negative edge from the active set
- [ ] Document which coins to exclude from live quoting

### Gate 2 — Hourly Regime Profiling (~12–24 hours of data)
```bash
python3 scripts/analyze_ticks.py --gate 2
```
- [ ] Identify "chaotic" UTC hours (high avg + high std spread) 
- [ ] Set `RegimeGovernor.calm_atr_threshold` based on observed calm spread
- [ ] Configure `RegimeGovernor.chaotic_atr_threshold` to halt in bad sessions

### Gate 3 — Spread Calibration (~1.5 days of data)
```bash
python3 scripts/analyze_ticks.py --gate 3
```
- [ ] Review recommended `base_spread_bps` per coin
- [ ] Update defaults in `MmAssetConfig` or Python screener
- [ ] Verify recommended spread > min_viable (1.0 bps) for all active coins

### Gate 4 — Weekend vs Weekday Regime (~3 days of data)
- [ ] Has the engine run through at least one Saturday?
- [ ] Has weekend data been analysed separately from weekday data?
- [ ] Are weekend spread multipliers set correctly in Regime Governor?

### Gate 5 — Production Go/No-Go (~5 days of data)
```bash
python3 scripts/check_data_integrity.py
python3 scripts/analyze_ticks.py
```
- [ ] `check_data_integrity.py` shows < 5 critical gaps (engine running stably)
- [ ] Shadow PnL is **positive** in `[SHADOW SESSION]` logs over 5 days
- [ ] Shadow Sharpe ratio > 1.5 (compute from `ShadowFill` records)
- [ ] Cancel/Fill ratio is below 50 (engine is not spamming orders)
- [ ] At least one Stall Panic recovery was observed and handled correctly
- [ ] `HL_ADDRESS` and `HL_PRIVATE_KEY` are set and tested (fetch balance works)
- [ ] You have reviewed and understood the risk parameters in `execution.rs`
- [ ] **Verbally confirm** you are comfortable losing up to 5% of funded capital

**Then:**
```bash
MM_SHADOW_MODE=false ./scripts/start_mm_bot.sh
# → Type "CONFIRM LIVE" when prompted
```

---

## 11. Troubleshooting

### Engine crashes immediately on start

```bash
# Rebuild from scratch
cd backend/mm-engine-rs
cargo clean && cargo build --release
```

### "Redis connection refused" in logs

This is **expected and non-fatal** if the Python screener is not running. The engine uses default `MmAssetConfig` values for all coins. Logs show:
```
[SCREENER] Redis unavailable. Using default configs.
```
To silence: start Redis separately:
```bash
redis-server --port 6380 --daemonize yes
```

### All shadow fills on same coin at same timestamp

This was the "phantom fill" bug (fixed). Caused by re-registering orders every 100ms tick without checking if an order was already resting. **Fixed by the `is_registered()` guard in `QueuePositionEstimator`.**

### Engine halted — won't resume quoting

```bash
# Check logs
RUST_LOG=debug cargo run --release 2>&1 | grep "HALT\|halted"

# If drawdown stop triggered → must restart the binary
# If OFI halt → auto-recovers when market calms down
```

### Tick CSVs are growing too large

```bash
# Convert to Parquet immediately (8-10x smaller)
python3 scripts/csv_to_parquet.py --all

# Then optionally delete old CSVs (keep at least 1 day raw as buffer)
# The Parquet files are the permanent store
```

### Gap in tick data

```bash
python3 scripts/check_data_integrity.py

# If gap < 10 minutes: acceptable, analysis handles it
# If gap > 2 hours: use watchdog_mm.sh to prevent future downtime
./scripts/watchdog_mm.sh
```

### Live order not being placed

Check in order:
1. Is `MM_SHADOW_MODE=false`? (check logs: `⚠️ LIVE!`)
2. Is engine halted? (check: `[LOOP] Engine halted`)
3. Is OFI blocking that side? (check: `[OFI] Sell/Buy dominance`)
4. Is Regime Governor in halt? (check: `regime = halt`)
5. Is `HL_ADDRESS` set correctly? (check: `HL_ADDRESS must be set`)

---

## Quick Reference

```bash
# Start (dev, auto-restart)
./scripts/watchdog_mm.sh

# Start (docker, production)
./scripts/start_mm_bot.sh

# Check data health
python3 scripts/check_data_integrity.py

# Convert to Parquet
python3 scripts/csv_to_parquet.py

# Run analysis
python3 scripts/analyze_ticks.py

# Live logs
docker logs -f mm_engine

# Stop docker stack
docker compose -f docker-compose.mm.yml down

# Ad-hoc DuckDB query
duckdb -c "SELECT coin, avg(spread_bps) FROM read_parquet('data/parquet/*/*.parquet') GROUP BY coin"
```
