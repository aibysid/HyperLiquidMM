import requests
import json
import os
from datetime import datetime, timedelta, timezone
from collections import defaultdict

url = "https://api.hyperliquid.xyz/info"
user = "0x6d5Ca0480Eaf4862d0E174B58B72431c4d01efD3"
data = {"type": "userFills", "user": user}

def analyze():
    try:
        res = requests.post(url, json=data).json()
        if not isinstance(res, list):
            print(f"Error fetching fills: {res}")
            return

        # One hour ago from now (UTC)
        now_ms = datetime.now(timezone.utc).timestamp() * 1000
        hour_ago_ms = now_ms - (60 * 60 * 1000)
        
        fills = sorted(res, key=lambda x: x['time'])
        fills = [f for f in fills if f['time'] >= hour_ago_ms]
        
        if not fills:
            print("No fills found in the last 60 minutes.")
            return

        print(f"\n=== ALGO ANALYSIS: LAST 60 MINUTES ({len(fills)} fills) ===\n")
        
        coin_stats = defaultdict(lambda: {
            'fills': [], 
            'pnl': 0.0, 
            'maker_rebates': 0.0, 
            'taker_fees': 0.0,
            'buys': 0,
            'sells': 0,
            'captured_spread_usd': 0.0
        })

        for fill in fills:
            coin = fill['coin']
            side = fill['dir']
            px = float(fill['px'])
            sz = float(fill['sz'])
            fee = float(fill.get('fee', 0))
            is_maker = fee < 0 # Maker rebates are negative numbers in HL API
            
            stats = coin_stats[coin]
            stats['fills'].append(fill)
            
            if is_maker:
                stats['maker_rebates'] += abs(fee)
            else:
                stats['taker_fees'] += fee
                
            if "Long" in side and "Open" in side or "Short" in side and "Close" in side:
                stats['buys'] += 1
            else:
                stats['sells'] += 1

        # Process per coin
        for coin, stats in sorted(coin_stats.items(), key=lambda x: len(x[1]['fills']), reverse=True):
            print(f"--- {coin} ({len(stats['fills'])} fills) ---")
            
            # Simple PnL calculation (Realized)
            positions = {'sz': 0.0, 'cost': 0.0}
            realized_pnl = 0.0
            
            for fill in stats['fills']:
                side = fill['dir']
                px = float(fill['px'])
                sz = float(fill['sz'])
                fee = float(fill.get('fee', 0))
                
                if side in ["Open Long", "Open Short"]:
                    positions['sz'] += sz
                    positions['cost'] += (px * sz)
                elif side in ["Close Long", "Close Short"]:
                    if positions['sz'] > 0:
                        avg_entry = positions['cost'] / positions['sz']
                        pnl = (px - avg_entry) * sz if side == "Close Long" else (avg_entry - px) * sz
                        realized_pnl += pnl
                        positions['sz'] -= sz
                        positions['cost'] -= (avg_entry * sz)
                elif side in ["Long > Short", "Short > Long"]:
                    if positions['sz'] > 0:
                        avg_entry = positions['cost'] / positions['sz']
                        close_sz = positions['sz']
                        pnl = (px - avg_entry) * close_sz if side == "Long > Short" else (avg_entry - px) * close_sz
                        realized_pnl += pnl
                        remain_sz = sz - close_sz
                        positions['sz'] = remain_sz
                        positions['cost'] = px * remain_sz
                    else:
                        positions['sz'] += sz
                        positions['cost'] += px * sz
            
            total_fee_impact = stats['taker_fees'] - stats['maker_rebates']
            net_profit = realized_pnl - total_fee_impact
            
            print(f"  Activity      : {stats['buys']} Buys | {stats['sells']} Sells")
            print(f"  Maker Rebates : +${stats['maker_rebates']:.4f}")
            print(f"  Taker Fees    : -${stats['taker_fees']:.4f}")
            print(f"  Raw Trade PnL :  ${realized_pnl:.4f}")
            print(f"  Net Profit    :  ${net_profit:.4f} {'✅' if net_profit > 0 else '❌'}")
            
            # Explain the algo for this coin
            if len(stats['fills']) >= 4:
                print("  Algo Behavior:")
                # Analyze time between fills
                times = [f['time'] for f in stats['fills']]
                avg_gap = (max(times) - min(times)) / (len(times) - 1) / 1000
                print(f"    - Frequency: Average fill every {avg_gap:.1f}s")
                
                # Check for "Spread Harvesting" (Tight buy-sell-buy-sell patterns)
                has_oscillation = False
                dirs = [f['dir'] for f in stats['fills']]
                oscillations = 0
                for i in range(len(dirs)-1):
                    if ("Long" in dirs[i] and "Short" in dirs[i+1]) or ("Short" in dirs[i] and "Long" in dirs[i+1]):
                        oscillations += 1
                
                if oscillations > len(dirs) / 2:
                    print(f"    - Strategy : PURE MARKET MAKING (Spread Harvesting detected - {oscillations} flips)")
                else:
                    print(f"    - Strategy : INVENTORY SKEWING (Accumulating/Distributing with the trend)")
            print()

    except Exception as e:
        print(f"Analysis failed: {e}")

if __name__ == "__main__":
    analyze()
