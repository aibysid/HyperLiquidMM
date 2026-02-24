// ─────────────────────────────────────────────────────────────────────────────
// mm-engine-rs: Market Maker Engine (V6 Final Architecture)
//
// Phase status:
//   [✅] Phase 9A: Structural Partitioning
//   [✅] Phase 9B: L2 WebSocket Ingestion + Network Stall Panic
//   [✅] Phase 9C: L2 Tick Data Harvester
//   [✅] Phase 9D: Latency Auditor infrastructure
//   [✅] Phase 9E: Protective Halts & State Reconciliation
//   [✅] Phase 9F: Grid Laddering & Soft Exit Inventory Skewing
//   [✅] Phase 9G: Data-Driven Regime Governor
//   [✅] Phase 9H: Python Screener bridge (Redis watch channel)
//   [✅] Phase 9I: Advanced Shadow Mode (Queue Position Estimator)
//   [✅] Phase 9J: Live Multi-Level Quoting (post-only limit orders)
// ─────────────────────────────────────────────────────────────────────────────
mod exchange;
mod signing;
mod ingestor;
mod execution;
mod risk;
mod monitor;
mod publisher;
mod persistence;
mod market_maker;

use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::collections::{HashMap, HashSet};
use tokio::sync::Mutex as AsyncMutex;

use ingestor::{MarketDataBuffer, new_stall_panic_flag, LatencyAuditor};
use execution::{MmExecutionEngine, MmEngineConfig};
use market_maker::{
    MmAssetConfig, RegimeGovernor, QueuePositionEstimator,
    ShadowSession, ShadowFill, compute_quote_grid,
};
use publisher::{MmScreenerSubscriber, MmStatusPublisher};
use exchange::{SimExchange, LiveExchange};

