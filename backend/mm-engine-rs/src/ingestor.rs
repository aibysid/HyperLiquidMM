// ─────────────────────────────────────────────────────────────────────────────
// ingestor.rs — Market Maker L2 Book Ingestor + Network Stall Panic
//
// Phase 9B: Subscribe to Hyperliquid's `l2Book` WebSocket channel.
// Phase 9C: Tick Data Harvester — writes every L2 snapshot to CSV.
// Phase 9D: Latency Auditor — measures receipt → logic → cancel round-trip time.
// ─────────────────────────────────────────────────────────────────────────────
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{StreamExt, SinkExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use log::{info, warn, error};
use url::Url;
use chrono::Utc;

const WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
const API_URL: &str = "https://api.hyperliquid.xyz/info";

/// If no WS message is received for this many seconds, declare a chain stall.
const STALL_TIMEOUT_SECS: u64 = 30;

// ─── L2 Book Data Structures ─────────────────────────────────────────────────

/// A single price level in the L2 order book.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2Level {
    /// Price as string (Hyperliquid sends prices as strings).
    pub px: String,
    /// Size at that level.
    pub sz: String,
    /// Number of orders at this level.
    pub n: u64,
}

/// A full snapshot of the L2 order book for one asset, as received from the WS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2BookSnapshot {
    pub coin: String,
    /// Best bids, sorted descending by price.
    pub bids: Vec<L2Level>,
    /// Best asks, sorted ascending by price.
    pub asks: Vec<L2Level>,
    /// Timestamp when snapshot was received by our engine (local epoch ms).
    pub received_at_ms: u64,
}

impl L2BookSnapshot {
    /// Returns the mid-price: arithmetic mean of best bid and best ask.
    pub fn mid_price(&self) -> Option<f64> {
        let best_bid = self.bids.first()?.px.parse::<f64>().ok()?;
        let best_ask = self.asks.first()?.px.parse::<f64>().ok()?;
        Some((best_bid + best_ask) / 2.0)
    }

    /// Returns the bid-ask spread in absolute price units.
    pub fn spread(&self) -> Option<f64> {
        let best_bid = self.bids.first()?.px.parse::<f64>().ok()?;
        let best_ask = self.asks.first()?.px.parse::<f64>().ok()?;
        Some(best_ask - best_bid)
    }

    /// Returns the spread as a fraction of the mid-price.
    pub fn spread_bps(&self) -> Option<f64> {
        let spread = self.spread()?;
        let mid = self.mid_price()?;
        if mid <= 0.0 { return None; }
        Some((spread / mid) * 10_000.0) // in basis points
    }
}

// ─── Trade Data (from `trades` channel — used for OFI calculation) ────────────

/// A single taker trade, used to compute Order Flow Imbalance (OFI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub coin: String,
    pub side: String, // "B" = taker buy, "A" = taker sell/ask
    pub px: String,
    pub sz: String,
    pub time: u64,
}

/// A private trade fill event for the authenticated user.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserFill {
    pub coin: String,
    pub px: String,
    pub sz: String,
    pub side: String,
    pub dir: String,
    pub closed_pnl: String,
    pub hash: String,
    pub tid: u64,
}

// ─── Funding / Market Context ─────────────────────────────────────────────────

/// Dynamic market data for secondary use (e.g., continuous funding bias).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketContext {
    pub funding_rate: f64,
    pub open_interest: f64,
    pub oracle_px: f64,
    pub day_ntl_vlm: f64,
    pub last_update: u64,
}

// ─── Shared Market Data Buffer ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MarketDataBuffer {
    /// Map of Coin → Latest L2 Book Snapshot (the primary MM data source).
    pub l2_books: HashMap<String, L2BookSnapshot>,
    /// Map of Coin → Recent taker trades (for OFI).
    pub trade_buffers: HashMap<String, VecDeque<Trade>>,
    /// Map of Coin → Funding/OI context.
    pub contexts: HashMap<String, MarketContext>,
    /// Recent private fills for the user.
    pub user_fills: VecDeque<UserFill>,
    /// Map of Coin -> Rolling Price History (timestamp, mid_price) for Volatility tracking.
    pub price_histories: HashMap<String, VecDeque<(u64, f64)>>,
    /// Last time any WS message was received (epoch ms). Used for stall detection.
    pub last_ws_message_ms: u64,
}

