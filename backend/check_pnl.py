import requests
import json
import os
from datetime import datetime

url = "https://api.hyperliquid.xyz/info"
user = "0x6d5Ca0480Eaf4862d0E174B58B72431c4d01efD3"
data = {"type": "userFills", "user": user}

try:
    res = requests.post(url, json=data).json()
    if isinstance(res, list):
        # res is from oldest to newest or newest to oldest?
        # usually APIs return newest first. Let's reverse it to chronological.
        fills = sorted(res, key=lambda x: x['time'])
        
        # We'll just look at the last 100 fills
        fills = fills[-100:]
        
        positions = {}
        realized_pnl = 0.0
        wins = 0
        losses = 0
        
        print("--- RECENT COMPLETED TRADES ---")
        
        for fill in fills:
            coin = fill['coin']
            side = fill['dir'] # "Open Long", "Close Long", "Open Short", "Close Short", "Long > Short", "Short > Long"
            px = float(fill['px'])
            sz = float(fill['sz'])
            fee = float(fill.get('fee', 0))
            ts = datetime.fromtimestamp(fill['time'] / 1000.0).strftime('%H:%M:%S')
            
            if coin not in positions:
                positions[coin] = {'sz': 0.0, 'cost': 0.0}
                
            pos = positions[coin]
            
            # Simplified PnL tracking for exact open/close matches
            if side in ["Open Long", "Open Short"]:
                pos['sz'] += sz
                pos['cost'] += (px * sz) + fee
            elif side in ["Close Long", "Close Short"]:
                if pos['sz'] > 0:
                    avg_entry = pos['cost'] / pos['sz']
                    # Calculate PnL for the closed portion
                    if side == "Close Long":
                        pnl = (px - avg_entry) * sz - fee
                    else: # Close Short
                        pnl = (avg_entry - px) * sz - fee
                        
                    realized_pnl += pnl
                    if pnl > 0:
                        wins += 1
                        print(f"[{ts}] {coin} WIN: +${pnl:.4f} (Entry: ~{avg_entry:.4f}, Exit: {px:.4f})")
                    else:
                        losses += 1
                        print(f"[{ts}] {coin} LOSS: ${pnl:.4f} (Entry: ~{avg_entry:.4f}, Exit: {px:.4f})")
                    
                    pos['sz'] -= sz
                    pos['cost'] -= (avg_entry * sz)
            elif side in ["Long > Short", "Short > Long"]:
                # Full flip. Treat as closing current pos, opening new.
                if pos['sz'] > 0:
                    avg_entry = pos['cost'] / pos['sz']
                    close_sz = pos['sz']
                    remain_sz = sz - close_sz
                    open_fee = fee * (remain_sz / sz) if sz > 0 else 0
                    close_fee = fee * (close_sz / sz) if sz > 0 else 0
                    
                    if side == "Long > Short": # Closing long
                        pnl = (px - avg_entry) * close_sz - close_fee
                    else:
                        pnl = (avg_entry - px) * close_sz - close_fee
                        
                    realized_pnl += pnl
                    if pnl > 0:
                        wins += 1
                        print(f"[{ts}] {coin} FLIP-WIN: +${pnl:.4f}")
                    else:
                        losses += 1
                        print(f"[{ts}] {coin} FLIP-LOSS: ${pnl:.4f}")
                        
                    # Now we hold the other side
                    pos['sz'] = remain_sz
                    pos['cost'] = (px * remain_sz) + open_fee
                else:
                    pos['sz'] += sz
                    pos['cost'] += (px * sz) + fee
        
        print("\n--- SUMMARY FOR LAST 100 FILLS ---")
        print(f"Winning Trades : {wins}")
        print(f"Losing Trades  : {losses}")
        print(f"Net PnL (w/fees): ${realized_pnl:.4f}")
        
    else:
        print(f"Error: {res}")
except Exception as e:
    print(f"Exception: {e}")
