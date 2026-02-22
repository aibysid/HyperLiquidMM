use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

// ─── Shared Models ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub coin: String,
    pub direction: String,        
    pub size: f64,                
    pub entry_price: f64,
    pub margin_used: f64,         
    pub leverage: f64,
    pub tp_price: f64,            
    pub sl_price: f64,            
    pub liquidation_price: f64,   
    pub entry_time: u64,          
    pub unrealized_pnl: f64,
}

impl Position {
    pub fn calc_pnl(&self, current_price: f64) -> f64 {
        let price_diff = match self.direction.as_str() {
            "LONG" => current_price - self.entry_price,
            "SHORT" => self.entry_price - current_price,
            _ => 0.0,
        };
        price_diff * self.size
    }

    pub fn is_tp_hit(&self, current_price: f64) -> bool {
        match self.direction.as_str() {
            "LONG" => current_price >= self.tp_price,
            "SHORT" => current_price <= self.tp_price,
            _ => false,
        }
    }

    pub fn is_sl_hit(&self, current_price: f64) -> bool {
        match self.direction.as_str() {
            "LONG" => current_price <= self.sl_price,
            "SHORT" => current_price >= self.sl_price,
            _ => false,
        }
    }

    pub fn is_time_stop_hit(&self, current_time: u64, max_secs: u64) -> bool {
        if current_time > self.entry_time {
            let elapsed_secs = (current_time - self.entry_time) / 1000;
            return elapsed_secs >= max_secs;
        }
        false
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeAction {
    #[serde(default)] // Allow missing for old logs
    pub session_id: String,
    pub coin: String,
    pub action: String,       
    pub direction: String,    
    pub price: f64,
    pub size: f64,
    pub reason: String,
    pub pnl: Option<f64>,
    pub fee: Option<f64>,
    pub exit_type: Option<String>, 
    pub ts: u64,
    
    // Analysis Fields
    pub entry_rsi: Option<f64>,
    pub entry_bb_dist: Option<f64>, 
    pub entry_score: Option<f64>,
}

#[derive(Debug, Clone)]
pub enum OrderError {
    InsufficientFunds(String),
    MaxPositionsReached,
    InvalidOrder(String),
    NetworkError(String),
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderError::InsufficientFunds(s) => write!(f, "Insufficient Funds: {}", s),
            OrderError::MaxPositionsReached => write!(f, "Max Positions Reached"),
            OrderError::InvalidOrder(s) => write!(f, "Invalid Order: {}", s),
            OrderError::NetworkError(s) => write!(f, "Network Error: {}", s),
        }
    }
}

// ─── Exchange Trait ────────────────────────────────────────────────

#[async_trait]
pub trait ExchangeClient: Send + Sync {
    async fn get_balance(&self) -> Result<f64, OrderError>;
    async fn get_positions(&self) -> Result<Vec<Position>, OrderError>;
    async fn get_open_orders(&self, coin: &str) -> Result<Vec<serde_json::Value>, OrderError>;
    async fn open_order(&mut self, coin: &str, direction: &str, size: f64, price: f64, leverage: f64, tp: f64, sl: f64, post_only: bool) -> Result<TradeAction, OrderError>;
    async fn close_position(&mut self, coin: &str, price: f64, reason: &str, ts: u64) -> Result<TradeAction, OrderError>;
    async fn withdraw(&mut self, amount: f64) -> Result<f64, OrderError>;
    async fn sweep_dead_orders(&mut self) -> Result<(), OrderError>;

    /// Cancels a single order by (asset_idx, order_id).
    /// Phase 9E: Building block for cancel_all_orders.
    async fn cancel_order(&mut self, asset_idx: u32, oid: u64) -> Result<(), OrderError>;

    /// Phase 9E: Protective Halt — cancels EVERY open order across ALL coins.
    /// Called on: WS reconnect (before reconcile), OFI spike, global drawdown stop, chain stall.
    async fn cancel_all_orders(&mut self) -> Result<u64, OrderError>;