impl MarketDataBuffer {
    pub fn new() -> Self {
        Self {
            l2_books: HashMap::new(),
            trade_buffers: HashMap::new(),
            contexts: HashMap::new(),
            user_fills: VecDeque::new(),
            price_histories: HashMap::new(),
            last_ws_message_ms: now_ms(),
        }
    }

    /// Returns true if no WS message received for longer than STALL_TIMEOUT_SECS.
    pub fn is_stalled(&self) -> bool {
        now_ms().saturating_sub(self.last_ws_message_ms) > STALL_TIMEOUT_SECS * 1_000
    }

    pub fn touch(&mut self) {
        self.last_ws_message_ms = now_ms();
    }

    pub fn update_l2(&mut self, snap: L2BookSnapshot) {
        self.touch();
        if let Some(mid) = snap.mid_price() {
            let history = self.price_histories.entry(snap.coin.clone()).or_insert_with(|| VecDeque::with_capacity(600));
            let now = now_ms();
            history.push_back((now, mid));
            
            // Keep 5 minutes of data (300,000 ms)
            while history.len() > 1 && (now - history.front().unwrap().0) > 300_000 {
                history.pop_front();
            }
        }
        self.l2_books.insert(snap.coin.clone(), snap);
    }

    /// Calculates the real-time volatility in basis points (standard deviation of the last 5 mins).
    pub fn realtime_vol_bps(&self, coin: &str) -> f64 {
        let history = match self.price_histories.get(coin) {
            Some(h) if h.len() > 10 => h, // Need at least 10 samples for a meaningful stddev
            _ => return 0.0,
        };

        let mids: Vec<f64> = history.iter().map(|(_, p)| *p).collect();
        let count = mids.len() as f64;
        let mean = mids.iter().sum::<f64>() / count;
        
        let variance = mids.iter().map(|p| {
            let diff = p - mean;
            diff * diff
        }).sum::<f64>() / count;

        let std_dev = variance.sqrt();
        (std_dev / mean) * 10_000.0 // Convert to BPS
    }

    pub fn add_trade(&mut self, trade: Trade) {
        self.touch();
        let buffer = self.trade_buffers.entry(trade.coin.clone()).or_insert_with(VecDeque::new);
        if buffer.len() >= 1_000 {
            buffer.pop_front();
        }
        buffer.push_back(trade);
    }

    pub fn update_context(&mut self, coin: String, ctx: MarketContext) {
        self.contexts.insert(coin, ctx);
    }

