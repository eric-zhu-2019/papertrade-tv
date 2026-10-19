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
    /// Perpetual futures, one per spot base asset ("BTC-PERP"); empty when FUTURES=0.
    pub perps: Vec<String>,
    pub fut_maker_fee: Decimal,
    pub fut_taker_fee: Decimal,
    /// Charged on the value of a liquidated position, paid into the insurance fund.
    pub liq_fee: Decimal,
    pub fut_default_leverage: Decimal,
    /// FEED=sim only: seconds between simulated funding payments.
    pub sim_funding_secs: i64,
    /// Spot margin: seconds between rollover fees (Kraken: every 4 hours).
    pub margin_rollover_secs: i64,
    /// Underlyings with options ("BTC", "ETH"); empty when OPTIONS=0.
    pub options: Vec<String>,
    /// Our market maker's quote: theo × (1 ± spread).
    pub opt_spread: Decimal,
    /// Option fee per contract as a fraction of the underlying, capped at a fraction of the premium.
    pub opt_fee: Decimal,
    pub opt_fee_cap: Decimal,
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
        let perps = if matches!(var("FUTURES", "1").as_str(), "0" | "false" | "no") {
            vec![]
        } else {
            symbols.iter().filter_map(|s| s.split_once('/')).map(|(b, _)| format!("{b}-PERP")).collect()
        };
        let options = if matches!(var("OPTIONS", "1").as_str(), "0" | "false" | "no") {
            vec![]
        } else {
            ["BTC", "ETH"]
                .iter()
                .filter(|u| symbols.iter().any(|s| s == &format!("{u}/{quote}")))
                .map(|u| u.to_string())
                .collect()
        };
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
            perps,
            // Kraken Futures entry-tier fees
            fut_maker_fee: dec("FUT_MAKER_FEE", "0.0002"),
            fut_taker_fee: dec("FUT_TAKER_FEE", "0.0005"),
            liq_fee: dec("LIQ_FEE", "0.005"),
            fut_default_leverage: dec("FUT_DEFAULT_LEVERAGE", "10"),
            sim_funding_secs: var("SIM_FUNDING_SECS", "3600").parse().expect("SIM_FUNDING_SECS"),
            margin_rollover_secs: var("MARGIN_ROLLOVER_SECS", "14400").parse().expect("MARGIN_ROLLOVER_SECS"),
            options,
            opt_spread: dec("OPT_SPREAD", "0.03"),
            opt_fee: dec("OPT_FEE", "0.0003"),
            opt_fee_cap: dec("OPT_FEE_CAP", "0.125"),
        }
    }

    pub fn is_perp(&self, s: &str) -> bool {
        self.perps.iter().any(|p| p == s)
    }

    /// Spot pairs and perps: every symbol that has a book.
    pub fn markets(&self) -> impl Iterator<Item = &String> {
        self.symbols.iter().chain(self.perps.iter())
    }

    pub fn is_market(&self, s: &str) -> bool {
        self.symbols.iter().any(|x| x == s) || self.is_perp(s)
    }

    pub fn fut_max_fee(&self) -> Decimal {
        self.fut_maker_fee.max(self.fut_taker_fee)
    }

    pub fn max_fee(&self) -> Decimal {
        self.maker_fee.max(self.taker_fee)
    }
}

/// Kraken Futures product id of a perp: "BTC-PERP" -> "PF_XBTUSD".
pub fn perp_product(perp: &str) -> String {
    let base = perp.trim_end_matches("-PERP");
    format!("PF_{}USD", if base == "BTC" { "XBT" } else { base })
}
