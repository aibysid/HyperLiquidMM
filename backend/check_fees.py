import requests
import os
from dotenv import load_dotenv

load_dotenv("mm-engine-rs/.env")
user = os.environ.get("HL_ACCOUNT_ADDRESS")

url = "https://api.hyperliquid.xyz/info"
data = {"type": "userFills", "user": user}

try:
    res = requests.post(url, json=data).json()
    if isinstance(res, list):
        print(f"Found {len(res)} fills for user {user}. Showing last 10:")
        for fill in res[:10]:
            print(f"Coin: {fill['coin']}, Side: {fill['dir']}, Px: {fill['px']}, Sz: {fill['sz']}, Fee: {fill['fee']}")
    else:
        print(f"Error: {res}")
except Exception as e:
    print(f"Exception: {e}")