    // For simulation/backtesting only
    fn as_sim_mut(&mut self) -> Option<&mut SimExchange> { None }
}


// ─── Sim Exchange (In-Memory) ──────────────────────────────────────

pub struct SimExchange {
    pub balance: f64,
    pub positions: HashMap<String, Position>,
    pub maker_fee: f64,
    pub taker_fee: f64,
    pub trade_count: u64,
    pub total_fees: f64,
}

impl SimExchange {
    pub fn new(initial_balance: f64, maker_fee: f64, taker_fee: f64) -> Self {
        Self {
            balance: initial_balance,
            positions: HashMap::new(),
            maker_fee,
            taker_fee,
            trade_count: 0,
            total_fees: 0.0,
        }
    }
    
    pub fn restore(&mut self, balance: f64, positions: Vec<Position>) {
        self.balance = balance;
        self.positions.clear();
        for p in positions {
            self.positions.insert(p.coin.clone(), p);
        }
    }
}

#[async_trait]
impl ExchangeClient for SimExchange {
    async fn get_balance(&self) -> Result<f64, OrderError> {
        Ok(self.balance)
    }

    async fn get_positions(&self) -> Result<Vec<Position>, OrderError> {
        Ok(self.positions.values().cloned().collect())
    }

    async fn get_open_orders(&self, _coin: &str) -> Result<Vec<serde_json::Value>, OrderError> {
        Ok(Vec::new()) // Sim doesn't really have resting orders, they execute immediately
    }

    async fn open_order(&mut self, coin: &str, direction: &str, size: f64, price: f64, leverage: f64, tp: f64, sl: f64, post_only: bool) -> Result<TradeAction, OrderError> {
        if self.positions.contains_key(coin) {
             return Err(OrderError::InvalidOrder(format!("Already have position in {}", coin)));
        }

        let notional = size * price;
        let margin = notional / leverage;
        let fee = if post_only { notional * self.maker_fee } else { notional * self.taker_fee };

        if self.balance < (margin + fee) {
            return Err(OrderError::InsufficientFunds(format!("Balance {:.2} < Margin {:.2} + Fee {:.2}", self.balance, margin, fee)));
        }
        
        self.balance -= margin + fee;
        self.total_fees += fee;
        
        // Create Position
        let position = Position {
            coin: coin.to_string(),
            direction: direction.to_string(),
            size,
            entry_price: price,
            margin_used: margin,
            leverage,
            tp_price: tp,
            sl_price: sl, 
            liquidation_price: 0.0, // TODO: Calc Liq
            entry_time: chrono::Utc::now().timestamp_millis() as u64,
            unrealized_pnl: 0.0,
        };
        
        self.positions.insert(coin.to_string(), position.clone());
        
        Ok(TradeAction {
                session_id: String::new(),
                coin: coin.to_string(),
                action: "OPEN".to_string(),
                direction: direction.to_string(),
                price,
                size,
                reason: "Sim Entry".to_string(),
                pnl: None,
                fee: Some(fee),
                exit_type: None,
                ts: chrono::Utc::now().timestamp_millis() as u64,
                entry_rsi: None,
                entry_bb_dist: None,
                entry_score: None,
        })
    }

    async fn close_position(&mut self, coin: &str, price: f64, reason: &str, ts: u64) -> Result<TradeAction, OrderError> {
         if let Some(pos) = self.positions.remove(coin) {
            let pnl = pos.calc_pnl(price);
            let notional = price * pos.size;
            let fee = notional * self.taker_fee; 
            
            self.balance = (self.balance + pos.margin_used + pnl - fee).max(0.0);
            self.total_fees += fee;
            self.trade_count += 1;
            
            Ok(TradeAction {
                session_id: String::new(),
                coin: coin.to_string(),
                action: "CLOSE".to_string(),
                direction: pos.direction,
                price,
                size: pos.size,
                reason: reason.to_string(),
                pnl: Some(pnl),
                fee: Some(fee),
                exit_type: Some(reason.to_string()),
                ts,
                entry_rsi: None,
                entry_bb_dist: None,
                entry_score: None,
            })
         } else {
             Err(OrderError::InvalidOrder(format!("No position in {}", coin)))
         }
    }
    
