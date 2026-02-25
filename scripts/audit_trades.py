#!/usr/bin/env python3
import requests
import json
import os
from datetime import datetime

ENV_FILE = "backend/mm-engine-rs/.env"

def load_env():
    env = {}
    with open(ENV_FILE, 'r') as f:
        for line in f:
            if '=' in line and not line.startswith('#'):
                k, v = line.strip().split('=', 1)
                env[k] = v
    return env

def fetch_fills(address):
    url = "https://api.hyperliquid.xyz/info"
    payload = {"type": "userFills", "user": address}
    resp = requests.post(url, json=payload)
    return resp.json()

def analyze():
    env = load_env()
    address = env.get("HL_ADDRESS")
    fills = fetch_fills(address)
    
    print(f"{'Time':<20} | {'Coin':<10} | {'Side':<5} | {'Size':<10} | {'Price':<10} | {'Fee':<10} | {'PnL':<10}")
    print("-" * 85)
    
    total_pnl = 0
    total_fees = 0
    
    for f in fills[:50]: # Last 50 fills
        ts = datetime.fromtimestamp(f['time'] / 1000).strftime('%Y-%m-%d %H:%M:%S')
        coin = f['coin']
        side = 'BUY' if f['side'] == 'B' else 'SELL'
        sz = f['sz']
        px = f['px']
        fee = float(f['fee'])
        pnl = float(f.get('closedPnl', 0))
        
        total_pnl += pnl
        total_fees += fee
        
        print(f"{ts:<20} | {coin:<10} | {side:<5} | {sz:<10} | {px:<10} | {fee:<10.5f} | {pnl:<10.5f}")

    print("-" * 85)
    print(f"Total Realized PnL (last 50): ${total_pnl:.4f}")
    print(f"Total Fees Paid:              ${total_fees:.4f}")

if __name__ == "__main__":
    analyze()
