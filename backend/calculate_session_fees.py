import requests
from datetime import datetime

url = "https://api.hyperliquid.xyz/info"
user = "0x6d5Ca0480Eaf4862d0E174B58B72431c4d01efD3"
data = {"type": "userFills", "user": user}

try:
    res = requests.post(url, json=data).json()
    if isinstance(res, list):
        total_maker_fees = 0.0
        total_taker_fees = 0.0
        maker_count = 0
        taker_count = 0
        
        # Hyperliquid fee schedule:
        # Maker: 1.44 bps (0.000144)
        # Taker: 3.50 bps (0.000350)
        
        for fill in res:
            fee = float(fill.get('fee', 0))
            px = float(fill.get('px', 0))
            sz = float(fill.get('sz', 0))
            notional = px * sz
            
            if notional > 0:
                fee_bps = (fee / notional) * 10000
                # We classify by proximity to fee tiers
                if abs(fee_bps - 1.44) < 0.5:
                    total_maker_fees += fee
                    maker_count += 1
                else:
                    total_taker_fees += fee
                    taker_count += 1
        
        print(f"--- FEE ANALYSIS (Session Total) ---")
        print(f"Total Maker Fees: ${total_maker_fees:.4f} ({maker_count} fills)")
        print(f"Total Taker Fees: ${total_taker_fees:.4f} ({taker_count} fills)")
        print(f"Grand Total Fees: ${total_maker_fees + total_taker_fees:.4f}")
        print(f"------------------------------------")
    else:
        print(f"Error: {res}")
except Exception as e:
    print(f"Exception: {e}")