#[tokio::main]
async fn main() {
    // Load .env file if present (silently ignored if missing)
    dotenvy::dotenv().ok();

    env_logger::init();
    log::info!("🏦 mm-engine-rs starting (V6 Final Architecture)…");

    // ─── Environment config ────────────────────────────────────────────────────
    let harvest_ticks = std::env::var("MM_HARVEST_TICKS")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(true);
    let shadow_mode = std::env::var("MM_SHADOW_MODE")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    let redis_url = std::env::var("REDIS_URL")
        .unwrap_or_else(|_| "redis://127.0.0.1:6380".to_string());

    log::info!("  Shadow Mode:    {}", if shadow_mode { "ON (no real orders)" } else { "⚠️  LIVE!" });
    log::info!("  Tick Harvester: {}", if harvest_ticks { "ENABLED" } else { "DISABLED" });
    log::info!("  Redis URL:      {}", redis_url);

    // ─── Phase 9J: Build exchange client ──────────────────────────────────────
    let exchange: Box<dyn exchange::ExchangeClient> = if shadow_mode {
        Box::new(SimExchange::new(5_000.0, -0.0001, 0.00035))
    } else {
        let addr = std::env::var("HL_ADDRESS")
            .expect("HL_ADDRESS must be set in live mode");
        let key = std::env::var("HL_PRIVATE_KEY")
            .expect("HL_PRIVATE_KEY must be set in live mode");
        let mut live = LiveExchange::new(addr, key);
        live.init().await.expect("LiveExchange init failed");
        Box::new(live)
    };

    // ─── Phase 9E: Execution engine ───────────────────────────────────────────
    let mm_config = MmEngineConfig { shadow_mode, ..MmEngineConfig::default() };
    let session_id  = uuid::Uuid::new_v4().to_string();
    let exec_engine = Arc::new(AsyncMutex::new(
        MmExecutionEngine::new(mm_config, exchange, session_id.clone()).await
    ));

    // ─── Phase 9B/C/D: Shared state ───────────────────────────────────────────
    let data_buffer     = Arc::new(Mutex::new(MarketDataBuffer::new()));
    let stall_panic     = new_stall_panic_flag();
    let latency_auditor = Arc::new(Mutex::new(LatencyAuditor::default()));

    // ─── Phase 9F/G: Regime Governor ──────────────────────────────────────────
    let regime_governor = Arc::new(Mutex::new(RegimeGovernor::new()));

    // ─── Phase 9I: Shadow session tracker ─────────────────────────────────────
    let shadow_session  = Arc::new(Mutex::new(ShadowSession::default()));
    let queue_estimator = Arc::new(Mutex::new(QueuePositionEstimator::new()));

    // ─── Phase 9B: Fetch initial universe ────────────────────────────────────
    log::info!("📡 Fetching Hyperliquid universe and asset contexts…");
    let coins = match ingestor::fetch_universe_and_ctx(data_buffer.clone()).await {
        Ok(c) => { log::info!("  Got {} coins.", c.len()); c }
        Err(e) => {
            log::error!("Fetch failed: {}. Fallback coins.", e);
            vec!["BTC".into(), "ETH".into(), "SOL".into(), "HYPE".into()]
        }
    };

    // ─── Phase 9H: Redis screener subscriber ──────────────────────────────────
    // Start with EMPTY config — only quote coins the screener explicitly selects.
    // Previously this seeded all 30 coins, causing unwanted trades on SOL/ETH/etc.
    let default_configs: Vec<MmAssetConfig> = Vec::new();
    let (config_tx, config_rx) = tokio::sync::watch::channel(default_configs);

    match MmScreenerSubscriber::new(&redis_url) {
        Some(sub) => {
            if let Err(e) = sub.spawn_listener(config_tx).await {
                log::warn!("[SCREENER] Redis listener failed: {}. Using defaults.", e);
            }
        }
        None => log::warn!("[SCREENER] Redis unavailable. Using default configs."),
    }
    let _status_publisher = MmStatusPublisher::new(&redis_url);

    // ─── Phase 9B: Spawn L2 ingestor ─────────────────────────────────────────
    {
        let c = coins.clone(); let b = data_buffer.clone(); let s = stall_panic.clone();
        let addr = if shadow_mode { None } else { std::env::var("HL_ADDRESS").ok() };
        tokio::spawn(async move {
            if let Err(e) = ingestor::connect_and_listen(c, b, s, harvest_ticks, addr).await {
                log::error!("Ingestor crashed: {}", e);
            }
        });
    }

    // ─── Phase 7.2: Private Fill Processor ───────────────────────────────────
    {
        let b = data_buffer.clone(); let ee = exec_engine.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                let fills: Vec<_> = {
                    let mut buf = b.lock().unwrap();
                    std::mem::take(&mut buf.user_fills).into_iter().collect()
                };
                
                if !fills.is_empty() {
                    let mut eng = ee.lock().await;
                    for fill in fills {
                        let sz: f64 = fill.sz.parse().unwrap_or(0.0);
                        let is_buy = fill.side == "B";
                        log::info!("🔔 [PRIVATE FILL] Applying trade: {} {} {} @ {}", fill.coin, fill.side, sz, fill.px);
                        eng.inventory.apply_fill(&fill.coin, is_buy, sz);
                    }
                }
            }
        });
    }

    // ─── Phase 9D: Latency reporter ───────────────────────────────────────────
    {
        let aud = latency_auditor.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                log::info!("[LATENCY] {}", aud.lock().unwrap().report());
            }
        });
    }

    // ─── Phase 9E: Stall Panic Monitor ───────────────────────────────────────
    {
        let sw = stall_panic.clone(); let ew = exec_engine.clone();
        tokio::spawn(async move {
            let mut was = false;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                let now = sw.load(Ordering::SeqCst);
                if now && !was  { log::error!("🚨 STALL PANIC: cancel_all."); ew.lock().await.cancel_all().await; }
                if was  && !now { log::info!("[STALL] WS back. Reconciling."); ew.lock().await.reconcile_after_reconnect().await; }
                was = now;
            }
        });
    }

    // ─── Phase 9E: Drawdown + C/F Monitor ─────────────────────────────────────
    {
        let ed = exec_engine.clone(); let gv = regime_governor.clone(); let ad = latency_auditor.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                let mut eng = ed.lock().await;
                eng.check_global_drawdown_stop().await;
                let cfr = eng.stats.cancel_fill_ratio();
                let p95 = ad.lock().unwrap().p95_us();
                gv.lock().unwrap().update(0.0, cfr, p95, 0.0);
                if eng.is_cancel_fill_ratio_breached() {
                    log::warn!("⚠️  C/F ratio {:.1} > limit. Widening spreads.", cfr);
                }
            }
        });
    }

    // ─── Shadow PnL reporter ──────────────────────────────────────────────────
    if shadow_mode {
        let ss = shadow_session.clone(); let sid = session_id.clone(); let url = redis_url.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                let (report, pnl) = { let s = ss.lock().unwrap(); (s.report(), s.total_pnl_usd) };
                log::info!("[SHADOW SESSION] {}", report);
                if let Some(p) = MmStatusPublisher::new(&url) {
                    let _ = p.publish_status(&sid, pnl, false, "shadow").await;
                }
            }
        });
    }

    // ─── Main quoting loop (100ms tick) ──────────────────────────────────────
    log::info!("✅ All systems active. Entering main quoting loop…");

    // Per-coin watermark: last trade timestamp we already fed into the estimator.
    let mut last_trade_ts: HashMap<String, u64> = HashMap::new();
    let mut last_active_coins: HashSet<String> = HashSet::new();

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        let is_halted     = exec_engine.lock().await.is_halted();
        if is_halted { continue; }

        let regime_mult = {
            let gov = regime_governor.lock().unwrap();
            if gov.is_halt() { continue; }
            gov.spread_multiplier()
        };

        // ── Phase 9H: Latest screener configs ─────────────────────────────────
        let asset_configs: HashMap<String, MmAssetConfig> = config_rx.borrow()
            .iter().map(|c| (c.asset.clone(), c.clone())).collect();
        
        let current_active_coins: HashSet<String> = asset_configs.keys().cloned().collect();
        
        // ── Phase 7: Instant Flush on Whitelist Change ────────────────────────
        if !shadow_mode && current_active_coins != last_active_coins {
            let removed: Vec<_> = last_active_coins.difference(&current_active_coins).collect();
            if !removed.is_empty() {
                log::warn!("[MAIN] Whitelist changed. Assets removed: {:?}. Triggering instant flush.", removed);
                let mut eng = exec_engine.lock().await;
                eng.flush_orphaned_positions(&current_active_coins).await;
            }
            last_active_coins = current_active_coins.clone();
        }

        // ── Phase 9B: Snapshot L2 data ────────────────────────────────────────
        let (l2_snap, trade_snap) = {
            let buf = data_buffer.lock().unwrap();
            (buf.l2_books.clone(), buf.trade_buffers.clone())
        };

        // ── Phase 9L: Global State Reconciliation & Flush (every 30s) ──────────────
        if !shadow_mode {
            static LAST_RECONCILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let now_ms = chrono::Utc::now().timestamp_millis() as u64;
            let last_rec = LAST_RECONCILE.load(Ordering::Relaxed);
            if now_ms - last_rec > 120_000 {
                LAST_RECONCILE.store(now_ms, Ordering::Relaxed);
                log::info!("[MAIN] Triggering 120s global reconciliation and flush check...");
                let mut eng = exec_engine.lock().await;
                match eng.exchange.get_positions().await {
                    Ok(positions) => {
                        log::info!("[MAIN] Fetched {} positions from exchange.", positions.len());
                        let _ = eng.inventory.reconcile(&positions);
                        // Flush Orphaned Positions (Phase 9M)
                        let active_assets: HashSet<String> = asset_configs.keys().cloned().collect();
                        log::info!("[MAIN] Active screener assets: {:?}", active_assets);
                        eng.flush_orphaned_positions(&active_assets).await;
                    }
                    Err(e) => log::warn!("[INVENTORY] Position fetch failed: {}", e),
                }
            }
        }

        // ── Per-asset loop ─────────────────────────────────────────────────────
        // Phase 9O Optimization: Only iterate over whitelisted assets to reduce loop latency.
        for (coin, config) in &asset_configs {
            let snap = match l2_snap.get(coin) {
                Some(s) => s,
                None => continue,
            };
            if config.regime == "halt" { continue; }
            
            let mid = match snap.mid_price() {
                Some(m) if m > 0.0 => m,
                _ => continue,
            };

            // OFI: feed most recent trade
            if let Some(trades) = trade_snap.get(coin) {
                if let Some(t) = trades.back() {
                    let is_buy = t.side == "B";
                    let px: f64 = t.px.parse().unwrap_or(0.0);
                    let sz: f64 = t.sz.parse().unwrap_or(0.0);
                    exec_engine.lock().await.record_taker_trade(coin, is_buy, px, sz);
                }
            }

            // Regime update
            let cfr = exec_engine.lock().await.stats.cancel_fill_ratio();
            let p95 = latency_auditor.lock().unwrap().p95_us();
            regime_governor.lock().unwrap().update(config.atr_fraction, cfr, p95, 0.0);

            // Grid
            let inv_usd = exec_engine.lock().await
                .inventory.positions.get(coin).cloned().unwrap_or(0.0) * mid;
            
            let ofi_bid_block = exec_engine.lock().await.ofi_bids_blocked(coin);
            let ofi_ask_block = exec_engine.lock().await.ofi_asks_blocked(coin);

            let grid = compute_quote_grid(mid, &config, inv_usd, regime_mult, ofi_bid_block, ofi_ask_block);
            if grid.is_empty() { continue; }

            // ── Shadow Mode (Phase 9I) ─────────────────────────────────────────
            if shadow_mode {
                let now_ms = chrono::Utc::now().timestamp_millis() as u64;

                // Idempotent registration: only register if not already resting
                {
                    let mut est = queue_estimator.lock().unwrap();
                    for bid in &grid.bids {
                        let key = format!("{}_bid_L{}", coin, bid.layer);
                        if !est.is_registered(&key) {
                            est.register_order(&key, bid.price, true, bid.size_usd, now_ms);
                        }
                    }
                    for ask in &grid.asks {
                        let key = format!("{}_ask_L{}", coin, ask.layer);
                        if !est.is_registered(&key) {
                            est.register_order(&key, ask.price, false, ask.size_usd, now_ms);
                        }
                    }
                }

                // Feed only NEW trades (watermark guard)
                let prev_ts = last_trade_ts.get(coin).cloned().unwrap_or(0);
                if let Some(trades) = trade_snap.get(coin) {
                    let new_trades: Vec<_> = trades.iter().filter(|t| t.time > prev_ts).collect();
                    if !new_trades.is_empty() {
                        let max_ts = new_trades.iter().map(|t| t.time).max().unwrap_or(prev_ts);
                        last_trade_ts.insert(coin.clone(), max_ts);
                        let mut est = queue_estimator.lock().unwrap();
                        for t in &new_trades {
                            let is_buy = t.side == "B";
                            let px: f64 = t.px.parse().unwrap_or(0.0);
                            let sz: f64 = t.sz.parse().unwrap_or(0.0);
                            let vol_usd = px * sz;
                            for layer in 1u8..=3 {
                                est.on_trade(&format!("{}_bid_L{}", coin, layer), px, is_buy, vol_usd);
                                est.on_trade(&format!("{}_ask_L{}", coin, layer), px, is_buy, vol_usd);
                            }
                        }
                    }
                }

                // Check fills
                for bid in &grid.bids {
                    let key = format!("{}_bid_L{}", coin, bid.layer);
                    if queue_estimator.lock().unwrap().is_likely_filled(&key, 0.70) {
                        shadow_session.lock().unwrap().record_fill(ShadowFill::new(coin, bid, now_ms));
                        queue_estimator.lock().unwrap().remove(&key);
                        exec_engine.lock().await.inventory.apply_fill(coin, true, bid.size_usd / mid);
                    }
                }
                for ask in &grid.asks {
                    let key = format!("{}_ask_L{}", coin, ask.layer);
                    if queue_estimator.lock().unwrap().is_likely_filled(&key, 0.70) {
                        shadow_session.lock().unwrap().record_fill(ShadowFill::new(coin, ask, now_ms));
                        queue_estimator.lock().unwrap().remove(&key);
                        exec_engine.lock().await.inventory.apply_fill(coin, false, ask.size_usd / mid);
                    }
                }

            } else {
                // ── Phase 9J: LIVE order placement ──────────────────────────────

                // Rate-limit: only refresh orders every 30,000ms (30s) per coin
                // This resolves the "Too many cumulative requests" deadlock for small accounts.
                let now_ms = chrono::Utc::now().timestamp_millis() as u64;
                let last_refresh = last_trade_ts.get(coin).cloned().unwrap_or(0);
                if now_ms - last_refresh < 30_000 {
                    continue;
                }
                last_trade_ts.insert(coin.clone(), now_ms);

                let mut eng = exec_engine.lock().await;

                // Step 1: Check inventory — skip the side that increases risk if at max
                let inv_coins = eng.inventory.positions.get(coin).cloned().unwrap_or(0.0);
                let inv_usd_abs = (inv_coins * mid).abs();
                
                let mut suppress_bids = false;
                let mut suppress_asks = false;

                if inv_usd_abs > config.max_inv_usd {
                    if inv_coins > 0.0 {
                        // We are LONG and over limit: Stop buying (suppress bids), but allow selling.
                        suppress_bids = true;
                        log::warn!(
                            "[LIVE] {} inventory ${:.2} > max ${:.2} (LONG) — Suppressing BIDS to reduce risk.",
                            coin, inv_usd_abs, config.max_inv_usd
                        );
                    } else if inv_coins < 0.0 {
                        // We are SHORT and over limit: Stop shorting (suppress asks), but allow buying.
                        suppress_asks = true;
                        log::warn!(
                            "[LIVE] {} inventory ${:.2} > max ${:.2} (SHORT) — Suppressing ASKS to reduce risk.",
                            coin, inv_usd_abs, config.max_inv_usd
                        );
                    }
                }

                // Step 2 & 3: Sticky Logic (Phase 9O)
                let all_quotes: Vec<_> = grid.bids.iter().chain(grid.asks.iter())
                    .filter(|q| {
                        if q.side == "bid" && suppress_bids { return false; }
                        if q.side == "ask" && suppress_asks { return false; }
                        true
                    })
                    .collect();

                let open_orders = match eng.exchange.get_open_orders(coin).await {
                    Ok(orders) => orders,
                    Err(_) => Vec::new(),
                };

                // Increased sticky threshold to 50% of spread to further reduce churn
                let sticky_threshold = mid * (config.base_spread_bps / 10_000.0) * 0.5;
                let mut all_sticky = !all_quotes.is_empty() && !open_orders.is_empty();

                for quote in &all_quotes {
                    let side_wire = if quote.side == "bid" { "B" } else { "A" };
                    let existing = open_orders.iter().find(|o| {
                        let o_side = o["side"].as_str().unwrap_or("");
                        let o_px = o["limitPx"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
                        let matched = o_side == side_wire && (o_px - quote.price).abs() < sticky_threshold;
                        if !matched && o_side == side_wire {
                            log::trace!("[STICKY-DEBUG] Side match but Price Diff: {:.6} > {:.6}", (o_px - quote.price).abs(), sticky_threshold);
                        }
                        matched
                    });

                    if existing.is_none() {
                        all_sticky = false;
                        break;
                    }
                }

                if all_sticky && open_orders.len() == all_quotes.len() {
                    // All target quotes are already represented by resting orders. Skip reset (Preserves Queue).
                    log::info!("[STICKY] {} orders are within 20% of target. Skipping reset. threshold={:.6}", coin, sticky_threshold);
                    continue;
                } else if !all_quotes.is_empty() {
                    log::trace!("[STICKY-DEBUG] {} not sticky: q_len={} o_len={} all_s={}", coin, all_quotes.len(), open_orders.len(), all_sticky);
                }

                // Step 3: Cancel ONLY this coin's orders (not all coins!)
                let _ = eng.exchange.cancel_coin_orders(coin).await;

                // Step 4: Atomic Margin Check (Phase 9L)
                let total_grid_notional: f64 = grid.bids.iter().map(|b| b.size_usd).sum::<f64>() 
                                             + grid.asks.iter().map(|a| a.size_usd).sum::<f64>();
                
                if !eng.has_sufficient_margin(total_grid_notional).await {
                    log::warn!("[MARGIN GUARD] Skipping {} grid due to insufficient account equity.", coin);
                    continue;
                }

                // Step 5: Place grid quotes (bids + asks)
                let mut placed = 0u32;
                let mut errors = 0u32;

                for quote in &all_quotes {
                    let side_is_bid = quote.side == "bid";
                    if side_is_bid && suppress_bids { continue; }
                    if !side_is_bid && suppress_asks { continue; }

                    let direction = if side_is_bid { "LONG" } else { "SHORT" };
                    let size_coins = quote.size_usd / mid;  // Convert USD notional → coin units

                    match eng.exchange.open_order(
                        coin,
                        direction,
                        size_coins,
                        quote.price,
                        1.0,   // leverage (cross margin, managed by position sizing)
                        0.0,   // no TP
                        0.0,   // no SL
                        true,  // post_only = ALO (maker only)
                    ).await {
                        Ok(_action) => {
                            placed += 1;
                            log::info!(
                                "[LIVE] {} {} L{} @ {:.6} sz={:.4} coins (${:.2})",
                                coin, quote.side, quote.layer, quote.price, size_coins, quote.size_usd
                            );
                        }
                        Err(e) => {
                            errors += 1;
                            log::warn!(
                                "[LIVE] {} {} L{} order failed: {}",
                                coin, quote.side, quote.layer, e
                            );
                        }
                    }
                }

                if placed > 0 || errors > 0 {
                    log::info!(
                        "[LIVE QUOTE] {} mid={:.4} inv=${:.2} placed={}/{} errors={} regime={:.2}x",
                        coin, mid, inv_usd_abs, placed, all_quotes.len(), errors, regime_mult
                    );
                }
            }
        }
    }
}