    async fn withdraw(&mut self, amount: f64) -> Result<f64, OrderError> {
        if self.balance >= amount {
            self.balance -= amount;
            Ok(amount)
        } else {
            Err(OrderError::InsufficientFunds(format!("Cannot withdraw {:.2}, balance {:.2}", amount, self.balance)))
        }
    }
    
    async fn cancel_order(&mut self, _asset_idx: u32, _oid: u64) -> Result<(), OrderError> {
        // Sim has no resting orders to cancel
        Ok(())
    }

    async fn cancel_all_orders(&mut self) -> Result<u64, OrderError> {
        // Sim has no resting orders
        Ok(0)
    }

    async fn sweep_dead_orders(&mut self) -> Result<(), OrderError> {
        Ok(())
    }

    fn as_sim_mut(&mut self) -> Option<&mut SimExchange> { Some(self) }
}

// ─── Live Exchange (Mock / Hyperliquid) ────────────────────────────

#[derive(Debug, Clone)]
pub struct AssetInfo {
    pub sz_decimals: u32,
    pub max_leverage: u32,
}

pub struct LiveExchange {
    pub base_url: String,
    pub account_address: String,
    pub private_key: String,
    pub client: reqwest::Client,
    pub coin_to_asset: HashMap<String, u32>,
    pub asset_info: HashMap<u32, AssetInfo>,
}

impl LiveExchange {
    pub fn new(account_address: String, private_key: String) -> Self {
        Self {
            base_url: "https://api.hyperliquid.xyz".to_string(),
            account_address,
            private_key,
            client: reqwest::Client::new(),
            coin_to_asset: HashMap::new(),
            asset_info: HashMap::new(),
        }
    }

    pub async fn init(&mut self) -> Result<(), OrderError> {
        log::info!("Fetching exchange metadata (universe)...");
        let payload = serde_json::json!({ "type": "meta" });
        let data = self.post_info(payload).await?;
        
        if let Some(universe) = data["universe"].as_array() {
            for (i, asset) in universe.iter().enumerate() {
                if let Some(name) = asset["name"].as_str() {
                    let asset_idx = i as u32;
                    self.coin_to_asset.insert(name.to_string(), asset_idx);
                    
                    let sz_decimals = asset["szDecimals"].as_u64().unwrap_or(4) as u32;
                    let max_leverage = asset["maxLeverage"].as_u64().unwrap_or(20) as u32;
                    self.asset_info.insert(asset_idx, AssetInfo { sz_decimals, max_leverage });
                }
            }
        }
        log::info!("Loaded {} assets from universe.", self.coin_to_asset.len());
        Ok(())
    }

    async fn post_info(&self, payload: serde_json::Value) -> Result<serde_json::Value, OrderError> {
        let resp = self.client.post(format!("{}/info", self.base_url))
            .json(&payload)
            .send()
            .await
            .map_err(|e| OrderError::NetworkError(e.to_string()))?;
        
        resp.json().await.map_err(|e| OrderError::NetworkError(e.to_string()))
    }

    async fn post_exchange(&self, action: serde_json::Value, nonce: u64, signature: crate::signing::Signature) -> Result<serde_json::Value, OrderError> {
        let payload = serde_json::json!({
            "action": action,
            "nonce": nonce,
            "signature": signature,
            "vaultAddress": serde_json::Value::Null,
        });
        
        log::info!("EXCHANGE REQUEST: {}", serde_json::to_string(&payload).unwrap_or_default());

        let resp = self.client.post(format!("{}/exchange", self.base_url))
            .json(&payload)
            .send()
            .await
            .map_err(|e| OrderError::NetworkError(e.to_string()))?;
        
        let status = resp.status();
        let text = resp.text().await.map_err(|e| OrderError::NetworkError(e.to_string()))?;
        log::info!("EXCHANGE RESPONSE ({}): {}", status, text);

        serde_json::from_str(&text).map_err(|e| OrderError::NetworkError(e.to_string()))
    }
}

