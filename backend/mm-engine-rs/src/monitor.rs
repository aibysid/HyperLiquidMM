use std::collections::VecDeque;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    pub window_size: usize,        // e.g. 100 trades
    pub min_trades_for_action: usize, // e.g. 20
    pub pf_threshold_warning: f64, // e.g. 1.0 -> reduce size
    pub pf_threshold_critical: f64, // e.g. 0.8 -> stop trading
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            window_size: 100,
            min_trades_for_action: 20,
            pf_threshold_warning: 1.0,
            pf_threshold_critical: 0.8,
        }
    }
}

pub struct PerformanceMonitor {
    config: MonitorConfig,
    history: VecDeque<TradeResult>,
}

#[derive(Debug, Clone)]
struct TradeResult {
    pub pnl: f64,
    pub is_win: bool,
}

impl PerformanceMonitor {
    pub fn new(config: MonitorConfig) -> Self {
        Self {
            config,
            history: VecDeque::new(),
        }
    }

    pub fn record_trade(&mut self, pnl: f64) {
        if self.history.len() >= self.config.window_size {
            self.history.pop_front();
        }
        self.history.push_back(TradeResult {
            pnl,
            is_win: pnl > 0.0,
        });
    }

    pub fn get_metrics(&self) -> PerformanceMetrics {
        if self.history.is_empty() {
            return PerformanceMetrics::default();
        }

        let total_trades = self.history.len();
        let wins = self.history.iter().filter(|t| t.is_win).count();
        let win_rate = wins as f64 / total_trades as f64 * 100.0;
        
        let gross_profit: f64 = self.history.iter().filter(|t| t.pnl > 0.0).map(|t| t.pnl).sum();
        let gross_loss: f64 = self.history.iter().filter(|t| t.pnl < 0.0).map(|t| t.pnl.abs()).sum();
        
        let profit_factor = if gross_loss > 0.0 {
            gross_profit / gross_loss
        } else if gross_profit > 0.0 {
            100.0 
        } else {
            0.0
        };

        PerformanceMetrics {
            win_rate,
            profit_factor,
            trade_count: total_trades,
            suggested_action: self.evaluate_action(profit_factor, total_trades),
        }
    }

    fn evaluate_action(&self, pf: f64, count: usize) -> FeedbackAction {
        if count < self.config.min_trades_for_action {
            return FeedbackAction::None;
        }

        if pf < self.config.pf_threshold_critical {
            FeedbackAction::HaltTrading
        } else if pf < self.config.pf_threshold_warning {
            FeedbackAction::ReduceSize(0.5) // Cut size in half
        } else {
            FeedbackAction::None
        }
    }

    pub fn win_rate(&self) -> f64 {
        if self.history.is_empty() {
            0.0
        } else {
            let total_trades = self.history.len();
            let wins = self.history.iter().filter(|t| t.is_win).count();
            wins as f64 / total_trades as f64 * 100.0
        }
    }
}

#[derive(Debug, Default)]
pub struct PerformanceMetrics {
    pub win_rate: f64,
    pub profit_factor: f64,
    pub trade_count: usize,
    pub suggested_action: FeedbackAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FeedbackAction {
    None,
    ReduceSize(f64), // Multiplier, e.g. 0.5
    HaltTrading,
}

impl Default for FeedbackAction {
    fn default() -> Self {
        Self::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_monitor_initial_state() {
        let config = MonitorConfig::default();
        let monitor = PerformanceMonitor::new(config);
        let metrics = monitor.get_metrics();
        
        assert_eq!(metrics.trade_count, 0);
        assert_eq!(metrics.win_rate, 0.0);
        assert_eq!(metrics.suggested_action, FeedbackAction::None);
    }

    #[test]
    fn test_monitor_normal_operation() {
        let config = MonitorConfig {
            window_size: 10,
            min_trades_for_action: 5,
            pf_threshold_warning: 1.5,
            pf_threshold_critical: 1.0,
        };
        let mut monitor = PerformanceMonitor::new(config);

        // 5 Winning trades
        for _ in 0..5 {
            monitor.record_trade(10.0);
        }

        let metrics = monitor.get_metrics();
        assert_eq!(metrics.trade_count, 5);
        assert_eq!(metrics.win_rate, 100.0);
        assert_eq!(metrics.profit_factor, 100.0);
        assert_eq!(metrics.suggested_action, FeedbackAction::None);
    }

    #[test]
    fn test_reduce_size_action() {
        let config = MonitorConfig {
            window_size: 10,
            min_trades_for_action: 5,
            pf_threshold_warning: 1.5, // Trigger if PF < 1.5
            pf_threshold_critical: 0.5,
        };
        let mut monitor = PerformanceMonitor::new(config);

        // 3 Wins (+30), 3 Losses (-25) -> PF = 1.2
        for _ in 0..3 { monitor.record_trade(10.0); }
        for _ in 0..3 { monitor.record_trade(-8.33); }

        let metrics = monitor.get_metrics();
        // PF ~ 1.2 which is < 1.5
        match metrics.suggested_action {
            FeedbackAction::ReduceSize(x) => assert_eq!(x, 0.5),
            _ => panic!("Expected ReduceSize, got {:?}", metrics.suggested_action),
        }
    }

    #[test]
    fn test_halt_trading_action() {
        let config = MonitorConfig {
            window_size: 10,
            min_trades_for_action: 5,
            pf_threshold_warning: 1.5,
            pf_threshold_critical: 0.8, // Trigger if PF < 0.8
        };
        let mut monitor = PerformanceMonitor::new(config);

        // 1 Win (+10), 5 Losses (-50) -> PF = 0.2
        monitor.record_trade(10.0);
        for _ in 0..5 { monitor.record_trade(-10.0); }

        let metrics = monitor.get_metrics();
        assert_eq!(metrics.suggested_action, FeedbackAction::HaltTrading);
    }

    #[test]
    fn test_window_rolling() {
        let config = MonitorConfig {
            window_size: 3,
            min_trades_for_action: 1,
            pf_threshold_warning: 0.0,
            pf_threshold_critical: 0.0,
        };
        let mut monitor = PerformanceMonitor::new(config);

        monitor.record_trade(1.0);
        monitor.record_trade(2.0);
        monitor.record_trade(3.0);
        
        assert_eq!(monitor.history.len(), 3);
        assert_eq!(monitor.history.front().unwrap().pnl, 1.0);

        monitor.record_trade(4.0);
        // [2, 3, 4] -> 1.0 pushed out
        assert_eq!(monitor.history.len(), 3);
        assert_eq!(monitor.history.front().unwrap().pnl, 2.0);
        assert_eq!(monitor.history.back().unwrap().pnl, 4.0);
    }
}
