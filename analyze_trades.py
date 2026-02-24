
import re
from collections import defaultdict

def analyze_trades(file_path):
    fills = []
    with open(file_path, 'r') as f:
        for line in f:
            match = re.search(r'\[PRIVATE FILL\] (\w+) (A|B) ([\d\.]+) @ ([\d\.]+)', line)
            if match:
                coin, side, sz, px = match.groups()
                fills.append({
                    'coin': coin,
                    'side': side,
                    'sz': float(sz),
                    'px': float(px)
                })
    
    inventory = defaultdict(float) # coin -> balance
    cost_basis = defaultdict(float) # coin -> avg_px
    trade_results = [] # list of profits
    
    for fill in fills:
        coin = fill['coin']
        px = fill['px']
        sz = fill['sz']
        side = fill['side']
        
        if side == 'B': # BUY
            if inventory[coin] < 0: # Closing a short
                realized_sz = min(abs(inventory[coin]), sz)
                profit = (cost_basis[coin] - px) * realized_sz
                trade_results.append(profit)
                inventory[coin] += sz
            else: # Opening or adding to long
                old_sz = inventory[coin]
                inventory[coin] += sz
                cost_basis[coin] = (cost_basis[coin] * old_sz + px * sz) / inventory[coin]
        else: # SELL (ASK)
            if inventory[coin] > 0: # Closing a long
                realized_sz = min(inventory[coin], sz)
                profit = (px - cost_basis[coin]) * realized_sz
                trade_results.append(profit)
                inventory[coin] -= sz
            else: # Opening or adding to short
                old_sz = abs(inventory[coin])
                inventory[coin] -= sz
                cost_basis[coin] = (cost_basis[coin] * old_sz + px * sz) / abs(inventory[coin])

    total_trades = len(trade_results)
    winners = [p for p in trade_results if p > 0]
    losers = [p for p in trade_results if p <= 0]
    
    print(f"Total Completed Trade Segments: {total_trades}")
    print(f"Winners: {len(winners)}")
    print(f"Losers: {len(losers)}")
    if total_trades > 0:
        print(f"Win Rate: {len(winners)/total_trades:.1%}")

if __name__ == "__main__":
    analyze_trades('latest_fills.txt')
