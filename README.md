# Kalshi Market Maker (v7) - Z-Score Signal Engine

Statistical arbitrage bot built in Rust for the Kalshi prediction market. Version 7 completely overhauls the pricing strategy, replacing static spread logic with a dynamic `SignalEngine` that tracks real-time price volatility using rolling Z-scores to identify momentum and mean-reversion opportunities.

## Core Strategy: The Signal Engine
The bot continuously tracks the midprice over two rolling time windows (e.g., 15s and 60s) to calculate real-time Z-scores. Based on these standard deviations, the market is classified into three regimes:

*   **Calm Regime:** The baseline state. The bot provides liquidity using standard laddered orders to capture the bid-ask spread. It utilizes soft inventory caps to selectively cancel orders on one side to prevent over-accumulation.
*   **Stretched Regime (Gear 1):** Triggered when the short-term Z-score breaches the `z_gear1_threshold`. The bot identifies a trend and places maker orders in the direction of the momentum to catch the wave. 
*   **Extreme Regime (Gear 2):** Triggered when the Z-score violently breaches the `z_gear2_threshold`. If the long-term trend is stable (filtered by `z60_noise_cap`), the bot aggressively crosses the spread with Taker IOC orders to capture the dislocation.

## Performance & Architecture Upgrades
*   **Price Banding & Spread EMAs:** Kalshi contracts trade differently at 10¢ versus 50¢. The bot calculates an Exponential Moving Average (EMA) of the spread specific to 20¢ price bands (e.g., "0-20", "20-40") to adjust its aggression accordingly.
*   **Signal Timeouts & Ghost Order Protection:** Momentum signals are strictly timed. If a `Gear 1` or `Gear 2` event isn't resolved within `signal_timeout_secs`, or if the Z-score crosses back over zero, the signal is aborted to prevent stale executions.
*   **Advanced Expiry Guards:** Introduces strict dual-phase shutdown timers: `no_open_before_close_secs` prevents opening new legs right before expiry (but allows closing existing ones), while `no_trade_before_close_secs` completely halts the bot and cancels all orders.

## Prerequisites
*   Rust and Cargo installed.
*   Kalshi API access (Key ID and Private Key).
