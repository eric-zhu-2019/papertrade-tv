//! Options on BTC and ETH (European, cash-settled in USD), priced by this server: Kraken has no
//! options and Deribit is not available to us. Buy-only for now: a student buys calls or puts and
//! can sell back what they hold, never write options.
//!
//! - Symbols follow Deribit: `BTC-25OCT26-80000-C`. One contract is one coin.
//! - Expiries at 08:00 UTC: the next 3 days, the next 4 Fridays and the last Friday of the next
//!   3 months. Strikes are a grid around the spot price.
//! - Price: Black-Scholes (no interest rate) on the Kraken spot mid, with volatility measured from
//!   Kraken's hourly candles over the last 30 days. We quote ask = theo × (1 + spread) and
//!   bid = theo × (1 − spread), with a minimum half-spread, like a market maker.
//! - Fees as on Deribit: 0.03% of the underlying per contract, capped at 12.5% of the premium.
//! - At expiry, options settle in cash at the average Kraken spot mid over the last 30 minutes.
//!
//! An option held is a balance of an asset named after its symbol, so the spot ledger, the
//! balances audit and the average-cost view all apply unchanged.

use crate::book::Side;
use crate::engine::{Engine, Event, OType, Order, OrderReq, Status, Tif};
use crate::util::*;
use rusqlite::params;
use rust_decimal::prelude::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;
const YEAR_MS: f64 = 365.0 * DAY as f64;
/// Options expire at 08:00 UTC.
const EXPIRY_HOUR: i64 = 8;
const SETTLE_WINDOW: i64 = 30 * 60_000;
const MONTHS: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];

#[derive(Clone, Debug, PartialEq)]
pub struct OptSpec {
    pub underlying: String,
    pub expiry_ms: i64,
    pub strike: Decimal,
    pub call: bool,
}

#[derive(Clone, Debug)]
pub struct OptQuote {
    pub theo: Decimal,
    pub bid: Decimal,
    pub ask: Decimal,
    pub delta: f64,
}

// ------------------------------------------------------------------ calendar

/// (year, month 1-12, day) of a day number since 1970-01-01 (Howard Hinnant's civil_from_days).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// 0 = Monday … 6 = Sunday
fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

fn expiry_of_day(days: i64) -> i64 {
    days * DAY + EXPIRY_HOUR * HOUR
}

/// "25OCT26"
pub fn date_code(expiry_ms: i64) -> String {
    let (y, m, d) = civil(expiry_ms.div_euclid(DAY));
    format!("{d}{}{:02}", MONTHS[m as usize - 1], y % 100)
}

fn parse_date_code(s: &str) -> Option<i64> {
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    let d: u32 = s[..split].parse().ok()?;
    let mon = MONTHS.iter().position(|m| s[split..].starts_with(m))? as u32 + 1;
    let y: i64 = s.get(split + 3..)?.parse().ok()?;
    if !(1..=31).contains(&d) || !(0..100).contains(&y) {
        return None;
    }
    let days = days_from_civil(2000 + y, mon, d);
    (civil(days) == (2000 + y, mon, d)).then(|| expiry_of_day(days))
}

/// Listed expiries after `now`: the next 3 days, the next 4 Fridays, the last Friday of the next 3 months.
pub fn listed_expiries(now: i64) -> Vec<i64> {
    let mut set = BTreeSet::new();
    let today = now.div_euclid(DAY);
    let mut day = today;
    let mut dailies = 0;
    while dailies < 3 {
        if expiry_of_day(day) > now {
            set.insert(expiry_of_day(day));
            dailies += 1;
        }
        day += 1;
    }
    let mut day = today;
    let mut fridays = 0;
    while fridays < 4 {
        if weekday(day) == 4 && expiry_of_day(day) > now {
            set.insert(expiry_of_day(day));
            fridays += 1;
        }
        day += 1;
    }
    let (mut y, mut m, _) = civil(today);
    let mut months = 0;
    while months < 3 {
        let next = if m == 12 { days_from_civil(y + 1, 1, 1) } else { days_from_civil(y, m + 1, 1) };
        let mut last = next - 1;
        while weekday(last) != 4 {
            last -= 1;
        }
        if expiry_of_day(last) > now {
            set.insert(expiry_of_day(last));
            months += 1;
        }
        if m == 12 {
            y += 1;
            m = 1;
        } else {
            m += 1;
        }
    }
    set.into_iter().collect()
}