    pub fn add_user_fill(&mut self, fill: UserFill) {
        self.touch();
        if self.user_fills.len() >= 500 {
            self.user_fills.pop_front();
        }
        self.user_fills.push_back(fill);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── Stall Panic Flag ─────────────────────────────────────────────────────────

/// Atomically shared flag. When the stall watcher sets this true,
/// the execution engine must trigger cancel_all immediately.
pub type StallPanicFlag = Arc<AtomicBool>;

pub fn new_stall_panic_flag() -> StallPanicFlag {
    Arc::new(AtomicBool::new(false))
}

// ─── Latency Auditor ─────────────────────────────────────────────────────────

/// Records round-trip latency: L2 receipt → processing → simulated-cancel.
/// Phase 9D: If P95 > 50ms, the Regime Governor must widen spreads.
#[derive(Debug, Default)]
pub struct LatencyAuditor {
    samples: VecDeque<u64>, // latency in microseconds
}

impl LatencyAuditor {
    pub fn record(&mut self, received_at_us: u64, processed_at_us: u64) {
        let delta = processed_at_us.saturating_sub(received_at_us);
        if self.samples.len() >= 10_000 {
            self.samples.pop_front();
        }
        self.samples.push_back(delta);
    }

    /// Returns P95 latency in microseconds.
    pub fn p95_us(&self) -> u64 {
        if self.samples.is_empty() { return 0; }
        let mut sorted: Vec<u64> = self.samples.iter().cloned().collect();
        sorted.sort_unstable();
        sorted[(sorted.len() as f64 * 0.95) as usize]
    }

    /// Returns true if P95 latency exceeds 50ms — trigger spread widening.
    pub fn is_too_slow(&self) -> bool {
        self.p95_us() > 50_000 // 50ms in microseconds
    }

    pub fn report(&self) -> String {
        if self.samples.is_empty() { return "No samples yet".to_string(); }
        let avg = self.samples.iter().sum::<u64>() / self.samples.len() as u64;
        format!("Latency: avg={}µs, p95={}µs, too_slow={}", avg, self.p95_us(), self.is_too_slow())
    }
}

// ─── Tick Data Harvester (Phase 9C) ───────────────────────────────────────────

/// PHASE 9C: Harvests every L2 tick to a CSV file for future backtesting.
/// Appends: timestamp_ms, coin, best_bid_px, best_ask_px, mid_price, spread_bps
pub fn harvest_tick_to_csv(snap: &L2BookSnapshot) {
    let mid = snap.mid_price().unwrap_or(0.0);
    let spread = snap.spread_bps().unwrap_or(0.0);
    let best_bid = snap.bids.first().map(|l| l.px.as_str()).unwrap_or("0");
    let best_ask = snap.asks.first().map(|l| l.px.as_str()).unwrap_or("0");

    // Append to a daily CSV file: data/ticks/COIN/YYYY-MM-DD.csv
    let date = Utc::now().format("%Y-%m-%d").to_string();
    let dir = format!("data/ticks/{}", snap.coin);
    let path = format!("{}/{}.csv", dir, date);

    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write;
            let _ = writeln!(
                file,
                "{},{},{},{},{:.6},{:.4}",
                snap.received_at_ms, snap.coin, best_bid, best_ask, mid, spread
            );
        }
    }
}

// ─── Fetch Universe & Market Context ──────────────────────────────────────────

