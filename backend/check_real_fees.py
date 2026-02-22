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
        print(f"Showing last 10 fills for {user}:")
        for fill in res[:10]:
            ts = datetime.fromtimestamp(fill['time'] / 1000.0).strftime('%Y-%m-%d %H:%M:%S')
            fee = float(fill.get('fee', 0))
            is_maker = "(MAKER REBATE)" if fee < 0 else "(TAKER FEE)"
            print(f"[{ts}] {fill['coin']} {fill['dir']} Px: {fill['px']} Sz: {fill['sz']} Fee: {fee:.6f} {is_maker}")
    else:
        print(f"Error: {res}")
except Exception as e:
    print(f"Exception: {e}")