// ------------------------------------------------------------------ pricing

/// Standard normal CDF (Abramowitz & Stegun 7.1.26, error < 1.5e-7).
fn ncdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * x.abs() / std::f64::consts::SQRT_2);
    let y = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592) * t
            * (-(x * x) / 2.0).exp();
    if x >= 0.0 {
        0.5 * (1.0 + y)
    } else {
        0.5 * (1.0 - y)
    }
}

/// Black-Scholes value and delta with no interest rate. `t` in years.
pub fn black_scholes(call: bool, s: f64, k: f64, t: f64, vol: f64) -> (f64, f64) {
    if t <= 0.0 || vol <= 0.0 {
        let v = if call { (s - k).max(0.0) } else { (k - s).max(0.0) };
        let delta = if call { (s > k) as i32 as f64 } else { -((s < k) as i32 as f64) };
        return (v, delta);
    }
    let sd = vol * t.sqrt();
    let d1 = ((s / k).ln() + sd * sd / 2.0) / sd;
    let d2 = d1 - sd;
    if call {
        (s * ncdf(d1) - k * ncdf(d2), ncdf(d1))
    } else {
        (k * ncdf(-d2) - s * ncdf(-d1), ncdf(d1) - 1.0)
    }
}

/// Annualised volatility of hourly closes.
pub fn realised_vol(closes: &[f64]) -> Option<f64> {
    let rets: Vec<f64> = closes.windows(2).filter(|w| w[0] > 0.0 && w[1] > 0.0).map(|w| (w[1] / w[0]).ln()).collect();
    if rets.len() < 100 {
        return None;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() - 1) as f64;
    Some((var * 24.0 * 365.0).sqrt())
}

pub fn strike_step(underlying: &str) -> Decimal {
    match underlying {
        "BTC" => Decimal::from(1000),
        "ETH" => Decimal::from(50),
        _ => Decimal::ONE,
    }
}

pub fn qty_step(underlying: &str) -> Decimal {
    if underlying == "BTC" {
        d("0.01")
    } else {
        d("0.1")
    }
}

fn max_qty(underlying: &str) -> Decimal {
    if underlying == "BTC" {
        Decimal::from(25)
    } else {
        Decimal::from(250)
    }
}

const TICK: &str = "0.1";

pub fn default_vol(underlying: &str) -> f64 {
    if underlying == "BTC" {
        0.5
    } else {
        0.65
    }
}

impl OptSpec {
    pub fn parse(symbol: &str) -> Option<OptSpec> {
        let mut it = symbol.split('-');
        let (u, date, k, cp) = (it.next()?, it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        let strike = parse_dec(k)?;
        Some(OptSpec {
            underlying: u.to_string(),
            expiry_ms: parse_date_code(date)?,
            strike,
            call: match cp {
                "C" => true,
                "P" => false,
                _ => return None,
            },
        })
    }

    pub fn symbol(&self) -> String {
        format!("{}-{}-{}-{}", self.underlying, date_code(self.expiry_ms), self.strike.normalize(), if self.call { "C" } else { "P" })
    }

    pub fn intrinsic(&self, spot: Decimal) -> Decimal {
        if self.call {
            (spot - self.strike).max(Decimal::ZERO)
        } else {
            (self.strike - spot).max(Decimal::ZERO)
        }
    }
}

fn s8(x: Decimal) -> String {
    x.round_dp(8).normalize().to_string()
}

impl Engine {
    // ---------------------------------------------------------------- market

    /// Is this an option symbol on one of our underlyings?
    pub fn is_option(&self, symbol: &str) -> bool {
        OptSpec::parse(symbol).map(|o| self.cfg.options.contains(&o.underlying)).unwrap_or(false)
    }

    pub fn opt_spot_symbol(&self, underlying: &str) -> String {
        format!("{underlying}/{}", self.cfg.quote)
    }

    pub fn opt_vol(&self, underlying: &str) -> f64 {
        self.opt_vols.get(underlying).copied().unwrap_or_else(|| default_vol(underlying))
    }

