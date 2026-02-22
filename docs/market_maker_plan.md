# Option 1 Pivot: Market Maker Rebate Bot Architecture (v6 Final)

## Rationale
Taker fees (0.035%) mathematically destroy expected value on a micro-account. By pivoting to a Pure Liquidity Provision (Market Making) strategy, we reverse the fee structure. The bot will exclusively use Post-Only Limit orders to earn the bid-ask spread *plus* a 0.010% Maker rebate on every fill.

## 0. Structural Partitioning
The Market Maker is fundamentally different from the directional scalping bot. To avoid muddying the codebases, the Market Maker will be completely isolated:
- **Dedicated Rust Engine:** `backend/mm-engine-rs` (cloned and stripped of directional/genetic logic).
- **Dedicated Python Screener:** `scripts/mm_asset_screener.py` (No generic continuous optimizer).
- **Dedicated Docker Compose:** `docker-compose.mm.yml`.
- **Dedicated Process:** `scripts/start_mm_bot.sh`.

## 1. Tri-Layer Control Planes (Architecture)
Relying purely on a slow Python scanner is fragile. We implement three distinct control planes connected via bulletproof IPC (Redis Pub/Sub):

### A. The Slow Control Plane (Python Screener)
- Runs hourly. Publishes JSON configs (e.g., `{"asset": "BTC", "base_spread": 0.001, "max_inv": 50}`).
- **Maker Edge Scoring (Retail focus):** Screener targets assets with granular tick sizes but wide spreads.
- **Correlation & Dead-Coin Filters:** Eliminates highly correlated assets and strictly prefers perennial liquidity (>$10M volume).

### B. The Regime Governor (Data-Driven Deceleration Layer)
- **Data-Driven Rules:** Uses real-time realized volatility (ATR) or mid-price variance to switch modes. 
- **Cancel-to-Fill Ratio Management:** Exchanges shadow-ban bots with massive cancel rates. If the `Orders_Sent / Trades_Filled` sliding window spikes, the governor forces the engine to widen quotes and *leave them there* to naturally get filled and reduce API spam.
- **Calendar & Impact Pauses:** Pauses quoting surrounding major macro announcements (Fed, Token unlocks).

### C. The Fast Control Plane (Rust Engine)
- Sub-second operations. Holds ultimate kill switch.
- **Micro-Vol Guards:** Immediate halt on rapid mid-price drift. 
- **Real-Time OFI (Order Flow Imbalance):** Tracks Taker Buy vs. Taker Sell volume. If OFI flips negative heavily, instantly cancels Bids before price moves.
- **Hyperliquid Network Health (Stall Panic):** Continually monitors WS heartbeat. If WS updates stall (no new blocks), triggers a catastrophic `Cancel-All` sequence using redundant RPC endpoints.
- **State Reconciliation:** If the *local* WS connection drops and reconnects, the bot halts quoting, makes a REST API call to fetch `clearinghouseState` (live inventory) and `openOrders`, diffs it against internal state to calculate what was filled in the dark, and *only then* resumes quoting.

## 2. Multi-Level Inventory Skewing (Soft Exits & Laddering)
- **Grid Laddering:** To avoid binary adverse selection, we don't just quote 1 Top-of-Book layer. We ladder 3 layers:
  - L1 (Top of Book): Tight spread, Minimum size.
  - L2 (Mid Book): Wider spread, 2x size.
  - L3 (Deep Book): Very wide spread, 4x size.
- **Soft Exits:** "Hard Exits" (market orders) are a catastrophic last resort. If we accumulate +X Long inventory, we algorithmically skew: drastically drop the entire Bid ladder and aggressively lower the Ask ladder. We exit by becoming highly attractive to Takers, but *staying* Maker.

## 3. Global Capital-at-Risk Budgeting
Limits are absolute across the entire portfolio:
- Max concurrent assets.
- Max total inventory exposure across all books.
- **Portfolio Drawdown Cap:** Hard stop if aggregate PnL drops 5-10% of starting capital in a single daily run.

## 4. Engineering Sequencing

1. **Phase 9A: Structural Partitioning:** Clone the Rust engine to `backend/mm-engine-rs` and isolate Docker definitions.
2. **Phase 9B: L2 & L1 Network Ingestion:** Connect Rust WebSocket to `l2Book`. Monitor block slots for Hyperliquid chain stalls. 
3. **Phase 9C: Tick Data Harvesting:** Build a background Daemon that writes every `l2Book` tick for our active coins to a local CSV/TimescaleDB. (Crucial for future Backtesting).
4. **Phase 9D: Latency Auditing:** Measure the `l2Book` receipt -> logic -> Cancel round-trip API time (<50ms).
5. **Phase 9E: Protective Halts & WS Reconciliation:** Build `cancel_all`, WS State Reconciliation logic, OFI triggers, and Global Portfolio Drawdown Stops.
6. **Phase 9F: Grid Laddering & Soft Exit Accounting:** Build tracking for position sizes, grid layer orders, and algorithmic quote skewing mathematics.
7. **Phase 9G: Data-Driven Regime Logic:** ATR-triggered limit widening, Cancel-to-Fill ratio limits, and Event calendar pausing.
8. **Phase 9H: Maker Edge Screener:** Python composite scoring targeting Retail/Tick ratio + Correlation filters.
9. **Phase 9I: Advanced Shadow Mode:** Dry-Run execution with **Queue Position Estimator** (FIFO matching simulation).
10. **Phase 9J: Live Quoting:** Execute real Post-Only multi-layer quotes.
