import requests
import json
import os
import time
from eth_account import Account
import hmac
import hashlib

# This script closes ALL open positions on Hyperliquid for the given account.
# It uses a Taker order (Market) to ensure immediate closure and margin release.

API_URL = "https://api.hyperliquid.xyz/info"
EXCHANGE_URL = "https://api.hyperliquid.xyz/exchange"
USER_ADDRESS = "0x6d5Ca0480Eaf4862d0E174B58B72431c4d01efD3"
# Private key should be in environment or handled securely. 
# Since I don't have it directly, I'll assume the environment has it or use the existing wallet logic if possible.
# Actually, I can't sign without the key.
# But wait, I have the Rust engine which DOES have the key in the container.

# Alternative: Add a 'flush' command to the Rust engine or a 'flush' mode.