    /// Theoretical value, our bid and ask, and delta; None without a spot price.
    pub fn opt_quote(&self, spec: &OptSpec, now: i64) -> Option<OptQuote> {
        let spot = self.mark(&self.opt_spot_symbol(&spec.underlying))?;
        let t = (spec.expiry_ms - now) as f64 / YEAR_MS;
        let (theo, delta) =
            black_scholes(spec.call, spot.to_f64()?, spec.strike.to_f64()?, t, self.opt_vol(&spec.underlying));
        let theo = Decimal::from_f64(theo)?.max(Decimal::ZERO).round_dp(8);
        let tick = d(TICK);
        // a market maker's quote: the spread, but never less than a minimum (as on Deribit, the
        // cheapest option still costs a tick of 0.0001 coin)
        let half = (theo * self.cfg.opt_spread).max(spot * d("0.0001"));
        let ask = ((theo + half) / tick).ceil() * tick;
        let bid = (((theo - half).max(Decimal::ZERO)) / tick).floor() * tick;
        Some(OptQuote { theo, bid, ask, delta })
    }

    /// Value of a holding for equity and the balances view: at the latest market-data time.
    pub fn opt_value(&self, symbol: &str) -> Option<Decimal> {
        self.opt_mark(symbol, self.last_feed_ms)
    }

    /// Mark value of one contract (theoretical value; intrinsic once expired).
    pub fn opt_mark(&self, symbol: &str, now: i64) -> Option<Decimal> {
        let spec = OptSpec::parse(symbol)?;
        if let Some(px) = self.opt_settled.get(&(spec.underlying.clone(), spec.expiry_ms)) {
            return Some(spec.intrinsic(*px));
        }
        self.opt_quote(&spec, now).map(|q| q.theo)
    }

    /// Deribit's fee: 0.03% of the underlying per contract, at most 12.5% of the premium.
    fn opt_fee(&self, spot: Decimal, price: Decimal, qty: Decimal) -> Decimal {
        round8((spot * self.cfg.opt_fee * qty).min(price * qty * self.cfg.opt_fee_cap))
    }

    pub fn opt_tradable(&self, spec: &OptSpec, now: i64) -> bool {
        spec.expiry_ms > now && self.tradable(&self.opt_spot_symbol(&spec.underlying), now)
    }

    /// The chain for one expiry: strikes around the spot price, calls and puts.
    pub fn opt_chain(&self, underlying: &str, expiry: Option<i64>, now: i64) -> Result<Value, String> {
        if !self.cfg.options.iter().any(|u| u == underlying) {
            return Err(format!("no options on {underlying}"));
        }
        let expiries = listed_expiries(now);
        let expiry = expiry.filter(|e| expiries.contains(e)).unwrap_or(expiries[0]);
        let spot = self.mark(&self.opt_spot_symbol(underlying)).ok_or("no spot price yet")?;
        let step = strike_step(underlying);
        let atm = (spot / step).round() * step;
        let rows: Vec<Value> = (-10..=10)
            .map(|i| atm + step * Decimal::from(i))
            .filter(|k| *k > Decimal::ZERO)
            .map(|k| {
                let side = |call: bool| {
                    let spec = OptSpec { underlying: underlying.to_string(), expiry_ms: expiry, strike: k, call };
                    let q = self.opt_quote(&spec, now);
                    json!({
                        "symbol": spec.symbol(), "theo": q.as_ref().map(|q| s8(q.theo)), "bid": q.as_ref().map(|q| s8(q.bid)),
                        "ask": q.as_ref().map(|q| s8(q.ask)), "delta": q.as_ref().map(|q| (q.delta * 1000.0).round() / 1000.0),
                    })
                };
                json!({"strike": k.normalize().to_string(), "call": side(true), "put": side(false)})
            })
            .collect();
        Ok(json!({
            "underlying": underlying, "spot": s8(spot), "vol": (self.opt_vol(underlying) * 10000.0).round() / 10000.0,
            "expiry_ms": expiry, "expiries": expiries.iter().map(|e| json!({"expiry_ms": e, "code": date_code(*e)})).collect::<Vec<_>>(),
            "rows": rows, "qty_step": qty_step(underlying).to_string(), "spread": self.cfg.opt_spread.to_string(),
            "fee": self.cfg.opt_fee.to_string(), "fee_cap": self.cfg.opt_fee_cap.to_string(),
        }))
    }

