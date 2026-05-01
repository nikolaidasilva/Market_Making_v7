#![allow(dead_code)]

mod auth;
mod config;
mod execution;
mod market_data;
mod market_finder;
mod risk;
mod signal;
mod ws;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use auth::Credentials;
use config::Config;
use execution::{ExecutionClient, OrderStatus, OrderTracker, TrackedOrder};
use market_data::{OrderBook, PriceHistory};
use risk::{RiskManager, RiskState};
use signal::{SignalEngine, StrategyAction};
use ws::{WsManager, WsMessage};

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kalshi_mm=info".into()),
        )
        .json()
        .init();

    let config = load_config();
    let creds = Arc::new(Credentials::load(config.api_key_id.clone(), &config.private_key_pem));

    tracing::info!(
        series = %config.series_ticker,
        max_inventory = config.max_inventory,
        hard_cap = config.hard_inventory_cap,
        order_size = config.order_size,
        taker = config.taker_enabled,
        z_window = config.z_score_window_secs,
        z60_window = config.z60_window_secs,
        gear1 = config.z_gear1_threshold,
        gear2 = config.z_gear2_threshold,
        z60_noise_cap = config.z60_noise_cap,
        ladder = config.ladder_levels,
        "Starting Kalshi market maker V7"
    );

    let mut daily_pnl_cents: i64 = 0;

    loop {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5)).tcp_nodelay(true).build()?;

        let market = match market_finder::find_current_market(&creds, &http, &config.series_ticker).await {
            Ok(m) => m,
            Err(e) => {
                tracing::error!(error = %e, "Failed to find market — retrying in 10s");
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        let ticker = market.ticker.clone();
        let close_time = market.close_time.as_ref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|dt: chrono::DateTime<chrono::FixedOffset>| dt.with_timezone(&chrono::Utc));

        let secs_left = close_time
            .map(|c| (c - chrono::Utc::now()).num_seconds().max(0))
            .unwrap_or(i64::MAX);

        if secs_left <= config.market_rotate_before_close_secs + 30 {
            tracing::info!(ticker = %ticker, secs_left, "Market too close to expiry — waiting 15s for next");
            tokio::time::sleep(Duration::from_secs(15)).await;
            continue;
        }

        tracing::info!(ticker = %ticker, title = %market.title, close_time = ?market.close_time,
            carry_pnl = daily_pnl_cents, "Trading market");

        daily_pnl_cents = market_loop(&ticker, &config, &creds, close_time, daily_pnl_cents).await;
        tracing::info!(ticker = %ticker, daily_pnl = daily_pnl_cents, "Market session ended — rotating");

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn market_loop(
    ticker: &str,
    config: &Config,
    creds: &Arc<Credentials>,
    close_time: Option<chrono::DateTime<chrono::Utc>>,
    carry_pnl: i64,
) -> i64 {
    let mut book = OrderBook::new();
    let mut risk = RiskManager::new(config);
    risk.carry_daily_pnl(carry_pnl);
    let mut tracker = OrderTracker::new();
    let mut engine = SignalEngine::new(config);
    let mut price_history_15 = PriceHistory::new(config.z_score_window_secs * 2.0);
    let mut price_history_60 = PriceHistory::new(config.z60_window_secs * 2.0);

    let rate_sem = execution::spawn_rate_limiter(config.rate_limit_writes_per_sec);
    let exec = Arc::new(ExecutionClient::new(config, creds.clone(), rate_sem));

    match exec.get_position(ticker).await {
        Ok(pos) => { risk.inventory = pos; tracing::info!(position = pos, "Initial position loaded"); }
        Err(e) => tracing::error!(error = %e, "Failed to load position — starting at 0"),
    }

    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<WsMessage>();
    let ws_handle = {
        let ws_creds = creds.clone();
        let ws_url = config.ws_url.clone();
        let ws_ticker = ticker.to_string();
        tokio::spawn(async move {
            let ws = WsManager::new(ws_url, ws_creds, ws_ticker);
            if let Err(e) = ws.run(ws_tx).await {
                tracing::error!(error = %e, "WS manager fatal error");
            }
        })
    };

    let mut ws_connected = false;
    let mut last_gc = Instant::now();
    let mut total_taker_fees_cents: i64 = 0;
    let mut bid_cross_cooldown_until: Option<Instant> = None;
    let mut ask_cross_cooldown_until: Option<Instant> = None;
    let mut halt_logged = false;
    let mut last_place_at: Option<Instant> = None;

    loop {
        let time_to_expiry = close_time
            .map(|c| (c - chrono::Utc::now()).num_seconds().max(0))
            .unwrap_or(i64::MAX);

        if time_to_expiry <= config.market_rotate_before_close_secs {
            tracing::info!(time_to_expiry, "Market closing soon — rotating");
            cancel_all(&exec, &mut tracker, ticker).await;
            ws_handle.abort();
            return risk.daily_pnl_cents;
        }

        let mut messages = Vec::new();
        match tokio::time::timeout(config.quote_refresh_interval, ws_rx.recv()).await {
            Ok(Some(msg)) => {
                messages.push(msg);
                while let Ok(msg) = ws_rx.try_recv() { messages.push(msg); }
            }
            Ok(None) => {
                tracing::error!("WS channel closed — halting");
                cancel_all(&exec, &mut tracker, ticker).await;
                ws_handle.abort();
                return risk.daily_pnl_cents;
            }
            Err(_) => {
                if last_gc.elapsed() > Duration::from_secs(10) {
                    tracker.gc(Duration::from_secs(300)); last_gc = Instant::now();
                }
                continue;
            }
        }

        let mut book_changed = false;
        let mut emergency_halt = false;

        for ws_msg in messages {
            if !ws_connected { risk.set_ws_connected(true); ws_connected = true; }

            match ws_msg {
                WsMessage::OrderbookSnapshot { market_ticker, yes_levels, no_levels, seq } => {
                    if market_ticker == ticker {
                        book.apply_snapshot(&yes_levels, &no_levels, seq);
                        risk.book_updated(); book_changed = true;
                    }
                }
                WsMessage::OrderbookDelta { market_ticker, side, price, delta, seq } => {
                    if market_ticker == ticker {
                        if !book.apply_delta(side, price, delta, seq) {
                            tracing::error!("Sequence gap — rotating");
                            risk.halt("sequence_gap");
                            cancel_all(&exec, &mut tracker, ticker).await;
                            ws_handle.abort();
                            return risk.daily_pnl_cents;
                        }
                        risk.book_updated(); book_changed = true;
                    }
                }
                WsMessage::Fill { order_id, side, yes_price, count, action, is_taker, post_position, .. } => {
                    risk.record_fill(&side, &action, yes_price, count, post_position);

                    if is_taker {
                        let fee = signal::taker_fee_cents(count, yes_price);
                        total_taker_fees_cents += fee;
                        risk.daily_pnl_cents -= fee;
                    }

                    if let Some(order) = tracker.get_mut(&order_id) {
                        order.remaining -= count;
                        if order.remaining <= 0 { order.status = OrderStatus::Executed; }
                    }

                    tracing::info!(order_id, side, yes_price, count, is_taker,
                        inventory = risk.inventory, daily_pnl = risk.daily_pnl_cents,
                        taker_fees = total_taker_fees_cents, pairs = risk.pairs_completed,
                        open_yes = risk.open_yes_count(), open_no = risk.open_no_count(), "FILL");

                    if risk.inventory.abs() >= risk.hard_inventory_cap && !emergency_halt {
                        emergency_halt = true;
                        risk.halt("hard_cap_breach_mid_batch");
                        cancel_all(&exec, &mut tracker, ticker).await;
                    }
                }
                WsMessage::UserOrder { order_id, status, .. } => {
                    let new_status = match status.as_str() {
                        "resting" => OrderStatus::Resting,
                        "canceled" => OrderStatus::Canceled,
                        "executed" => OrderStatus::Executed,
                        _ => continue,
                    };
                    tracker.update_status(&order_id, new_status);
                    if matches!(new_status, OrderStatus::Canceled | OrderStatus::Executed) {
                        tracker.remove(&order_id);
                    }
                }
                WsMessage::Error { code, message } => {
                    tracing::error!(code, message, "WS error");
                    if message.starts_with("WS disconnect") {
                        ws_connected = false; risk.set_ws_connected(false);
                        book.initialized = false;
                        cancel_all(&exec, &mut tracker, ticker).await;
                    }
                }
                _ => {}
            }
        }

        if !book.initialized { continue; }

        let now = Instant::now();

        if book_changed {
            if let Some(mid) = book.midprice() {
                price_history_15.push(now, mid);
                price_history_60.push(now, mid);
            }
        }

        let risk_state = risk.tick();

        if risk_state == RiskState::Halted {
            if !halt_logged {
                tracing::error!(
                    inventory = risk.inventory,
                    daily_pnl = risk.daily_pnl_cents,
                    ws = risk.ws_connected,
                    "Bot halted — working down inventory"
                );
                halt_logged = true;
            }

            let inv = risk.inventory;
            let inv_safe = inv.abs() <= risk.max_inventory;
            let ws_ok = risk.ws_connected;
            let book_fresh = risk.last_book_update.elapsed() < risk.max_book_staleness;
            let pnl_ok = risk.daily_pnl_cents > -risk.daily_loss_cap_cents;

            if inv_safe && ws_ok && book_fresh && pnl_ok {
                risk.reset();
                halt_logged = false;
                tracing::info!(inventory = inv, daily_pnl = risk.daily_pnl_cents,
                    "Auto-recovery — resuming trading");
            } else if ws_ok && book_fresh && book.initialized {
                if inv > 0 {
                    let resting = count_resting(&tracker, ticker, "no");
                    if resting == 0 {
                        let nb = book.best_no_bid();
                        if nb >= 1 && (100 - nb) > book.best_yes_bid() {
                            place_order(&exec, &mut tracker, ticker, "no", nb,
                                config.order_size, config.ladder_levels,
                                &mut ask_cross_cooldown_until, &mut last_place_at).await;
                        }
                    }
                } else if inv < 0 {
                    let resting = count_resting(&tracker, ticker, "yes");
                    if resting == 0 {
                        let bb = book.best_yes_bid();
                        if bb >= 1 && bb < book.best_yes_ask() {
                            place_order(&exec, &mut tracker, ticker, "yes", bb,
                                config.order_size, config.ladder_levels,
                                &mut bid_cross_cooldown_until, &mut last_place_at).await;
                        }
                    }
                }
            }

            continue;
        }

        halt_logged = false;

        let on_cooldown = last_place_at
            .map(|t| now.duration_since(t) < Duration::from_millis(500))
            .unwrap_or(false);

        let z15 = price_history_15.z_score(config.z_score_window_secs, now);
        let z60 = price_history_60.z_score(config.z60_window_secs, now);

        let actions = engine.tick(&book, z15, z60, time_to_expiry, &risk, &tracker, ticker, now);

        if !actions.is_empty() {
            let (cancels, rest): (Vec<_>, Vec<_>) = actions.into_iter().partition(|a| {
                matches!(a, StrategyAction::CancelOrder { .. } | StrategyAction::CancelAll)
            });
            let (takers, makers): (Vec<_>, Vec<_>) = rest.into_iter().partition(|a| {
                matches!(a, StrategyAction::TakeYes { .. } | StrategyAction::TakeNo { .. })
            });

            for action in cancels {
                execute_action(&exec, &mut tracker, ticker, action,
                    &mut bid_cross_cooldown_until, &mut ask_cross_cooldown_until,
                    config.ladder_levels, &mut last_place_at).await;
            }
            for action in takers {
                execute_action(&exec, &mut tracker, ticker, action,
                    &mut bid_cross_cooldown_until, &mut ask_cross_cooldown_until,
                    config.ladder_levels, &mut last_place_at).await;
            }
            if !on_cooldown {
                for action in makers {
                    let now_check = Instant::now();
                    let cross_ok = match &action {
                        StrategyAction::PlaceBid { .. } =>
                            bid_cross_cooldown_until.map(|t| now_check >= t).unwrap_or(true),
                        StrategyAction::PlaceAsk { .. } =>
                            ask_cross_cooldown_until.map(|t| now_check >= t).unwrap_or(true),
                        _ => true,
                    };
                    if cross_ok {
                        execute_action(&exec, &mut tracker, ticker, action,
                            &mut bid_cross_cooldown_until, &mut ask_cross_cooldown_until,
                            config.ladder_levels, &mut last_place_at).await;
                    }
                }
            }
        }

        if last_gc.elapsed() > Duration::from_secs(10) {
            tracker.gc(Duration::from_secs(300)); last_gc = Instant::now();
        }
    }
}

fn count_resting(tracker: &OrderTracker, ticker: &str, side: &str) -> usize {
    tracker.orders.values().filter(|o| {
        o.ticker == ticker && o.side == side && o.action == "buy"
            && matches!(o.status, OrderStatus::Resting | OrderStatus::PendingNew)
    }).count()
}

async fn place_order(
    exec: &Arc<ExecutionClient>, tracker: &mut OrderTracker, ticker: &str,
    side: &str, price: i64, count: i64, max_per_side: usize,
    cross_cooldown: &mut Option<Instant>, last_place: &mut Option<Instant>,
) {
    let resting = count_resting(tracker, ticker, side);
    if resting >= max_per_side { return; }

    match exec.place_order(ticker, side, "buy", price, count).await {
        Ok(resp) => {
            *cross_cooldown = None;
            tracker.track(TrackedOrder {
                order_id: resp.order_id.clone(), client_order_id: resp.client_order_id,
                ticker: ticker.to_string(), side: side.to_string(), action: "buy".into(),
                price_cents: price, count, remaining: count,
                status: OrderStatus::PendingNew, created_at: Instant::now(),
            });
            *last_place = Some(Instant::now());
            let log_price = if side == "no" { 100 - price } else { price };
            tracing::info!(order_id = %resp.order_id, price = log_price, side, "Order placed");
        }
        Err(e) => {
            if e.to_string().contains("Post-only cross") {
                *cross_cooldown = Some(Instant::now() + Duration::from_millis(500));
            }
            tracing::error!(error = %e, price, side, "Order failed");
        }
    }
}

async fn execute_action(
    exec: &Arc<ExecutionClient>, tracker: &mut OrderTracker, ticker: &str,
    action: StrategyAction,
    bid_cross_cooldown: &mut Option<Instant>, ask_cross_cooldown: &mut Option<Instant>,
    max_resting_per_side: usize,
    last_place: &mut Option<Instant>,
) {
    match action {
        StrategyAction::PlaceBid { price_cents, count } => {
            place_order(exec, tracker, ticker, "yes", price_cents, count,
                max_resting_per_side, bid_cross_cooldown, last_place).await;
        }
        StrategyAction::PlaceAsk { price_cents, count } => {
            let no_price = 100 - price_cents;
            place_order(exec, tracker, ticker, "no", no_price, count,
                max_resting_per_side, ask_cross_cooldown, last_place).await;
        }
        StrategyAction::TakeYes { price, count } => {
            match exec.place_taker_order(ticker, "yes", "buy", price, count, false).await {
                Ok(resp) => tracing::info!(order_id = %resp.order_id, price, "TAKER YES executed"),
                Err(e) => tracing::error!(error = %e, price, "TAKER YES failed"),
            }
        }
        StrategyAction::TakeNo { price, count } => {
            match exec.place_taker_order(ticker, "no", "buy", price, count, false).await {
                Ok(resp) => tracing::info!(order_id = %resp.order_id, price, "TAKER NO executed"),
                Err(e) => tracing::error!(error = %e, price, "TAKER NO failed"),
            }
        }
        StrategyAction::CancelOrder { order_id } => {
            if let Err(e) = exec.cancel_order(&order_id).await {
                tracing::warn!(error = %e, order_id, "Cancel failed");
            } else {
                tracker.update_status(&order_id, OrderStatus::PendingCancel);
                tracker.remove(&order_id);
            }
        }
        StrategyAction::CancelAll => {
            let ids = tracker.resting_order_ids(ticker);
            if !ids.is_empty() {
                if let Err(e) = exec.batch_cancel(&ids).await {
                    tracing::error!(error = %e, "Batch cancel failed");
                    for id in &ids { let _ = exec.cancel_order(id).await; }
                }
                for id in &ids { tracker.remove(id); }
            }
        }
    }
}

async fn cancel_all(exec: &Arc<ExecutionClient>, tracker: &mut OrderTracker, ticker: &str) {
    let ids = tracker.resting_order_ids(ticker);
    if ids.is_empty() { return; }
    tracing::info!(count = ids.len(), "Canceling all resting orders");
    for attempt in 0..3u32 {
        match exec.batch_cancel(&ids).await {
            Ok(()) => { for id in &ids { tracker.remove(id); } return; }
            Err(e) => {
                tracing::warn!(error = %e, attempt, "Cancel all failed, retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    for id in &ids { tracker.remove(id); }
}

fn load_config() -> Config {
    if let Ok(contents) = std::fs::read_to_string(".env") {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') { continue; }
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim().trim_matches('"');
                if std::env::var(key).is_err() {
                    unsafe { std::env::set_var(key, val) };
                }
            }
        }
    }

    let mut config = Config::from_env();
    config.api_key_id = std::env::var("KALSHI_KEY_ID").expect("KALSHI_KEY_ID required in .env");
    let key_path = std::env::var("KALSHI_PRIVATE_KEY_PATH").expect("KALSHI_PRIVATE_KEY_PATH required in .env");
    config.private_key_pem = std::fs::read_to_string(&key_path)
        .unwrap_or_else(|e| panic!("Failed to read private key from {}: {}", key_path, e));

    tracing::info!(key_id = %config.api_key_id, series = %config.series_ticker, "Config loaded");
    config
}