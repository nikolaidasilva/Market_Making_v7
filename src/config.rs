use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key_id: String,
    pub private_key_pem: String,
    pub rest_base_url: String,
    pub ws_url: String,
    pub series_ticker: String,

    pub order_size: i64,

    pub max_inventory: i64,
    pub hard_inventory_cap: i64,

    pub taker_enabled: bool,
    pub daily_loss_cap_cents: i64,

    pub rate_limit_writes_per_sec: u32,
    pub quote_refresh_interval: Duration,
    pub rest_timeout: Duration,
    pub market_check_interval: Duration,
    pub market_rotate_before_close_secs: i64,

    pub z_score_window_secs: f64,
    pub z60_window_secs: f64,
    pub z_gear1_threshold: f64,
    pub z_gear2_threshold: f64,
    pub z60_noise_cap: f64,
    pub signal_timeout_secs: f64,
    pub ladder_levels: usize,
    pub gear2_enabled: bool,
    pub no_open_before_close_secs: i64,
    pub no_trade_before_close_secs: i64,
    pub spread_max_for_gear2: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            api_key_id: String::new(),
            private_key_pem: String::new(),
            rest_base_url: "https://api.elections.kalshi.com/trade-api/v2".into(),
            ws_url: "wss://api.elections.kalshi.com/trade-api/ws/v2".into(),
            series_ticker: "KXBTC15M".into(),

            order_size: 1,

            max_inventory: 5,
            hard_inventory_cap: 7,

            taker_enabled: true,
            daily_loss_cap_cents: 500,

            rate_limit_writes_per_sec: 28,
            quote_refresh_interval: Duration::from_millis(5),
            rest_timeout: Duration::from_millis(200),
            market_check_interval: Duration::from_secs(30),
            market_rotate_before_close_secs: 210,

            z_score_window_secs: 13.0,
            z60_window_secs: 60.0,
            z_gear1_threshold: 1.2,
            z_gear2_threshold: 2.0,
            z60_noise_cap: 1.0,
            signal_timeout_secs: 30.0,
            ladder_levels: 2,
            gear2_enabled: false,
            no_open_before_close_secs: 210,
            no_trade_before_close_secs: 240,
            spread_max_for_gear2: 1.5,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("SERIES_TICKER") { c.series_ticker = v; }
        if let Ok(v) = std::env::var("ORDER_SIZE") { if let Ok(n) = v.parse() { c.order_size = n; } }
        if let Ok(v) = std::env::var("MAX_INVENTORY") { if let Ok(n) = v.parse() { c.max_inventory = n; } }
        if let Ok(v) = std::env::var("HARD_INVENTORY_CAP") { if let Ok(n) = v.parse() { c.hard_inventory_cap = n; } }
        if let Ok(v) = std::env::var("TAKER_ENABLED") { c.taker_enabled = v != "0" && v != "false"; }
        if let Ok(v) = std::env::var("DAILY_LOSS_CAP") { if let Ok(n) = v.parse() { c.daily_loss_cap_cents = n; } }
        if let Ok(v) = std::env::var("RATE_LIMIT_WPS") { if let Ok(n) = v.parse() { c.rate_limit_writes_per_sec = n; } }
        if let Ok(v) = std::env::var("QUOTE_REFRESH_MS") { if let Ok(n) = v.parse::<u64>() { c.quote_refresh_interval = Duration::from_millis(n); } }
        if let Ok(v) = std::env::var("REST_TIMEOUT_MS") { if let Ok(n) = v.parse::<u64>() { c.rest_timeout = Duration::from_millis(n); } }
        if let Ok(v) = std::env::var("ROTATE_BEFORE_CLOSE_SECS") { if let Ok(n) = v.parse() { c.market_rotate_before_close_secs = n; } }
        if let Ok(v) = std::env::var("REST_BASE_URL") { c.rest_base_url = v; }
        if let Ok(v) = std::env::var("WS_URL") { c.ws_url = v; }
        if let Ok(v) = std::env::var("Z_SCORE_WINDOW_SECS") { if let Ok(n) = v.parse() { c.z_score_window_secs = n; } }
        if let Ok(v) = std::env::var("Z60_WINDOW_SECS") { if let Ok(n) = v.parse() { c.z60_window_secs = n; } }
        if let Ok(v) = std::env::var("Z_GEAR1_THRESHOLD") { if let Ok(n) = v.parse() { c.z_gear1_threshold = n; } }
        if let Ok(v) = std::env::var("Z_GEAR2_THRESHOLD") { if let Ok(n) = v.parse() { c.z_gear2_threshold = n; } }
        if let Ok(v) = std::env::var("Z60_NOISE_CAP") { if let Ok(n) = v.parse() { c.z60_noise_cap = n; } }
        if let Ok(v) = std::env::var("SIGNAL_TIMEOUT_SECS") { if let Ok(n) = v.parse() { c.signal_timeout_secs = n; } }
        if let Ok(v) = std::env::var("LADDER_LEVELS") { if let Ok(n) = v.parse() { c.ladder_levels = n; } }
        if let Ok(v) = std::env::var("GEAR2_ENABLED") { c.gear2_enabled = v != "0" && v != "false"; }
        if let Ok(v) = std::env::var("NO_OPEN_BEFORE_CLOSE_SECS") { if let Ok(n) = v.parse() { c.no_open_before_close_secs = n; } }
        if let Ok(v) = std::env::var("NO_TRADE_BEFORE_CLOSE_SECS") { if let Ok(n) = v.parse() { c.no_trade_before_close_secs = n; } }
        if let Ok(v) = std::env::var("SPREAD_MAX_FOR_GEAR2") { if let Ok(n) = v.parse() { c.spread_max_for_gear2 = n; } }
        c
    }
}