    /// One contract: quote and details, for the order form.
    pub fn opt_info(&self, symbol: &str, now: i64) -> Result<Value, String> {
        let spec = OptSpec::parse(symbol).filter(|s| self.cfg.options.contains(&s.underlying)).ok_or("unknown option")?;
        let spot = self.mark(&self.opt_spot_symbol(&spec.underlying));
        let q = self.opt_quote(&spec, now);
        Ok(json!({
            "symbol": spec.symbol(), "underlying": spec.underlying, "expiry_ms": spec.expiry_ms,
            "strike": spec.strike.normalize().to_string(), "type": if spec.call { "call" } else { "put" },
            "spot": spot.map(s8), "vol": self.opt_vol(&spec.underlying), "theo": q.as_ref().map(|q| s8(q.theo)),
            "bid": q.as_ref().map(|q| s8(q.bid)), "ask": q.as_ref().map(|q| s8(q.ask)), "delta": q.as_ref().map(|q| q.delta),
            "qty_step": qty_step(&spec.underlying).to_string(), "max_qty": max_qty(&spec.underlying).to_string(),
            "tradable": self.opt_tradable(&spec, now),
            "settled_at": self.opt_settled.get(&(spec.underlying.clone(), spec.expiry_ms)).map(|p| s8(*p)),
        }))
    }

    // ---------------------------------------------------------------- orders

    /// Validate and reserve: a buy reserves the premium at its limit (or the ask) plus the most the
    /// fee can be; a sell can only sell contracts held (no writing options) and reserves them.
    pub(crate) fn opt_place_order(&mut self, uid: i64, req: OrderReq, now: i64) -> Result<Order, String> {
        let spec = OptSpec::parse(&req.symbol).ok_or("unknown option")?;
        if spec.symbol() != req.symbol {
            return Err(format!("write the option as {}", spec.symbol()));
        }
        if !self.opt_tradable(&spec, now) {
            return Err(if spec.expiry_ms <= now {
                "this option has expired".to_string()
            } else {
                "market data is unavailable right now; trading is paused".to_string()
            });
        }
        if !(spec.strike > Decimal::ZERO && is_multiple(spec.strike, strike_step(&spec.underlying))) {
            return Err(format!("strike must be a multiple of {}", strike_step(&spec.underlying)));
        }
        if !listed_expiries(now).contains(&spec.expiry_ms) {
            return Err("that expiry is not listed".into());
        }
        if !matches!(req.otype, OType::Market | OType::Limit) {
            return Err("options take market and limit orders only".into());
        }
        if req.leverage.is_some() || req.reduce_only {
            return Err("options take no leverage or reduce_only".into());
        }
        if req.post_only && req.otype != OType::Limit {
            return Err("post_only is only valid for limit orders".into());
        }
        if req.tif != Tif::Gtc && req.otype != OType::Limit {
            return Err("time_in_force other than gtc is only valid for limit orders".into());
        }
        let step = qty_step(&spec.underlying);
        if req.qty <= Decimal::ZERO || !is_multiple(req.qty, step) {
            return Err(format!("quantity must be a positive multiple of {step} contracts"));
        }
        if req.qty > max_qty(&spec.underlying) {
            return Err(format!("at most {} contracts per order", max_qty(&spec.underlying)));
        }
        let limit = match req.otype {
            OType::Limit => {
                let p = req.limit_price.ok_or("limit_price is required for this order type")?;
                if p <= Decimal::ZERO || !is_multiple(p, d(TICK)) {
                    return Err(format!("limit_price must be a positive multiple of {TICK}"));
                }
                Some(p)
            }
            _ => None,
        };
        let q = self.opt_quote(&spec, now).ok_or("no price for this option yet")?;
        let (asset, lock) = match req.side {
            Side::Buy => {
                let px = limit.unwrap_or(q.ask);
                (self.cfg.quote.clone(), ceil8(req.qty * px * (Decimal::ONE + self.cfg.opt_fee_cap)))
            }
            Side::Sell => (req.symbol.clone(), req.qty),
        };
        let free = self.free(uid, &asset);
        if lock > free {
            return Err(if req.side == Side::Sell {
                format!("you can only sell options you hold ({} available); writing options is not supported", free.normalize())
            } else {
                format!("insufficient {asset}: need {}, available {}", lock.round_dp(2), free.max(Decimal::ZERO).round_dp(2))
            });
        }
        let note = req.note.as_ref().map(|n| n.trim().chars().take(500).collect::<String>()).filter(|n| !n.is_empty());
        if self.cfg.require_note && note.is_none() {
            return Err("a trade note is required: write one sentence on why you are placing this order".into());
        }
        let o = self.insert_order(Order {
            id: 0,
            user_id: uid,
            season_id: self.season_id,
            symbol: req.symbol,
            side: req.side,
            otype: req.otype,
            tif: req.tif,
            post_only: req.post_only,
            reduce_only: false,
            leverage: None,
            qty: req.qty,
            limit_price: limit,
            trigger_price: None,
            filled: Decimal::ZERO,
            cum_cost: Decimal::ZERO,
            fee: Decimal::ZERO,
            status: Status::Pending,
            locked: lock,
            queue_ahead: None,
            note,
            reason: None,
            created_ms: now,
            updated_ms: now,
        });
        self.balances.entry((uid, asset.clone())).or_default().locked += lock;
        self.save_bal(uid, &asset);
        self.commit();
        self.emit_order(&o);
        Ok(o)
    }

