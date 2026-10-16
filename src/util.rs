use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

pub fn parse_dec(s: &str) -> Option<Decimal> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Decimal::from_str(s).ok().or_else(|| Decimal::from_scientific(s).ok())
}

/// Exact decimal from a JSON string or number (numbers keep their original text
/// because serde_json is built with `arbitrary_precision`).
pub fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => parse_dec(s),
        Value::Number(n) => parse_dec(&n.to_string()),
        _ => None,
    }
}

pub fn d(s: &str) -> Decimal {
    parse_dec(s).expect("bad decimal literal")
}

pub fn round8(x: Decimal) -> Decimal {
    x.round_dp_with_strategy(8, RoundingStrategy::MidpointAwayFromZero)
}

pub fn ceil8(x: Decimal) -> Decimal {
    x.round_dp_with_strategy(8, RoundingStrategy::ToPositiveInfinity)
}

pub fn floor_to(x: Decimal, inc: Decimal) -> Decimal {
    if inc.is_zero() {
        return x;
    }
    (x / inc).floor() * inc
}

pub fn is_multiple(x: Decimal, inc: Decimal) -> bool {
    inc.is_zero() || (x % inc).is_zero()
}

/// Parse an RFC3339 UTC timestamp like 2023-09-25T07:49:37.708706Z to epoch ms.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let (y, m, dd) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut ms = 0i64;
    if b.len() > 20 && b[19] == b'.' {
        let frac: String = s[20..].chars().take_while(|c| c.is_ascii_digit()).take(3).collect();
        if !frac.is_empty() {
            ms = frac.parse::<i64>().ok()? * 10i64.pow(3 - frac.len() as u32);
        }
    }
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + ms)
}
