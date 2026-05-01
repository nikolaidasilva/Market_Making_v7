use std::collections::HashMap;
use std::time::Instant;

use crate::config::Config;
use crate::execution::{OrderStatus, OrderTracker};
use crate::market_data::OrderBook;
use crate::risk::RiskManager;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    Calm,
    Stretched,
    Extreme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZDirection {
    Up,
    Down,
}

#[derive(Debug)]
pub enum StrategyAction {
    PlaceBid { price_cents: i64, count: i64 },
    PlaceAsk { price_cents: i64, count: i64 },
    CancelOrder { order_id: String },
    CancelAll,
    TakeYes { price: i64, count: i64 },
    TakeNo { price: i64, count: i64 },
}

pub fn price_band(mid: f64) -> &'static str {
    if mid < 20.0 { "0-20" }
    else if mid < 40.0 { "20-40" }
    else if mid < 60.0 { "40-60" }
    else if mid < 80.0 { "60-80" }
    else { "80-100" }
}

#[derive(Debug)]
pub struct SignalEvent {
    pub started_at: Instant,
    pub entry_mid: f64,
    pub gear: u8,
    pub band: &'static str,
    pub resolved: bool,
}

pub struct SignalEngine {
    active_yes_event: Option<SignalEvent>,
    active_no_event: Option<SignalEvent>,
    spread_emas: HashMap<&'static str, f64>,
    cfg_gear1: f64,
    cfg_gear2: f64,
    cfg_signal_timeout: f64,
    cfg_ladder_levels: usize,
    cfg_gear2_enabled: bool,
    cfg_no_open_before_close: i64,
    cfg_no_trade_before_close: i64,
    cfg_spread_max_gear2: f64,
    cfg_order_size: i64,
    cfg_max_inventory: i64,
    cfg_hard_inventory_cap: i64,
    cfg_z60_noise_cap: f64,
}

impl SignalEngine {
    pub fn new(config: &Config) -> Self {
        Self {
            active_yes_event: None,
            active_no_event: None,
            spread_emas: HashMap::new(),
            cfg_gear1: config.z_gear1_threshold,
            cfg_gear2: config.z_gear2_threshold,
            cfg_signal_timeout: config.signal_timeout_secs,
            cfg_ladder_levels: config.ladder_levels,
            cfg_gear2_enabled: config.gear2_enabled,
            cfg_no_open_before_close: config.no_open_before_close_secs,
            cfg_no_trade_before_close: config.no_trade_before_close_secs,
            cfg_spread_max_gear2: config.spread_max_for_gear2,
            cfg_order_size: config.order_size,
            cfg_max_inventory: config.max_inventory,
            cfg_hard_inventory_cap: config.hard_inventory_cap,
            cfg_z60_noise_cap: config.z60_noise_cap,
        }
    }

    fn classify_regime(&self, z: f64) -> Regime {
        let abs_z = z.abs();
        if abs_z >= self.cfg_gear2 { Regime::Extreme }
        else if abs_z >= self.cfg_gear1 { Regime::Stretched }
        else { Regime::Calm }
    }

    fn z_direction(z: f64) -> ZDirection {
        if z >= 0.0 { ZDirection::Up } else { ZDirection::Down }
    }

    fn update_spread_ema(&mut self, band: &'static str, spread: f64) {
        let ema = self.spread_emas.entry(band).or_insert(spread);
        *ema = 0.05 * spread + 0.95 * *ema;
    }

    fn avg_spread(&self, band: &'static str) -> f64 {
        self.spread_emas.get(band).copied().unwrap_or(1.0)
    }

    fn band_allows_gear2(&self, band: &'static str) -> bool {
        if !self.cfg_gear2_enabled { return false; }
        match band {
            "20-40" => false,
            "0-20" | "40-60" => true,
            "60-80" | "80-100" => self.avg_spread(band) < self.cfg_spread_max_gear2,
            _ => false,
        }
    }

    fn ladder_levels_for_band(&self, band: &'static str) -> usize {
        if band == "20-40" { 1 } else { self.cfg_ladder_levels }
    }