    /// The order reaches our market maker: it fills at the quote if marketable, else it rests.
    pub(crate) fn opt_execute(&mut self, oid: i64, now: i64) {
        let Some(o) = self.orders.get(&oid).cloned() else { return };
        let Some(spec) = OptSpec::parse(&o.symbol) else { return };
        let q = match self.opt_quote(&spec, now) {
            Some(q) if self.opt_tradable(&spec, now) => q,
            _ => {
                self.finish(oid, Status::Rejected, Some("market data became unavailable before the order reached the book"), now);
                return;
            }
        };
        let px = if o.side == Side::Buy { q.ask } else { q.bid };
        let marketable = match (o.otype, o.limit_price) {
            (OType::Market, _) => true,
            (_, Some(l)) => if o.side == Side::Buy { px <= l } else { px >= l },
            _ => false,
        };
        if o.side == Side::Sell && px <= Decimal::ZERO && o.otype == OType::Market {
            self.finish(oid, Status::Canceled, Some("there is no bid for this option"), now);
            return;
        }
        if marketable {
            if o.post_only {
                self.finish(oid, Status::Canceled, Some("post-only order would have taken liquidity"), now);
                return;
            }
            if o.side == Side::Buy {
                // the ask may have moved since the order was accepted
                let spot = self.mark(&self.opt_spot_symbol(&spec.underlying)).unwrap_or_default();
                let due = round8(px * o.qty) + self.opt_fee(spot, px, o.qty);
                if due > o.locked + self.free(o.user_id, &self.cfg.quote) {
                    self.finish(oid, Status::Canceled, Some("insufficient funds at execution"), now);
                    return;
                }
            }
            self.opt_fill(oid, px, o.qty, true, now);
            return;
        }
        if o.tif != Tif::Gtc {
            self.finish(oid, Status::Canceled, Some("unfilled remainder of an immediate order was cancelled"), now);
            return;
        }
        let o2 = self.orders.get_mut(&oid).unwrap();
        o2.status = Status::Open;
        o2.updated_ms = now;
        let snap = o2.clone();
        self.save_order(&snap);
        self.emit_order(&snap);
    }

