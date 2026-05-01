use std::collections::VecDeque;
use std::time::Instant;

const MAX_PRICE: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Yes,
    No,
}

#[derive(Debug, Clone)]
pub struct BookSide {
    pub quantities: [i64; MAX_PRICE],
    pub occupied: u128,
    pub best_bid: i64,
    pub total_qty: i64,
}

impl BookSide {
    pub fn new() -> Self {
        Self {
            quantities: [0; MAX_PRICE],
            occupied: 0,
            best_bid: 0,
            total_qty: 0,
        }
    }

    #[inline(always)]
    fn recompute_best_bid(&mut self) {
        if self.occupied == 0 {
            self.best_bid = 0;
        } else {
            self.best_bid = (127 - self.occupied.leading_zeros()) as i64;
        }
    }

    pub fn apply_snapshot(&mut self, levels: &[(i64, i64)]) {
        self.quantities = [0; MAX_PRICE];
        self.occupied = 0;
        self.total_qty = 0;
        for &(price, qty) in levels {
            if price >= 1 && price <= 99 {
                self.quantities[price as usize] = qty;
                self.total_qty += qty;
                if qty > 0 {
                    self.occupied |= 1u128 << price;
                }
            }
        }
        self.recompute_best_bid();
    }

    #[inline(always)]
    pub fn apply_delta(&mut self, price: i64, delta: i64) {
        if price < 1 || price > 99 {
            return;
        }
        let idx = price as usize;
        let old_qty = self.quantities[idx];
        let new_qty = (old_qty + delta).max(0);
        self.quantities[idx] = new_qty;
        self.total_qty += new_qty - old_qty;

        if new_qty > 0 {
            self.occupied |= 1u128 << idx;
        } else {
            self.occupied &= !(1u128 << idx);
        }

        if new_qty == 0 && price == self.best_bid {
            self.recompute_best_bid();
        } else if new_qty > 0 && price > self.best_bid {
            self.best_bid = price;
        }
    }

    #[inline(always)]
    pub fn best_bid_qty(&self) -> i64 {
        if self.best_bid >= 1 && self.best_bid <= 99 {
            self.quantities[self.best_bid as usize]
        } else {
            0
        }
    }

    #[inline(always)]
    pub fn best_bid_below(&self, below_price: i64) -> i64 {
        if below_price <= 1 {
            return 0;
        }
        let mask = self.occupied & ((1u128 << below_price) - 1);
        if mask == 0 {
            0
        } else {
            (127 - mask.leading_zeros()) as i64
        }
    }

    pub fn top_n(&self, n: usize) -> Vec<(i64, i64)> {
        let mut result = Vec::with_capacity(n);
        let mut remaining = self.occupied;
        for _ in 0..n {
            if remaining == 0 {
                break;
            }
            let p = (127 - remaining.leading_zeros()) as usize;
            result.push((p as i64, self.quantities[p]));
            remaining &= !(1u128 << p);
        }
        result
    }
}

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub yes_bids: BookSide,
    pub no_bids: BookSide,
    pub last_update: Instant,
    pub seq: u64,
    pub initialized: bool,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            yes_bids: BookSide::new(),
            no_bids: BookSide::new(),
            last_update: Instant::now(),
            seq: 0,
            initialized: false,
        }
    }

    pub fn apply_snapshot(
        &mut self,
        yes_levels: &[(i64, i64)],
        no_levels: &[(i64, i64)],
        seq: u64,
    ) {
        self.yes_bids.apply_snapshot(yes_levels);
        self.no_bids.apply_snapshot(no_levels);
        self.seq = seq;
        self.last_update = Instant::now();
        self.initialized = true;
    }

    #[inline(always)]
    pub fn apply_delta(&mut self, side: Side, price: i64, delta: i64, seq: u64) -> bool {
        if seq != self.seq + 1 {
            tracing::warn!(
                expected = self.seq + 1,
                got = seq,
                "Sequence gap detected — book state unreliable"
            );
            return false;
        }
        match side {
            Side::Yes => self.yes_bids.apply_delta(price, delta),
            Side::No => self.no_bids.apply_delta(price, delta),
        }
        self.seq = seq;
        self.last_update = Instant::now();
        true
    }

    #[inline(always)]
    pub fn best_yes_bid(&self) -> i64 {
        self.yes_bids.best_bid
    }

    #[inline(always)]
    pub fn best_yes_ask(&self) -> i64 {
        if self.no_bids.best_bid > 0 {
            100 - self.no_bids.best_bid
        } else {
            0
        }
    }

    #[inline(always)]
    pub fn best_no_bid(&self) -> i64 {
        self.no_bids.best_bid
    }

    #[inline(always)]
    pub fn best_no_ask(&self) -> i64 {
        if self.yes_bids.best_bid > 0 {
            100 - self.yes_bids.best_bid
        } else {
            0
        }
    }

    pub fn yes_spread(&self) -> i64 {
        let ask = self.best_yes_ask();
        let bid = self.best_yes_bid();
        if ask > 0 && bid > 0 {
            ask - bid
        } else {
            i64::MAX
        }
    }

    pub fn midprice(&self) -> Option<f64> {
        let bid = self.best_yes_bid();
        let ask = self.best_yes_ask();
        if bid > 0 && ask > 0 && ask > bid {
            Some((bid as f64 + ask as f64) / 2.0)
        } else {
            None
        }
    }

    pub fn microprice(&self) -> Option<f64> {
        let bid = self.best_yes_bid();
        let ask = self.best_yes_ask();
        if bid <= 0 || ask <= 0 || ask <= bid {
            return None;
        }
        let bid_qty = self.yes_bids.best_bid_qty() as f64;
        let ask_qty = self.no_bids.best_bid_qty() as f64;
        if bid_qty + ask_qty == 0.0 {
            return self.midprice();
        }
        Some((bid as f64 * ask_qty + ask as f64 * bid_qty) / (bid_qty + ask_qty))
    }

    pub fn imbalance_ratio(&self) -> Option<f64> {
        let bid_qty = self.yes_bids.best_bid_qty() as f64;
        let ask_qty = self.no_bids.best_bid_qty() as f64;
        if ask_qty > 0.0 {
            Some(bid_qty / ask_qty)
        } else if bid_qty > 0.0 {
            Some(f64::MAX)
        } else {
            None
        }
    }

    pub fn near_bbo_depth(&self, range: i64) -> i64 {
        let mut total = 0i64;

        let yb = self.yes_bids.best_bid;
        if yb > 0 {
            let lo = (yb - range).max(1) as usize;
            let hi = yb as usize;
            for p in lo..=hi {
                total += self.yes_bids.quantities[p];
            }
        }

        let nb = self.no_bids.best_bid;
        if nb > 0 {
            let lo = (nb - range).max(1) as usize;
            let hi = nb as usize;
            for p in lo..=hi {
                total += self.no_bids.quantities[p];
            }
        }

        total
    }
}