    fn z60_is_noise(&self, z60: Option<f64>) -> bool {
        match z60 {
            Some(z) => z.abs() < self.cfg_z60_noise_cap,
            None => true,
        }
    }

    fn resolve_events(
        &mut self, z: f64, now: Instant, actions: &mut Vec<StrategyAction>,
        book: &OrderBook, ticker: &str, tracker: &OrderTracker,
    ) {
        let timeout = self.cfg_signal_timeout;

        if let Some(ref mut ev) = self.active_yes_event {
            if !ev.resolved {
                let elapsed = now.duration_since(ev.started_at).as_secs_f64();
                let timed_out = elapsed >= timeout;
                let z_crossed_zero = z >= 0.0;
                let ghost = ev.gear == 1
                    && count_resting_side(tracker, ticker, "yes") == 0
                    && elapsed > 1.0;
                if z_crossed_zero || timed_out || ghost {
                    ev.resolved = true;
                    if ev.gear == 2 {
                        let no_ask = book.best_no_ask();
                        if no_ask > 0 && no_ask <= 99 {
                            actions.push(StrategyAction::TakeNo { price: no_ask, count: 1 });
                        }
                    }
                }
            }
        }
        if self.active_yes_event.as_ref().map(|e| e.resolved).unwrap_or(false) {
            self.active_yes_event = None;
        }

        if let Some(ref mut ev) = self.active_no_event {
            if !ev.resolved {
                let elapsed = now.duration_since(ev.started_at).as_secs_f64();
                let timed_out = elapsed >= timeout;
                let z_crossed_zero = z <= 0.0;
                let ghost = ev.gear == 1
                    && count_resting_side(tracker, ticker, "no") == 0
                    && elapsed > 1.0;
                if z_crossed_zero || timed_out || ghost {
                    ev.resolved = true;
                    if ev.gear == 2 {
                        let yes_ask = book.best_yes_ask();
                        if yes_ask > 0 && yes_ask <= 99 {
                            actions.push(StrategyAction::TakeYes { price: yes_ask, count: 1 });
                        }
                    }
                }
            }
        }
        if self.active_no_event.as_ref().map(|e| e.resolved).unwrap_or(false) {
            self.active_no_event = None;
        }
    }

    pub fn tick(
        &mut self,
        book: &OrderBook,
        z15: Option<f64>,
        z60: Option<f64>,
        time_to_expiry: i64,
        risk: &RiskManager,
        tracker: &OrderTracker,
        ticker: &str,
        now: Instant,
    ) -> Vec<StrategyAction> {
        let mut actions = Vec::new();

        if !book.initialized { return actions; }

        let mid = match book.midprice() {
            Some(m) => m,
            None => return actions,
        };

        let spread = book.yes_spread();
        let spread_f = if spread == i64::MAX { 99.0 } else { spread as f64 };
        let band = price_band(mid);
        self.update_spread_ema(band, spread_f);

        if time_to_expiry < self.cfg_no_trade_before_close {
            actions.push(StrategyAction::CancelAll);
            return actions;
        }

        if time_to_expiry < self.cfg_no_open_before_close {
            cancel_all_resting(&mut actions, tracker, ticker);
            if self.active_yes_event.as_ref().map(|e| e.gear == 2).unwrap_or(false) {
                let no_ask = book.best_no_ask();
                if no_ask > 0 && no_ask <= 99 {
                    actions.push(StrategyAction::TakeNo { price: no_ask, count: 1 });
                }
                self.active_yes_event = None;
            }
            if self.active_no_event.as_ref().map(|e| e.gear == 2).unwrap_or(false) {
                let yes_ask = book.best_yes_ask();
                if yes_ask > 0 && yes_ask <= 99 {
                    actions.push(StrategyAction::TakeYes { price: yes_ask, count: 1 });
                }
                self.active_no_event = None;
            }
            return actions;
        }

        let inv = risk.inventory;
        if inv.abs() >= self.cfg_hard_inventory_cap {
            actions.push(StrategyAction::CancelAll);
            return actions;
        }

        let z = match z15 {
            Some(z) => z,
            None => {
                self.generate_calm_actions(&mut actions, book, band, inv, tracker, ticker);
                return actions;
            }
        };

        self.resolve_events(z, now, &mut actions, book, ticker, tracker);

        let regime = self.classify_regime(z);
        let direction = Self::z_direction(z);

        match regime {
            Regime::Calm => {
                self.generate_calm_actions(&mut actions, book, band, inv, tracker, ticker);
            }
            Regime::Stretched => {
                self.generate_stretched_actions(
                    &mut actions, book, band, inv, direction, tracker, ticker, now, mid,
                );
            }
            Regime::Extreme => {
                self.generate_extreme_actions(
                    &mut actions, book, band, inv, direction, z60, tracker, ticker, now, mid,
                );
            }
        }

        actions
    }