    /// Fill the whole order (our market maker has no depth limit) and settle it like a spot trade.
    fn opt_fill(&mut self, oid: i64, price: Decimal, qty: Decimal, taker: bool, now: i64) {
        let quote = self.cfg.quote.clone();
        let o = self.orders[&oid].clone();
        let spec = OptSpec::parse(&o.symbol).unwrap();
        let spot = self.mark(&self.opt_spot_symbol(&spec.underlying)).unwrap_or_default();
        let cost = round8(price * qty);
        let fee = self.opt_fee(spot, price, qty);
        let uid = o.user_id;
        let user = format!("user:{uid}");
        match o.side {
            Side::Buy => {
                let b = self.balances.entry((uid, quote.clone())).or_default();
                b.total -= cost + fee;
                b.locked -= o.locked;
                self.balances.entry((uid, o.symbol.clone())).or_default().total += qty;
            }
            Side::Sell => {
                let b = self.balances.entry((uid, o.symbol.clone())).or_default();
                b.total -= qty;
                b.locked -= o.locked;
                self.balances.entry((uid, quote.clone())).or_default().total += cost - fee;
            }
        }
        let snap = {
            let o = self.orders.get_mut(&oid).unwrap();
            o.locked = Decimal::ZERO;
            o.filled += qty;
            o.cum_cost += cost;
            o.fee += fee;
            o.updated_ms = now;
            o.status = Status::Filled;
            o.clone()
        };
        let fid = self.record_fill(&snap, price, qty, cost, fee, taker, now);
        let txn = self.new_txn();
        let (pay, get) = match o.side {
            Side::Buy => ((user.as_str(), "market", quote.as_str(), cost), ("market", user.as_str(), o.symbol.as_str(), qty)),
            Side::Sell => ((user.as_str(), "market", o.symbol.as_str(), qty), ("market", user.as_str(), quote.as_str(), cost)),
        };
        self.transfer(txn, now, pay.0, pay.1, pay.2, pay.3, "trade", fid);
        self.transfer(txn, now, get.0, get.1, get.2, get.3, "trade", fid);
        self.transfer(txn, now, &user, "fees", &quote, fee, "fee", fid);
        self.save_bal(uid, &quote);
        self.save_bal(uid, &o.symbol);
        self.save_order(&snap);
        self.orders.remove(&oid);
        self.publish_fill(&snap, fid, price, qty, cost, fee, taker, now);
    }

    // ---------------------------------------------------------------- once a second

    /// Fill resting limit orders the quote has reached, record spot prices for settlement, and
    /// settle expiries that have passed.
    pub fn opt_tick(&mut self, now: i64) {
        if self.cfg.options.is_empty() {
            return;
        }
        for u in self.cfg.options.clone() {
            if let Some(m) = self.mark(&self.opt_spot_symbol(&u)).filter(|_| self.feed_ok(now)) {
                let s = self.opt_samples.entry(u.clone()).or_default();
                s.push_back((now, m));
                while s.front().map(|x| now - x.0 > SETTLE_WINDOW + 60_000).unwrap_or(false) {
                    s.pop_front();
                }
            }
        }
        let resting: Vec<i64> = self
            .orders
            .values()
            .filter(|o| o.status == Status::Open && self.is_option(&o.symbol))
            .map(|o| o.id)
            .collect();
        for id in resting {
            let o = self.orders[&id].clone();
            let spec = OptSpec::parse(&o.symbol).unwrap();
            if !self.opt_tradable(&spec, now) {
                continue;
            }
            let Some(q) = self.opt_quote(&spec, now) else { continue };
            let Some(l) = o.limit_price else { continue };
            // the quote moved through a resting order: it fills at its own price, as maker
            let hit = if o.side == Side::Buy { q.ask <= l } else { q.bid >= l && q.bid > Decimal::ZERO };
            if hit {
                self.opt_fill(id, l, o.qty, false, now);
            }
        }
        self.opt_settle(now);
        self.commit();
    }

