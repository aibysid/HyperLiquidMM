use chrono::{DateTime, Utc, Duration};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub max_daily_drawdown_percent: f64, // e.g. 0.15 (15%)
    pub max_consecutive_losses: usize,   // e.g. 5
    pub trading_halt_duration_secs: i64, // e.g. 3600 (1 hour)
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_daily_drawdown_percent: 0.10,
            max_consecutive_losses: 5,
            trading_halt_duration_secs: 3600,
        }
    }
}

pub struct RiskManager {
    config: RiskConfig,
    start_of_day_balance: f64,
    last_day_reset: DateTime<Utc>,
    consecutive_loss_count: usize,
    halt_until: Option<DateTime<Utc>>,
    is_kill_switch_active: bool,
}

impl RiskManager {
    pub fn new(config: RiskConfig, current_balance: f64) -> Self {
        Self {
            config,
            start_of_day_balance: current_balance,
            last_day_reset: Utc::now(),
            consecutive_loss_count: 0,
            halt_until: None,
            is_kill_switch_active: false,
        }
    }

    /// Check if trading is allowed based on current state
    pub fn can_trade(&self) -> Result<(), String> {
        if self.is_kill_switch_active {
            return Err("Kill switch is ACTIVE".to_string());
        }

        if let Some(halt_end) = self.halt_until {
            if Utc::now() < halt_end {
                return Err(format!("Trading halted until {}", halt_end));
            }
        }

        Ok(())
    }

    /// Update state with a closed trade result
    pub fn update_trade_result(&mut self, pnl: f64, current_balance: f64) {
        // 1. Reset daily stats if it's a new day (UTC)
        let now = Utc::now();
        if now.date_naive() > self.last_day_reset.date_naive() {
            self.start_of_day_balance = current_balance;
            self.last_day_reset = now;
        }

        // 2. Update consecutive losses
        if pnl < 0.0 {
            self.consecutive_loss_count += 1;
        } else {
            self.consecutive_loss_count = 0;
        }

        // 3. Check circuit breakers
        self.check_circuit_breakers(current_balance);
    }

    fn check_circuit_breakers(&mut self, current_balance: f64) {
        // A. Consecutive Loss Halt
        if self.consecutive_loss_count >= self.config.max_consecutive_losses {
            let halt_duration = Duration::seconds(self.config.trading_halt_duration_secs);
            self.halt_until = Some(Utc::now() + halt_duration);
            self.consecutive_loss_count = 0; // Reset counter after halting
            log::warn!("RISK: Halt triggered! {} consecutive losses. Pausing for {}s", 
                self.config.max_consecutive_losses, self.config.trading_halt_duration_secs);
        }

        // B. Daily Drawdown Halt
        let drawdown = (self.start_of_day_balance - current_balance) / self.start_of_day_balance;
        if drawdown > self.config.max_daily_drawdown_percent {
            // Halt until tomorrow
            let tomorrow = Utc::now().date_naive().succ_opt().unwrap().and_hms_opt(0, 0, 0).unwrap().and_utc();
            self.halt_until = Some(tomorrow);
            log::error!("RISK: CRITICAL! Daily drawdown {:.1}% exceeds limit {:.1}%. Halting until tomorrow.", 
                drawdown * 100.0, self.config.max_daily_drawdown_percent * 100.0);
        }
    }

    /// Manual Kill Switch
    pub fn set_kill_switch(&mut self, active: bool) {
        self.is_kill_switch_active = active;
        log::warn!("RISK: Kill switch set to {}", active);
    }

    pub fn clear_halt(&mut self) {
        self.halt_until = None;
        self.consecutive_loss_count = 0;
        log::info!("RISK: Trading halt manually cleared.");
    }

    pub fn get_state(&self) -> RiskState {
        RiskState {
            start_of_day_balance: self.start_of_day_balance,
            last_day_reset: self.last_day_reset,
            consecutive_loss_count: self.consecutive_loss_count,
            halt_until: self.halt_until,
            is_kill_switch_active: self.is_kill_switch_active,
        }
    }

    pub fn restore_state(&mut self, state: RiskState) {
        self.start_of_day_balance = state.start_of_day_balance;
        self.last_day_reset = state.last_day_reset;
        self.consecutive_loss_count = state.consecutive_loss_count;
        self.halt_until = state.halt_until;
        self.is_kill_switch_active = state.is_kill_switch_active;
        log::info!("RISK: Restored state: StartBal=${:.2}, LossStreak={}", 
            self.start_of_day_balance, self.consecutive_loss_count);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskState {
    pub start_of_day_balance: f64,
    pub last_day_reset: DateTime<Utc>,
    pub consecutive_loss_count: usize,
    pub halt_until: Option<DateTime<Utc>>,
    pub is_kill_switch_active: bool,
}