    fn generate_calm_actions(
        &self,
        actions: &mut Vec<StrategyAction>,
        book: &OrderBook,
        band: &'static str,
        inv: i64,
        tracker: &OrderTracker,
        ticker: &str,
    ) {
        let levels = self.ladder_levels_for_band(band);
        let best_bid = book.best_yes_bid();
        let best_ask = book.best_yes_ask();
        if best_bid <= 0 || best_ask <= 0 || best_ask <= best_bid { return; }
        let no_best = book.best_no_bid();

        let mut yes_count = 0usize;
        let mut no_count = 0usize;

        for o in tracker.orders.values() {
            if o.ticker != ticker
                || !matches!(o.status, OrderStatus::Resting | OrderStatus::PendingNew)
            { continue; }
            if o.side == "yes" && o.action == "buy" {
                if o.price_cents >= best_ask {
                    actions.push(StrategyAction::CancelOrder { order_id: o.order_id.clone() });
                } else {
                    yes_count += 1;
                }
            } else if o.side == "no" && o.action == "buy" {
                if (100 - o.price_cents) <= best_bid {
                    actions.push(StrategyAction::CancelOrder { order_id: o.order_id.clone() });
                } else {
                    no_count += 1;
                }
            }
        }

        let at_soft_cap_long = inv >= self.cfg_max_inventory;
        let at_soft_cap_short = inv <= -self.cfg_max_inventory;

        if at_soft_cap_long {
            cancel_side_orders(actions, tracker, ticker, "yes");
            yes_count = 0;
        }
        if at_soft_cap_short {
            cancel_side_orders(actions, tracker, ticker, "no");
            no_count = 0;
        }

        let can_open_yes = inv <= 0 && !at_soft_cap_long && self.active_yes_event.is_none();
        let can_open_no = inv >= 0 && !at_soft_cap_short && self.active_no_event.is_none();

        let can_exit_yes = inv < 0;
        let can_exit_no = inv > 0;

        if (can_open_yes || can_exit_yes) && yes_count < levels {
            let needed = levels - yes_count;
            let mut placed = 0;
            let mut price = best_bid;
            while placed < needed && price >= 1 {
                if price < best_ask {
                    actions.push(StrategyAction::PlaceBid {
                        price_cents: price, count: self.cfg_order_size,
                    });
                    placed += 1;
                }
                price -= 1;
            }
        } else if !can_open_yes && !can_exit_yes && yes_count > 0 {
            cancel_side_orders(actions, tracker, ticker, "yes");
        }

        if (can_open_no || can_exit_no) && no_count < levels && no_best > 0 {
            let needed = levels - no_count;
            let mut placed = 0;
            let mut no_price = no_best;
            while placed < needed && no_price >= 1 {
                let yes_ask_equiv = 100 - no_price;
                if yes_ask_equiv > best_bid {
                    actions.push(StrategyAction::PlaceAsk {
                        price_cents: yes_ask_equiv, count: self.cfg_order_size,
                    });
                    placed += 1;
                }
                no_price -= 1;
            }
        } else if !can_open_no && !can_exit_no && no_count > 0 {
            cancel_side_orders(actions, tracker, ticker, "no");
        }
    }

