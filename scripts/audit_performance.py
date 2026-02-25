#!/usr/bin/env python3
import requests
import json
import os
from datetime import datetime

ENV_FILE = "backend/mm-engine-rs/.env"

def load_env():
    env = {}
    if not os.path.exists(ENV_FILE):
        return {}
    with open(ENV_FILE, 'r') as f:
        for line in f:
            if '=' in line and not line.startswith('#'):
                parts = line.strip().split('=', 1)
                if len(parts) == 2:
                    env[parts[0]] = parts[1]
    return env

def fetch_fills(address):
    url = "https://api.hyperliquid.xyz/info"
    payload = {"type": "userFills", "user": address}
    resp = requests.post(url, json=payload)
    return resp.json()

def analyze():
    env = load_env()
    address = env.get("HL_ADDRESS")
    if not address:
        print("Error: HL_ADDRESS not found in .env")
        return

    fills = fetch_fills(address)
    
    # We only care about trades that closed a position (had PnL)
    closed_trades = [f for f in fills if float(f.get('closedPnl', 0)) != 0]
    
    wins = [f for f in closed_trades if float(f['closedPnl']) > 0]
    losses = [f for f in closed_trades if float(f['closedPnl']) < 0]
    
    total_pnl = sum(float(f['closedPnl']) for f in closed_trades)
    total_fees = sum(float(f['fee']) for f in fills) # All fills have fees
    
    win_rate = (len(wins) / len(closed_trades) * 100) if closed_trades else 0
    
    avg_win = (sum(float(f['closedPnl']) for f in wins) / len(wins)) if wins else 0
    avg_loss = (sum(float(f['closedPnl']) for f in losses) / len(losses)) if losses else 0
    
    print(f"📊 --- PERFORMANCE AUDIT (Last {len(fills)} Fills) ---")
    print(f"Total Fills:          {len(fills)}")
    print(f"Closed Trades:        {len(closed_trades)}")
    print(f"Wins:                 {len(wins)}")
    print(f"Losses:               {len(losses)}")
    print(f"Win Rate:             {win_rate:.1f}%")
    print("-" * 40)
    print(f"Gross PnL:            ${total_pnl:.4f}")
    print(f"Total Fees:           ${total_fees:.4f}")
    print(f"Net Profit:           ${total_pnl - total_fees:.4f}")
    print("-" * 40)
    print(f"Avg Win:              ${avg_win:.4f}")
    print(f"Avg Loss:             ${avg_loss:.4f}")
    print(f"Reward/Risk Ratio:    {abs(avg_win/avg_loss) if avg_loss != 0 else 0:.2f}")
    
    print("\n💀 --- TOP 5 TOXIC LOSSES ---")
    sorted_losses = sorted(losses, key=lambda x: float(x['closedPnl']))
    for f in sorted_losses[:5]:
        ts = datetime.fromtimestamp(f['time'] / 1000).strftime('%Y-%m-%d %H:%M')
        print(f"{ts} | {f['coin']:<10} | Lnl: ${float(f['closedPnl']):.4f} | Px: {f['px']}")

if __name__ == "__main__":
    analyze()
