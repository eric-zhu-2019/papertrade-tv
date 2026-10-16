use crate::util::parse_dec;
use rust_decimal::Decimal;
use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: String,
    pub db_path: String,
    pub web_dir: String,
    pub feed: String,
    pub symbols: Vec<String>,
    pub quote: String,
    pub club_name: String,
    pub invite_code: String,
    pub admin_user: String,
    pub admin_password: String,
    pub start_balance: Decimal,
    pub maker_fee: Decimal,
    pub taker_fee: Decimal,
    pub latency_min_ms: u64,
    pub latency_max_ms: u64,
    pub liquidity_ttl_ms: i64,
    pub stale_ms: i64,
    pub book_depth: usize,
    pub checksum: String,
    pub require_note: bool,
    pub snapshot_secs: u64,
    pub max_open_orders: usize,
    pub orders_per_sec: f64,
}

fn var(k: &str, default: &str) -> String {
    env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Config {
        let symbols: Vec<String> = var(
            "SYMBOLS",
            "BTC/USD,ETH/USD,SOL/USD,XRP/USD,DOGE/USD,ADA/USD,LINK/USD,LTC/USD",
        )
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
        let quote = symbols[0].split('/').nth(1).unwrap_or("USD").to_string();
        for s in &symbols {
            assert!(
                s.ends_with(&format!("/{quote}")),
                "all SYMBOLS must share one quote currency ({quote}); got {s}"
            );
        }
        let dec = |k: &str, dflt: &str| parse_dec(&var(k, dflt)).unwrap_or_else(|| panic!("bad {k}"));
        Config {
            bind: var("BIND", "0.0.0.0:8080"),
            db_path: var("DB_PATH", "data/papertrade.db"),
            web_dir: var("WEB_DIR", "web"),
            feed: var("FEED", "kraken"),
            symbols,
            quote,
            club_name: var("CLUB_NAME", "Crypto Club"),
            invite_code: var("INVITE_CODE", ""),
            admin_user: var("ADMIN_USER", "admin"),
            admin_password: var("ADMIN_PASSWORD", ""),
            start_balance: dec("START_BALANCE", "100000"),
            maker_fee: dec("MAKER_FEE", "0.0025"),
            taker_fee: dec("TAKER_FEE", "0.004"),
            latency_min_ms: var("LATENCY_MIN_MS", "50").parse().expect("LATENCY_MIN_MS"),
            latency_max_ms: var("LATENCY_MAX_MS", "200").parse().expect("LATENCY_MAX_MS"),
            liquidity_ttl_ms: var("LIQUIDITY_TTL_MS", "30000").parse().expect("LIQUIDITY_TTL_MS"),
            stale_ms: var("STALE_MS", "5000").parse().expect("STALE_MS"),
            book_depth: var("BOOK_DEPTH", "100").parse().expect("BOOK_DEPTH"),
            checksum: var("BOOK_CHECKSUM", "enforce"),
            require_note: matches!(var("REQUIRE_NOTE", "0").as_str(), "1" | "true" | "yes"),
            snapshot_secs: var("SNAPSHOT_SECS", "300").parse().expect("SNAPSHOT_SECS"),
            max_open_orders: var("MAX_OPEN_ORDERS", "100").parse().expect("MAX_OPEN_ORDERS"),
            orders_per_sec: var("ORDERS_PER_SEC", "10").parse().expect("ORDERS_PER_SEC"),
        }
    }

    pub fn max_fee(&self) -> Decimal {
        self.maker_fee.max(self.taker_fee)
    }
}
