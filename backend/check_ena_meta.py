import requests

resp = requests.post("https://api.hyperliquid.xyz/info", json={"type": "meta"})
data = resp.json()
for i, asset in enumerate(data["universe"]):
    if asset["name"] == "ENA":
        print(f"Index: {i}")
        print(asset)
        break