#[async_trait]
impl ExchangeClient for LiveExchange {
    async fn get_balance(&self) -> Result<f64, OrderError> {
        let payload = serde_json::json!({
            "type": "clearinghouseState",
            "user": self.account_address
        });

        let data = self.post_info(payload).await?;
        log::debug!("FULL CLEARINGHOUSE STATE: {}", serde_json::to_string(&data).unwrap_or_default());
        
        // HL returns 'withdrawable'. Let's also check 'marginSummary.accountValue'
        let withdrawable = data["withdrawable"].as_f64()
            .or_else(|| data["withdrawable"].as_str().and_then(|s| s.parse::<f64>().ok()))
            .unwrap_or(0.0);
            
        let account_value = data["marginSummary"]["accountValue"].as_f64()
            .or_else(|| data["marginSummary"]["accountValue"].as_str().and_then(|s| s.parse::<f64>().ok()))
            .unwrap_or(withdrawable);

        log::info!("BALANCE DEBUG: Withdrawable: ${:.2}, Account Value: ${:.2}", withdrawable, account_value);
        
        // Return account_value so the bot sees the total equity including active margin.
        // This resolves the issue where 'withdrawable' drops to near zero when a position is open.
        let balance = account_value;
        
        Ok(balance)
    }

    async fn get_positions(&self) -> Result<Vec<Position>, OrderError> {
        let payload = serde_json::json!({
            "type": "clearinghouseState",
            "user": self.account_address
        });

        let data = self.post_info(payload).await?;
        let mut positions = Vec::new();

        if let Some(pos_list) = data["assetPositions"].as_array() {
            for p in pos_list {
                let pos_data = &p["position"];
                let coin = pos_data["coin"].as_str().unwrap_or("").to_string();
                let sz = pos_data["s"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                if sz.abs() < 1e-8 { continue; }

                let entry_price = pos_data["entryPx"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                let direction = if sz > 0.0 { "LONG" } else { "SHORT" };

                positions.push(Position {
                    coin,
                    direction: direction.to_string(),
                    size: sz.abs(),
                    entry_price,
                    margin_used: 0.0, 
                    leverage: 0.0,
                    tp_price: 0.0,
                    sl_price: 0.0,
                    liquidation_price: 0.0,
                    entry_time: 0,
                    unrealized_pnl: pos_data["unrealizedPnl"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
                });
            }
        }
        Ok(positions)

    }

    async fn get_open_orders(&self, coin: &str) -> Result<Vec<serde_json::Value>, OrderError> {
        let payload = serde_json::json!({
            "type": "openOrders",
            "user": self.account_address
        });

        let data = self.post_info(payload).await?;
        let mut orders = Vec::new();
        
        if let Some(arr) = data.as_array() {
            for order in arr {
                if coin.is_empty() || order["coin"].as_str() == Some(coin) {
                    orders.push(order.clone());
                }
            }
        }
        Ok(orders)
    }

    async fn open_order(&mut self, coin: &str, direction: &str, size: f64, price: f64, leverage: f64, tp: f64, sl: f64, post_only: bool) -> Result<TradeAction, OrderError> {
        let is_buy = direction == "LONG";
        
        // Note: Idempotency is handled by the caller (main loop does cancel-then-replace).
        // We allow multiple orders per coin for MM grid quoting.

        let asset_idx = *self.coin_to_asset.get(coin).ok_or_else(|| OrderError::InvalidOrder(format!("Unknown coin: {}", coin)))?;
        
        // If post_only is false, we want this to act as a Taker (Market) order.
        // Hyperliquid implements Market orders as aggressive Limit orders.
        let execution_price = if !post_only {
            if is_buy {
                price * 1.05 // Aggressive buy
            } else {
                price * 0.95 // Aggressive sell
            }
        } else {
            price
        };

        let price_rounded = round_to_5_sig_figs(execution_price);
        
        let nonce = chrono::Utc::now().timestamp_millis() as u64;
        
        let asset_idx = *self.coin_to_asset.get(coin).ok_or_else(|| OrderError::InvalidOrder(format!("Unknown coin: {}", coin)))?;
        let sz_decimals = self.asset_info.get(&asset_idx).map(|info| info.sz_decimals).unwrap_or(4);
        
        // Re-round size using precise decimals from API if available
        let size_rounded = round_f64(size, sz_decimals as usize);

        // Format as Strings using float_to_wire (matches Python SDK: strips trailing zeros)
        let limit_px_str = float_to_wire(price_rounded);
        let sz_str = float_to_wire(size_rounded);

        // Clamp Leverage to Asset Limit
        let max_lev = self.asset_info.get(&asset_idx).map(|info| info.max_leverage as f64).unwrap_or(20.0);
        let final_leverage = if leverage > max_lev {
            log::warn!("LEVERAGE: Requested {:.1}x for {} but max is {:.1}x. Clamping.", leverage, coin, max_lev);
            max_lev
        } else {
            leverage
        };

        // 2. Place Order (Use final_leverage for margin check if we were doing it locally, 
        // but for API we imply cross margin usually unless isolated. 
        // Wait, Hyperliquid API doesn't take leverage in "order" action, it takes it in "updateLeverage" action.
        // The default cross margin uses the account leverage setting.
        // We probably need to "updateLeverage" separately if we want to change it.
        // But for this bot, we assume the account is set to a reasonable cross leverage (e.g. 20x or 50x)
        // and we manage risk by position sizing.
        // However, if we wanted to enforce it, we'd need a separate call.
        // For now, we just proceed with the order.
        
        let mut orders = Vec::new();
        let tif_str = if post_only { "Alo".to_string() } else { "Ioc".to_string() };
        orders.push(crate::signing::OrderRequest {
            asset: asset_idx,
            is_buy: is_buy,
            limit_px: limit_px_str,
            sz: sz_str,
            reduce_only: false, 
            order_type: crate::signing::OrderTypeWire::Limit(crate::signing::LimitOrderWire { tif: tif_str }),
        });

        let action_wire = crate::signing::ActionWire {
             r#type: "order".to_string(),
             orders: orders,
             grouping: "na".to_string(),
        };

        let (sig, action_json) = crate::signing::sign_l1_action(&self.private_key, action_wire, nonce).await
             .map_err(|e| OrderError::InvalidOrder(e.to_string()))?;
        
        let result = self.post_exchange(action_json, nonce, sig).await?;
        
        if result["status"].as_str() == Some("err") {
            return Err(OrderError::InvalidOrder(result["response"].to_string()));
        }

        // Hyperliquid can return "ok" status even if the order fails internally (e.g. Insufficient Margin)
        // We must check the statuses array.
        if let Some(statuses) = result["response"]["data"]["statuses"].as_array() {
            if let Some(s) = statuses.get(0) {
                if let Some(err) = s["error"].as_str() {
                    return Err(OrderError::InvalidOrder(err.to_string()));
                }
            }
        }

        Ok(TradeAction {
             session_id: String::new(),
             coin: coin.to_string(),
             action: "OPEN".to_string(),
             direction: direction.to_string(),
             price: price_rounded,
             size: size_rounded,
             reason: "Live Order Placed".to_string(),
             pnl: None,
             fee: Some(0.0),
             exit_type: None,
             ts: nonce,
             entry_rsi: None,
             entry_bb_dist: None,
             entry_score: None,
        })
    }

    async fn close_position(&mut self, coin: &str, price: f64, reason: &str, ts: u64) -> Result<TradeAction, OrderError> {
        let positions = self.get_positions().await?;
        let pos = positions.iter().find(|p| p.coin == coin)
            .ok_or_else(|| OrderError::InvalidOrder(format!("No live position to close for {}", coin)))?;
        
        let is_buy = pos.direction == "SHORT"; 
        
        let execution_price = if is_buy {
            price * 1.05 // Aggressive buy
        } else {
            price * 0.95 // Aggressive sell
        };
        let price_rounded = round_to_5_sig_figs(execution_price);
        let nonce = chrono::Utc::now().timestamp_millis() as u64;

        let asset_idx = *self.coin_to_asset.get(coin).ok_or_else(|| OrderError::InvalidOrder(format!("Unknown coin: {}", coin)))?;
        let sz_decimals = self.asset_info.get(&asset_idx).map(|info| info.sz_decimals).unwrap_or(4); // Default 4 if missing
        
        // Format strings using float_to_wire (matches Python SDK)
        let limit_px_str = float_to_wire(price_rounded);
        let sz_str = float_to_wire(pos.size);

        let mut orders = Vec::new();
        orders.push(crate::signing::OrderRequest {
            asset: asset_idx,
            is_buy: is_buy,
            limit_px: limit_px_str,
            sz: sz_str,
            reduce_only: true,
            order_type: crate::signing::OrderTypeWire::Limit(crate::signing::LimitOrderWire { tif: "Ioc".to_string() }), // IOC for closing
        });

        let action_wire = crate::signing::ActionWire {
             r#type: "order".to_string(),
             orders: orders,
             grouping: "na".to_string(),
        };

        let (sig, action_json) = crate::signing::sign_l1_action(&self.private_key, action_wire, nonce).await
            .map_err(|e| OrderError::InvalidOrder(e.to_string()))?;
        
        let result = self.post_exchange(action_json, nonce, sig).await?;

        Ok(TradeAction {
             session_id: String::new(),
             coin: coin.to_string(),
             action: "CLOSE".to_string(),
             direction: pos.direction.clone(),
             price: price_rounded,
             size: pos.size,
             reason: reason.to_string(),
             pnl: Some(pos.calc_pnl(price_rounded)),
             fee: Some(0.0),
             exit_type: Some(reason.to_string()),
             ts: nonce,
             entry_rsi: None,
             entry_bb_dist: None,
             entry_score: None,
        })
    }

    async fn withdraw(&mut self, _amount: f64) -> Result<f64, OrderError> {
        Err(OrderError::InvalidOrder("Withdraw not implemented in engine yet".to_string()))
    }

    async fn cancel_order(&mut self, asset_idx: u32, oid: u64) -> Result<(), OrderError> {
        let nonce = chrono::Utc::now().timestamp_millis() as u64;
        let (sig, action_json) = crate::signing::sign_cancel_action(&self.private_key, asset_idx, oid, nonce).await
            .map_err(|e| OrderError::InvalidOrder(e.to_string()))?;
        let result = self.post_exchange(action_json, nonce, sig).await?;
        if result["status"].as_str() == Some("err") {
            return Err(OrderError::InvalidOrder(result["response"].to_string()));
        }
        Ok(())
    }

    /// Phase 9E: Cancel ALL open orders across ALL coins.
    /// Returns the count of successfully cancelled orders.
    async fn cancel_all_orders(&mut self) -> Result<u64, OrderError> {
        let orders = self.get_open_orders("").await?;
        let total = orders.len();
        if total == 0 {
            log::info!("[CANCEL ALL] No open orders to cancel.");
            return Ok(0);
        }
        log::warn!("[CANCEL ALL] Cancelling {} open orders...", total);

        let mut cancelled = 0u64;
        for order in &orders {
            let coin = match order["coin"].as_str() { Some(c) => c, None => continue };
            let oid  = match order["oid"].as_u64()  { Some(o) => o, None => continue };
            let asset_idx = match self.coin_to_asset.get(coin) {
                Some(&idx) => idx,
                None => { log::warn!("[CANCEL ALL] Unknown coin: {}", coin); continue; }
            };

            let nonce = chrono::Utc::now().timestamp_millis() as u64 + cancelled;
            match crate::signing::sign_cancel_action(&self.private_key, asset_idx, oid, nonce).await {
                Ok((sig, action_json)) => {
                    match self.post_exchange(action_json, nonce, sig).await {
                        Ok(res) if res["status"].as_str() != Some("err") => {
                            cancelled += 1;
                            log::info!("[CANCEL ALL] Cancelled {} oid={}", coin, oid);
                        }
                        Ok(res) => {
                            log::error!("[CANCEL ALL] Exchange error for {} oid={}: {}", coin, oid, res["response"]);
                        }
                        Err(e) => log::error!("[CANCEL ALL] Network error for {} oid={}: {}", coin, oid, e),
                    }
                }
                Err(e) => log::error!("[CANCEL ALL] Signing error for {} oid={}: {}", coin, oid, e),
            }
        }
        log::warn!("[CANCEL ALL] Done. Cancelled {}/{} orders.", cancelled, total);
        Ok(cancelled)
    }

    async fn sweep_dead_orders(&mut self) -> Result<(), OrderError> {
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;
        let orders = self.get_open_orders("").await?;
        let mut cancel_count = 0;

        for order in orders {
            if let (Some(coin), Some(oid), Some(ts)) = (
                order["coin"].as_str(),
                order["oid"].as_u64(),
                order["timestamp"].as_u64()
            ) {
                if now_ms > ts && now_ms - ts > 15 * 60 * 1000 {
                    let asset_idx = match self.coin_to_asset.get(coin) {
                        Some(&idx) => idx,
                        None => continue,
                    };
                    
                    log::info!("SWEEP: Canceling stale {:?} order dead for {:.1} mins", coin, (now_ms - ts) as f64 / 60000.0);
                    
                    let nonce = chrono::Utc::now().timestamp_millis() as u64 + cancel_count;
                    if let Ok((sig, action_json)) = crate::signing::sign_cancel_action(&self.private_key, asset_idx, oid, nonce).await {
                        let result = self.post_exchange(action_json, nonce, sig).await;
                        if let Err(e) = result {
                            log::error!("Failed to cancel stale order {}: {:?}", oid, e);
                        } else {
                            cancel_count += 1;
                        }
                    }
                }
            }
        }
        
        Ok(())
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

pub fn round_to_5_sig_figs(val: f64) -> f64 {
    if val == 0.0 {
        return 0.0;
    }
    let d = 5 - 1 - (val.abs().log10().floor() as i32);
    let d = d.clamp(0, 8);
    let factor = 10_f64.powi(d);
    (val * factor).round() / factor
}

pub fn round_f64(val: f64, decimals: usize) -> f64 {
    let factor = 10_f64.powi(decimals as i32);
    (val * factor).round() / factor
}

/// Matches the Python SDK's `float_to_wire` function:
/// ```python
/// def float_to_wire(x: float) -> str:
///     rounded = f"{x:.8f}"
///     normalized = Decimal(rounded).normalize()
///     return f"{normalized:f}"
/// ```
/// Round to 8 decimals, then strip trailing zeros (but keep at least one digit after decimal point is NOT required — SDK allows "100" with no decimal).
pub fn float_to_wire(x: f64) -> String {
    // Step 1: Round to 8 decimal places
    let rounded = format!("{:.8}", x);
    
    // Step 2: Normalize (strip trailing zeros after decimal point)
    if rounded.contains('.') {
        let trimmed = rounded.trim_end_matches('0');
        let trimmed = trimmed.trim_end_matches('.');
        trimmed.to_string()
    } else {
        rounded
    }
}
