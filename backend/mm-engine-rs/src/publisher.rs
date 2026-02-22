// ─────────────────────────────────────────────────────────────────────────────
// publisher.rs — Redis IPC Bridge (MM Engine ↔ Python Screener)
//
// Phase 9H: Implements both sides of the bridge:
//   - MmScreenerSubscriber: READS asset configs from the Python screener
//   - MmStatusPublisher:    WRITES engine status / shadow fills back to Python
//
// Channel layout (all prefixed with "mm:"):
//   mm:asset_config   → Python → Rust: Vec<MmAssetConfig> as JSON (every 30s)
//   mm:shadow_fills   → Rust  → Python: ShadowFill JSON stream
//   mm:engine_status  → Rust  → Python: heartbeat + session PnL
// ─────────────────────────────────────────────────────────────────────────────
use redis::AsyncCommands;
use std::error::Error;
use crate::market_maker::{MmAssetConfig, ShadowFill};

const CHANNEL_ASSET_CONFIG: &str = "mm:asset_config";
const CHANNEL_SHADOW_FILLS: &str = "mm:shadow_fills";
const CHANNEL_ENGINE_STATUS: &str = "mm:engine_status";

// ─── Screener Subscriber ──────────────────────────────────────────────────────

/// Connects to Redis and listens on `mm:asset_config` for the Python
/// screener's asset selection + per-asset config JSON.
pub struct MmScreenerSubscriber {
    client: redis::Client,
}

impl MmScreenerSubscriber {
    pub fn new(redis_url: &str) -> Option<Self> {
        redis::Client::open(redis_url).ok().map(|client| Self { client })
    }

    /// Phase 9H: Spawns a background task that listens for screener updates.
    /// Returns a tokio watch channel receiver — the main loop reads from this
    /// to get the latest asset configs without blocking.
    pub async fn spawn_listener(
        &self,
        sender: tokio::sync::watch::Sender<Vec<MmAssetConfig>>,
    ) -> Result<(), Box<dyn Error>> {
        let client = self.client.clone();
        tokio::spawn(async move {
            loop {
                match client.get_async_connection().await {
                    Ok(con) => {
                        let mut pubsub = con.into_pubsub();
                        if let Err(e) = pubsub.subscribe(CHANNEL_ASSET_CONFIG).await {
                            log::error!("[SCREENER] Failed to subscribe to {}: {}", CHANNEL_ASSET_CONFIG, e);
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                            continue;
                        }
                        log::info!("[SCREENER] Subscribed to {} for asset configs.", CHANNEL_ASSET_CONFIG);

                        use futures_util::StreamExt;
                        let mut stream = pubsub.into_on_message();
                        while let Some(msg) = stream.next().await {
                            if let Ok(payload) = msg.get_payload::<String>() {
                                match serde_json::from_str::<Vec<MmAssetConfig>>(&payload) {
                                    Ok(configs) => {
                                        log::info!(
                                            "[SCREENER] Received {} asset configs from screener.",
                                            configs.len()
                                        );
                                        let _ = sender.send(configs);
                                    }
                                    Err(e) => {
                                        log::warn!("[SCREENER] Failed to parse asset config: {}", e);
                                    }
                                }
                            }
                        }
                        log::warn!("[SCREENER] Redis pub/sub connection dropped. Reconnecting...");
                    }
                    Err(e) => {
                        log::error!("[SCREENER] Redis connection failed: {}. Retrying in 5s.", e);
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        });
        Ok(())
    }
}

// ─── Status Publisher ─────────────────────────────────────────────────────────

/// Publishes shadow fills and engine heartbeat back to the Python screener.
pub struct MmStatusPublisher {
    client: redis::Client,
}

impl MmStatusPublisher {
    pub fn new(redis_url: &str) -> Option<Self> {
        redis::Client::open(redis_url).ok().map(|client| Self { client })
    }

    /// Publishes a shadow fill event so Python can log and analyse it.
    pub async fn publish_shadow_fill(&self, fill: &ShadowFill) -> Result<(), Box<dyn Error>> {
        let mut con = self.client.get_async_connection().await?;
        let payload = serde_json::to_string(fill)?;
        let _: () = con.publish(CHANNEL_SHADOW_FILLS, &payload).await?;
        Ok(())
    }

    /// Publishes a heartbeat with session PnL so Python can monitor the engine.
    pub async fn publish_status(
        &self,
        session_id: &str,
        shadow_pnl_usd: f64,
        is_halted: bool,
        regime: &str,
    ) -> Result<(), Box<dyn Error>> {
        let mut con = self.client.get_async_connection().await?;
        let payload = serde_json::json!({
            "session_id":     session_id,
            "shadow_pnl_usd": shadow_pnl_usd,
            "is_halted":      is_halted,
            "regime":         regime,
            "ts_ms": chrono::Utc::now().timestamp_millis()
        });
        let _: () = con.publish(CHANNEL_ENGINE_STATUS, payload.to_string()).await?;
        Ok(())
    }
}

// ─── Legacy Publisher (kept for directional bot compatibility) ─────────────────

pub struct RedisPublisher {
    client: redis::Client,
    con: Option<redis::aio::Connection>,
    prefix: String,
}

impl RedisPublisher {
    pub fn new(redis_url: &str, prefix: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let client = redis::Client::open(redis_url)?;
        Ok(Self { client, con: None, prefix: prefix.to_string() })
    }

    pub async fn connect(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.con = Some(self.client.get_async_connection().await?);
        Ok(())
    }

    pub async fn publish_message(&mut self, channel: &str, msg: &str) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(con) = &mut self.con {
            let prefixed = format!("{}{}", self.prefix, channel);
            let _: () = con.publish(&prefixed, msg).await?;
        }
        Ok(())
    }
}
