import random

def run_monte_carlo(initial_capital, win_rate, reward_per_trade_pct, risk_per_trade_pct, max_leverage, min_order_val, trades_per_sim, num_sims):
    ruins = 0
    successes_2pct_daily = 0  # Assuming 3 trades a day, 500 trades is ~166 days. 2% daily means compounding 166 times.
    
    # 2% daily target over trades_per_sim (assuming 3 trades/day -> target = initial * (1.02)^(trades/3))
    target_balance = initial_capital * (1.02 ** (trades_per_sim / 3)) 
    
    final_balances = []
    
    for _ in range(num_sims):
        balance = initial_capital
        ruined = False
        
        for _ in range(trades_per_sim):
            # Dynamic position sizing (risk 2% of account if possible)
            # Position = Account * Leverage / Max_positions. Let's assume Max_positions = 1.
            # But the max risk we want is `risk_per_trade_pct`.
            # If SL is 1%, and we risk 2% of account, position size = Account * 2
            
            # Simple assumption: we use 2x leverage.
            # So win means + (2 * reward_per_trade_pct) * balance
            # Loss means - (2 * risk_per_trade_pct) * balance
            
            # If position size < 12, we must boost leverage up to 20x to reach $12.
            # If balance * 20 < 12, we are ruined.
            
            if balance * max_leverage < min_order_val:
                ruined = True
                break
                
            # Desired position size based on 2x leverage
            pos_size = balance * 2.0
            if pos_size < min_order_val:
                pos_size = min_order_val # Boost to minimum
            
            # Check if this forced position exceeds max leverage
            if pos_size / balance > max_leverage:
                ruined = True
                break
            
            # Simulate Trade
            if random.random() < win_rate:
                # Win
                balance += pos_size * reward_per_trade_pct
            else:
                # Loss
                balance -= pos_size * risk_per_trade_pct
                
            if balance < 1.0: # Hard ruin
                ruined = True
                break
                
        if ruined:
            ruins += 1
            final_balances.append(0)
        else:
            final_balances.append(balance)
            if balance >= target_balance:
                successes_2pct_daily += 1
                
    ruin_prob = (ruins / num_sims) * 100
    success_2pct_prob = (successes_2pct_daily / num_sims) * 100
    avg_final = sum(final_balances) / len(final_balances) if final_balances else 0
    sorted_bals = sorted(final_balances)
    n = len(sorted_bals)
    if n == 0:
        median_final = 0
    elif n % 2 == 1:
        median_final = sorted_bals[n//2]
    else:
        median_final = (sorted_bals[n//2 - 1] + sorted_bals[n//2]) / 2
    
    return {
        "ruin_prob": ruin_prob,
        "success_2pct_prob": success_2pct_prob,
        "avg_final": avg_final,
        "median_final": median_final,
        "target_balance": target_balance
    }

print("Monte Carlo Simulation: 10,000 runs, 9,000 trades each (approx 6 months @ 50 trades/day)")
print("Assumptions: 45% Win Rate, 1.5% TP (unleveraged), 1.0% SL (unleveraged), $12 Minimum Trade Notional")
print("-" * 50)

for starting_cap in [6.0, 100.0, 500.0, 1000.0]:
    stats = run_monte_carlo(
        initial_capital=starting_cap, 
        win_rate=0.45,  # Realistic Win Rate
        reward_per_trade_pct=0.015, # 1.5% move
        risk_per_trade_pct=0.010,   # 1.0% move
        max_leverage=20.0, 
        min_order_val=12.0, 
        trades_per_sim=9000, 
        num_sims=10000
    )
    
    print(f"Initial Capital: ${starting_cap:.2f}")
    print(f"  Risk of Ruin (0 balance): {stats['ruin_prob']:.2f}%")
    print(f"  Probability of hitting 2% Daily Compounding Target (${stats['target_balance']:,.2f}): {stats['success_2pct_prob']:.2f}%")
    print(f"  Expected Average Balance after 6 Mos: ${stats['avg_final']:,.2f}")
    print(f"  Expected Median Balance after 6 Mos: ${stats['median_final']:,.2f}")
    print("-" * 50)