pub async fn fetch_universe_and_ctx(
    buffer: Arc<Mutex<MarketDataBuffer>>,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let resp = client
        .post(API_URL)
        .json(&serde_json::json!({"type": "metaAndAssetCtxs"}))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    let mut all_coins = Vec::new();
    let mut coins = Vec::new();

    if let Some(arr) = resp.as_array() {
        if arr.len() >= 2 {
            if let Some(universe) = arr[0]["universe"].as_array() {
                for asset in universe {
                    if let Some(name) = asset["name"].as_str() {
                        all_coins.push(name.to_string());
                    }
                }
            }
            if let Some(ctxs) = arr[1].as_array() {
                let mut buf = buffer.lock().unwrap();
                let ts = now_ms();
                let mut coin_volumes: Vec<(String, f64)> = Vec::new();

                for (i, ctx_val) in ctxs.iter().enumerate() {
                    if i >= all_coins.len() { break; }
                    let coin = &all_coins[i];

                    let vol: f64 = ctx_val["dayNtlVlm"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                    coin_volumes.push((coin.clone(), vol));

                    let ctx = MarketContext {
                        funding_rate: ctx_val["funding"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                        open_interest: ctx_val["openInterest"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                        oracle_px: ctx_val["oraclePx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0),
                        day_ntl_vlm: vol,
                        last_update: ts,
                    };
                    buf.update_context(coin.clone(), ctx);
                }

                // Sort by volume and take top 100 liquid assets (Phase 7.1 broadening)
                coin_volumes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                coins = coin_volumes.into_iter().take(100).map(|(c, _)| c).collect();
                info!("Ingestor: Top {} assets by volume selected.", coins.len());
            }
        }
    }
    Ok(coins)
}

// ─── Main WS Connection Loop ───────────────────────────────────────────────────

/// PHASE 9B: Connects to the Hyperliquid WS and subscribes to both:
///   - `l2Book` (primary data for MM quoting)
///   - `trades` (secondary data for OFI calculation)
///
/// Also spawns:
///   - A stall watcher task that fires `stall_panic` if no message for 30s.
///   - A latency auditor exposed via `LatencyAuditor`.
///
/// On WS reconnect, the caller is responsible for calling `execution.reconcile_state()`.
pub async fn connect_and_listen(
    coins: Vec<String>,
    buffer: Arc<Mutex<MarketDataBuffer>>,
    stall_panic: StallPanicFlag,
    harvest_ticks: bool, // Phase 9C toggle
    user_address: Option<String>, // Phase 7.2: Real-time user fills
) -> Result<(), Box<dyn std::error::Error>> {

    // Refresh market context every 60 seconds in the background.
    let buf_ctx = buffer.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            if let Err(e) = fetch_universe_and_ctx(buf_ctx.clone()).await {
                error!("Failed to refresh MarketContext: {}", e);
            }
        }
    });

    // ─── Stall Watcher Task ───────────────────────────────────────────────────
    // Polls every 5 seconds to check if the last WS message is too old.
    // If stalled, sets the `stall_panic` AtomicBool which the execution engine
    // watches to trigger cancel_all and reconnect.
    let buf_stall = buffer.clone();
    let stall_flag = stall_panic.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let stalled = {
                let buf = buf_stall.lock().unwrap();
                buf.is_stalled()
            };
            if stalled && !stall_flag.load(Ordering::SeqCst) {
                error!(
                    "🚨 NETWORK STALL DETECTED: No WS message for >{}s. Triggering cancel_all panic!",
                    STALL_TIMEOUT_SECS
                );
                stall_flag.store(true, Ordering::SeqCst);
            } else if !stalled && stall_flag.load(Ordering::SeqCst) {
                // WS recovered — the execution engine will clear the flag after reconciling.
                info!("[STALL WATCHER] WS appears recovered. Awaiting execution reconciliation.");
            }
        }
    });

    // ─── Reconnection Loop with Exponential Backoff ───────────────────────────
    let mut retry_delay_secs: u64 = 1;
    let max_delay_secs: u64 = 32;

    loop {
        info!("Connecting to Hyperliquid WS: {}", WS_URL);
        match connect_async(Url::parse(WS_URL)?).await {
            Ok((ws_stream, _)) => {
                info!("✅ WS Connected.");
                retry_delay_secs = 1;

                // Clear the stall panic flag on reconnect — execution will reconcile state.
                stall_panic.store(false, Ordering::SeqCst);

                let (mut write, mut read) = ws_stream.split();

                // Subscribe to l2Book and trades for each coin.
                // We batch subscriptions to avoid flooding the WS with 60 messages at once.
                for chunk in coins.chunks(20) {
                    for coin in chunk {
                        // l2Book: the primary feed for Market Making
                        let l2_sub = serde_json::json!({
                            "method": "subscribe",
                            "subscription": { "type": "l2Book", "coin": coin }
                        });
                        if let Err(e) = write.send(Message::Text(l2_sub.to_string())).await {
                            error!("Failed to subscribe to l2Book for {}: {}", coin, e);
                        }

                        // trades: secondary feed for OFI signal
                        let trade_sub = serde_json::json!({
                            "method": "subscribe",
                            "subscription": { "type": "trades", "coin": coin }
                        });
                        if let Err(e) = write.send(Message::Text(trade_sub.to_string())).await {
                            error!("Failed to subscribe to trades for {}: {}", coin, e);
                        }
                    }
                    info!("  Subscribed to {} coins (l2Book + trades).", chunk.len());
                    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
                }

                // Phase 7.2: Subscribe to private user fills if address provided
                if let Some(ref addr) = user_address {
                    let fill_sub = serde_json::json!({
                        "method": "subscribe",
                        "subscription": { "type": "userFills", "user": addr }
                    });
                    if let Err(e) = write.send(Message::Text(fill_sub.to_string())).await {
                        error!("Failed to subscribe to userFills for {}: {}", addr, e);
                    } else {
                        info!("✅ Subscribed to userFills for {}.", addr);
                    }
                }

                // ─── Message Loop ─────────────────────────────────────────────
                while let Some(msg) = read.next().await {
                    // Phase 9D: Record receipt timestamp
                    let received_at_us = now_ms() * 1_000;

                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                                let channel = parsed["channel"].as_str().unwrap_or("");

                                match channel {
                                    // ── L2 Book Update ──────────────────────────────
                                    "l2Book" => {
                                        if let Some(data) = parsed.get("data") {
                                            let coin = data["coin"].as_str().unwrap_or("").to_string();
                                            let received_at_ms = now_ms();

                                            let parse_levels = |key: &str| -> Vec<L2Level> {
                                                data["levels"][key].as_array()
                                                    .unwrap_or(&vec![])
                                                    .iter()
                                                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                                                    .collect()
                                            };

                                            let (bids, asks) = if let Some(levels_arr) = data["levels"].as_array() {
                                                let bids: Vec<L2Level> = levels_arr.get(0)
                                                    .and_then(|v| v.as_array())
                                                    .map(|arr| arr.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect())
                                                    .unwrap_or_default();
                                                let asks: Vec<L2Level> = levels_arr.get(1)
                                                    .and_then(|v| v.as_array())
                                                    .map(|arr| arr.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect())
                                                    .unwrap_or_default();
                                                (bids, asks)
                                            } else {
                                                (parse_levels("bids"), parse_levels("asks"))
                                            };

                                            if !coin.is_empty() {
                                                let snap = L2BookSnapshot {
                                                    coin: coin.clone(),
                                                    bids,
                                                    asks,
                                                    received_at_ms,
                                                };
                                                if harvest_ticks { harvest_tick_to_csv(&snap); }
                                                buffer.lock().unwrap().update_l2(snap);
                                            }
                                        }
                                    }

                                    // ── Private User Fills ───────────────────────
                                    "userFills" => {
                                        if let Some(data) = parsed.get("data") {
                                            // Hyperliquid sends 'isSnapshot' (camelCase)
                                            let is_snap = data["isSnapshot"].as_bool().unwrap_or(false);
                                            if !is_snap {
                                                if let Some(fills) = data["fills"].as_array() {
                                                    let mut buf = buffer.lock().unwrap();
                                                    for f_val in fills {
                                                        if let Ok(fill) = serde_json::from_value::<UserFill>(f_val.clone()) {
                                                            info!("🔔 [PRIVATE FILL] {} {} {} @ {}", fill.coin, fill.side, fill.sz, fill.px);
                                                            buf.add_user_fill(fill);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // ── Trade (OFI) Update ─────────────────────────
                                    "trades" => {
                                        if let Some(data_arr) = parsed["data"].as_array() {
                                            let mut buf = buffer.lock().unwrap();
                                            for t in data_arr {
                                                if let Ok(trade) = serde_json::from_value::<Trade>(t.clone()) {
                                                    buf.add_trade(trade);
                                                }
                                            }
                                        }
                                    }

                                    // ── WS Ping (Hyperliquid sends periodic pings) ─
                                    "ping" | "pong" => {
                                        // Just update the heartbeat to prevent stall detection
                                        buffer.lock().unwrap().touch();
                                    }

                                    _ => {}
                                }
                            }
                        }
                        Ok(Message::Ping(data)) => {
                            let _ = write.send(Message::Pong(data)).await;
                            buffer.lock().unwrap().touch();
                        }
                        Ok(Message::Close(_)) => {
                            warn!("WS connection closed by server. Reconnecting...");
                            break;
                        }
                        Err(e) => {
                            error!("WS error: {}. Reconnecting...", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!("Failed to connect to WS: {}. Retrying in {}s...", e, retry_delay_secs);
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(retry_delay_secs)).await;
        retry_delay_secs = std::cmp::min(retry_delay_secs * 2, max_delay_secs);
    }
}
