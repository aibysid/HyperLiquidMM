use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use log::{info, error};
use crate::risk::RiskState;
use crate::exchange::Position;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineState {
    pub balance: f64,
    pub positions: Vec<Position>,
    pub risk_state: Option<RiskState>, // Option for backward compatibility
    pub vault_balance: Option<f64>,
}

impl EngineState {
    pub fn new(balance: f64) -> Self {
        Self {
            balance,
            positions: Vec::new(),
            risk_state: None,
            vault_balance: None,
        }
    }
}

pub fn load_state<P: AsRef<Path>>(path: P, default_balance: f64) -> EngineState {
    if path.as_ref().exists() {
        match fs::read_to_string(&path) {
            Ok(content) => {
                match serde_json::from_str::<EngineState>(&content) {
                    Ok(state) => {
                        info!("Loaded state from {:?}: Balance=${:.2}, Positions={}", 
                            path.as_ref(), state.balance, state.positions.len());
                        return state;
                    },
                    Err(e) => error!("Failed to parse state file: {}", e),
                }
            },
            Err(e) => error!("Failed to read state file: {}", e),
        }
    }
    info!("State file not found. Initializing new state with ${:.2}", default_balance);
    EngineState::new(default_balance)
}

pub fn save_state<P: AsRef<Path>>(path: P, state: &EngineState) {
    match serde_json::to_string_pretty(state) {
        Ok(content) => {
            if let Err(e) = fs::write(path, content) {
                error!("Failed to write state file: {}", e);
            }
        },
        Err(e) => error!("Failed to serialize state: {}", e),
    }
}