    fn generate_stretched_actions(
        &mut self,
        actions: &mut Vec<StrategyAction>,
        book: &OrderBook,
        band: &'static str,
        inv: i64,
        direction: ZDirection,
        tracker: &OrderTracker,
        ticker: &str,
        now: Instant,
        mid: f64,
    ) {
        match direction {
            ZDirection::Up => {
                if inv < self.cfg_max_inventory && self.active_yes_event.is_none() {
                    if count_resting_side(tracker, ticker, "yes") == 0 {
                        let bb = book.best_yes_bid();
                        let ba = book.best_yes_ask();
                        if bb >= 1 && bb < ba {
                            self.active_yes_event = Some(SignalEvent {
                                started_at: now, entry_mid: mid,
                                gear: 1, band, resolved: false,
                            });
                            actions.push(StrategyAction::PlaceBid {
                                price_cents: bb, count: self.cfg_order_size,
                            });
                        }
                    }
                }
            }
            ZDirection::Down => {
                if inv > -self.cfg_max_inventory && self.active_no_event.is_none() {
                    if count_resting_side(tracker, ticker, "no") == 0 {
                        let nb = book.best_no_bid();
                        if nb >= 1 {
                            let ya = 100 - nb;
                            if ya > book.best_yes_bid() {
                                self.active_no_event = Some(SignalEvent {
                                    started_at: now, entry_mid: mid,
                                    gear: 1, band, resolved: false,
                                });
                                actions.push(StrategyAction::PlaceAsk {
                                    price_cents: ya, count: self.cfg_order_size,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    fn generate_extreme_actions(
        &mut self,
        actions: &mut Vec<StrategyAction>,
        book: &OrderBook,
        band: &'static str,
        inv: i64,
        direction: ZDirection,
        z60: Option<f64>,
        tracker: &OrderTracker,
        ticker: &str,
        now: Instant,
        mid: f64,
    ) {
        if !self.band_allows_gear2(band) { return; }
        if !self.z60_is_noise(z60) { return; }

        match direction {
            ZDirection::Down => {
                if self.active_yes_event.is_some() || inv > 0 { return; }
                cancel_side_orders(actions, tracker, ticker, "yes");
                let ask = book.best_yes_ask();
                if ask > 0 && ask <= 99 {
                    self.active_yes_event = Some(SignalEvent {
                        started_at: now, entry_mid: mid,
                        gear: 2, band, resolved: false,
                    });
                    actions.push(StrategyAction::TakeYes { price: ask, count: 1 });
                }
            }
            ZDirection::Up => {
                if self.active_no_event.is_some() || inv < 0 { return; }
                cancel_side_orders(actions, tracker, ticker, "no");
                let ask = book.best_no_ask();
                if ask > 0 && ask <= 99 {
                    self.active_no_event = Some(SignalEvent {
                        started_at: now, entry_mid: mid,
                        gear: 2, band, resolved: false,
                    });
                    actions.push(StrategyAction::TakeNo { price: ask, count: 1 });
                }
            }
        }
    }
}

fn cancel_side_orders(
    actions: &mut Vec<StrategyAction>, tracker: &OrderTracker, ticker: &str, side: &str,
) {
    for o in tracker.orders.values() {
        if o.ticker == ticker && o.side == side
            && matches!(o.status, OrderStatus::Resting | OrderStatus::PendingNew)
        {
            actions.push(StrategyAction::CancelOrder { order_id: o.order_id.clone() });
        }
    }
}

fn cancel_all_resting(
    actions: &mut Vec<StrategyAction>, tracker: &OrderTracker, ticker: &str,
) {
    for o in tracker.orders.values() {
        if o.ticker == ticker
            && matches!(o.status, OrderStatus::Resting | OrderStatus::PendingNew)
        {
            actions.push(StrategyAction::CancelOrder { order_id: o.order_id.clone() });
        }
    }
}

fn count_resting_side(tracker: &OrderTracker, ticker: &str, side: &str) -> usize {
    tracker.orders.values().filter(|o| {
        o.ticker == ticker && o.side == side && o.action == "buy"
            && matches!(o.status, OrderStatus::Resting | OrderStatus::PendingNew)
    }).count()
}

#[inline]
pub fn taker_fee_cents(count: i64, price_cents: i64) -> i64 {
    let p = price_cents as f64 / 100.0;
    (0.07 * count as f64 * p * (1.0 - p)).ceil().max(1.0) as i64
}