#[derive(Debug, Clone)]
pub enum BookEvent {
    Updated,
    SequenceGap { expected: u64, got: u64 },
    TopOfBookChanged {
        old_bid: i64,
        old_ask: i64,
        new_bid: i64,
        new_ask: i64,
    },
}

pub struct BookEventDetector {
    prev_yes_bid: i64,
    prev_yes_ask: i64,
    warmed_up: bool,
}

impl BookEventDetector {
    pub fn new() -> Self {
        Self {
            prev_yes_bid: 0,
            prev_yes_ask: 0,
            warmed_up: false,
        }
    }

    pub fn detect(&mut self, book: &OrderBook) -> Vec<BookEvent> {
        let new_bid = book.best_yes_bid();
        let new_ask = book.best_yes_ask();

        if !self.warmed_up {
            self.prev_yes_bid = new_bid;
            self.prev_yes_ask = new_ask;
            self.warmed_up = true;
            return Vec::new();
        }

        let mut events = Vec::new();

        if new_bid != self.prev_yes_bid || new_ask != self.prev_yes_ask {
            events.push(BookEvent::TopOfBookChanged {
                old_bid: self.prev_yes_bid,
                old_ask: self.prev_yes_ask,
                new_bid,
                new_ask,
            });
        }

        self.prev_yes_bid = new_bid;
        self.prev_yes_ask = new_ask;

        events
    }
}

pub struct PriceHistory {
    entries: VecDeque<(Instant, f64)>,
    max_window_secs: f64,
}

impl PriceHistory {
    pub fn new(max_window_secs: f64) -> Self {
        Self {
            entries: VecDeque::with_capacity(4096),
            max_window_secs,
        }
    }

    pub fn push(&mut self, now: Instant, mid: f64) {
        self.entries.push_back((now, mid));
        while let Some(&(t, _)) = self.entries.front() {
            if now.duration_since(t).as_secs_f64() > self.max_window_secs {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn z_score(&self, window_secs: f64, now: Instant) -> Option<f64> {
        let cutoff_dur = std::time::Duration::from_secs_f64(window_secs);
        let cutoff = now.checked_sub(cutoff_dur)?;

        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        let mut count = 0u64;
        let mut latest_mid = 0.0;
        let mut oldest_t = now;

        for &(t, mid) in self.entries.iter().rev() {
            if t < cutoff {
                break;
            }
            sum += mid;
            sum_sq += mid * mid;
            count += 1;
            latest_mid = mid;
            oldest_t = t;
        }

        if count < 10 {
            return None;
        }

        let span = now.duration_since(oldest_t).as_secs_f64();
        if span < window_secs * 0.5 {
            return None;
        }

        let mean = sum / count as f64;
        let variance = (sum_sq / count as f64) - (mean * mean);
        let std_dev = variance.max(0.0).sqrt().max(0.1);

        Some((latest_mid - mean) / std_dev)
    }
}