    /// Cash-settle every expiry that has passed: holders get the intrinsic value at the average
    /// spot mid of the last 30 minutes; open orders on it are cancelled.
    fn opt_settle(&mut self, now: i64) {
        let mut due: BTreeSet<(String, i64)> = BTreeSet::new();
        for (_, asset) in self.balances.keys() {
            if let Some(s) = OptSpec::parse(asset) {
                if s.expiry_ms <= now && !self.opt_settled.contains_key(&(s.underlying.clone(), s.expiry_ms)) {
                    due.insert((s.underlying, s.expiry_ms));
                }
            }
        }
        for o in self.orders.values() {
            if let Some(s) = OptSpec::parse(&o.symbol) {
                if s.expiry_ms <= now {
                    due.insert((s.underlying, s.expiry_ms));
                }
            }
        }
        for (u, expiry) in due {
            let ids: Vec<i64> = self
                .orders
                .values()
                .filter(|o| OptSpec::parse(&o.symbol).map(|s| s.underlying == u && s.expiry_ms == expiry).unwrap_or(false))
                .map(|o| o.id)
                .collect();
            for id in ids {
                self.finish(id, Status::Canceled, Some("the option expired"), now);
            }
            let price = match self.opt_settled.get(&(u.clone(), expiry)) {
                Some(p) => *p,
                None => {
                    let samples: Vec<Decimal> = self
                        .opt_samples
                        .get(&u)
                        .map(|s| s.iter().filter(|x| x.0 > expiry - SETTLE_WINDOW && x.0 <= expiry).map(|x| x.1).collect())
                        .unwrap_or_default();
                    let p = if samples.is_empty() {
                        // no record of the window (e.g. the server was down): the price now, once the feed is live
                        if !self.feed_ok(now) {
                            continue;
                        }
                        let Some(m) = self.mark(&self.opt_spot_symbol(&u)) else { continue };
                        m
                    } else {
                        (samples.iter().sum::<Decimal>() / Decimal::from(samples.len())).round_dp(8)
                    };
                    self.ensure_tx();
                    self.db
                        .execute(
                            "INSERT OR REPLACE INTO option_settlements(underlying, expiry_ms, price, settled_ms) VALUES(?,?,?,?)",
                            params![u, expiry, p.to_string(), now],
                        )
                        .unwrap();
                    self.opt_settled.insert((u.clone(), expiry), p);
                    tracing::info!("options: {u} {} settled at {p}", date_code(expiry));
                    p
                }
            };
            self.opt_pay_out(&u, expiry, price, now);
        }
    }

    /// Exchange each holding of an expired series for its intrinsic value in USD.
    fn opt_pay_out(&mut self, underlying: &str, expiry: i64, price: Decimal, now: i64) {
        let quote = self.cfg.quote.clone();
        let held: Vec<((i64, String), Decimal)> = self
            .balances
            .iter()
            .filter(|(k, b)| {
                !b.total.is_zero()
                    && OptSpec::parse(&k.1).map(|s| s.underlying == underlying && s.expiry_ms == expiry).unwrap_or(false)
            })
            .map(|(k, b)| (k.clone(), b.total))
            .collect();
        for ((uid, sym), qty) in held {
            let spec = OptSpec::parse(&sym).unwrap();
            let payout = round8(spec.intrinsic(price) * qty);
            let txn = self.new_txn();
            let user = format!("user:{uid}");
            self.transfer(txn, now, &user, "market", &sym, qty, "expiry", 0);
            self.transfer(txn, now, "market", &user, &quote, payout, "expiry", 0);
            self.balances.insert((uid, sym.clone()), Default::default());
            self.balances.entry((uid, quote.clone())).or_default().total += payout;
            self.save_bal(uid, &sym);
            self.save_bal(uid, &quote);
            let payload = json!({
                "type": "option_expiry", "symbol": sym, "qty": s8(qty), "settlement": s8(price), "payout": s8(payout),
            })
            .to_string();
            self.publish(Event::User { user_id: uid, payload: payload.into() });
        }
    }

    /// New season: options are cashed in at their theoretical value before balances are swept.
    pub(crate) fn opt_season_reset(&mut self, now: i64) {
        let quote = self.cfg.quote.clone();
        let held: Vec<((i64, String), Decimal)> = self
            .balances
            .iter()
            .filter(|(k, b)| !b.total.is_zero() && OptSpec::parse(&k.1).is_some())
            .map(|(k, b)| (k.clone(), b.total))
            .collect();
        for ((uid, sym), qty) in held {
            let value = round8(self.opt_mark(&sym, now).unwrap_or_default() * qty);
            let txn = self.new_txn();
            let user = format!("user:{uid}");
            self.transfer(txn, now, &user, "market", &sym, qty, "season_reset", 0);
            self.transfer(txn, now, "market", &user, &quote, value, "season_reset", 0);
            self.balances.insert((uid, sym.clone()), Default::default());
            self.balances.entry((uid, quote.clone())).or_default().total += value;
            self.save_bal(uid, &sym);
            self.save_bal(uid, &quote);
        }
    }

    pub(crate) fn opt_load(&mut self) {
        let rows: Vec<(String, i64, String)> = {
            let mut st = self.db.prepare("SELECT underlying, expiry_ms, price FROM option_settlements").unwrap();
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for (u, e, p) in rows {
            if let Some(p) = parse_dec(&p) {
                self.opt_settled.insert((u, e), p);
            }
        }
    }
}
