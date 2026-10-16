//! Perpetual futures: a separate USD wallet, one net position per market with
//! isolated or cross margin, Kraken's margin tiers, hourly funding and
//! liquidation.
//!
//! Perp orders go through the same matching simulator as spot, against the
//! live Kraken Futures book and trade stream. Only settlement differs: a fill
//! opens, grows, reduces or flips the user's position instead of moving coins.
//!
//! Ledger accounts, all in USD: `fut:{user}` is the futures wallet,
//! `posm:{position}` holds an isolated position's margin, `market` is the
//! counterparty for realised P&L (as for spot trades), and `fees`, `funding`
//! and `insurance`. The insurance fund collects liquidation fees and covers
//! any loss larger than the margin behind it, so no account goes below zero.

use crate::book::Side;
use crate::config::perp_product;
use crate::engine::{Engine, OType, Order, OrderReq, Status, Ticker};
use crate::util::*;
use rusqlite::params;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarginMode {
    Isolated,
    Cross,
}

impl MarginMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MarginMode::Isolated => "isolated",
            MarginMode::Cross => "cross",
        }
    }
    pub fn parse(s: &str) -> Option<MarginMode> {
        match s {
            "isolated" => Some(MarginMode::Isolated),
            "cross" => Some(MarginMode::Cross),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Perp {
    pub symbol: String,
    pub product: String,
    /// Kraken margin tiers: (position value from, initial margin rate, maintenance margin rate).
    pub tiers: Vec<(Decimal, Decimal, Decimal)>,
    pub max_position: Option<Decimal>,
    pub mark: Option<Decimal>,
    pub index: Option<Decimal>,
    /// This period's funding as a fraction of position value; longs pay shorts when positive.
    pub funding_rate: Option<Decimal>,
    pub next_funding_ms: Option<i64>,
    pub open_interest: Option<Decimal>,
}

impl Perp {
    pub fn new(symbol: &str) -> Perp {
        Perp {
            symbol: symbol.to_string(),
            product: perp_product(symbol),
            tiers: vec![],
            max_position: None,
            mark: None,
            index: None,
            funding_rate: None,
            next_funding_ms: None,
            open_interest: None,
        }
    }

    pub fn max_leverage(&self) -> Decimal {
        self.tiers.first().filter(|t| t.1 > Decimal::ZERO).map(|t| (Decimal::ONE / t.1).floor()).unwrap_or(Decimal::ONE)
    }

    /// (initial, maintenance) margin rates for a position of this value.
    pub fn rates(&self, value: Decimal) -> (Decimal, Decimal) {
        self.tiers
            .iter()
            .rev()
            .find(|t| value >= t.0)
            .or(self.tiers.first())
            .map(|t| (t.1, t.2))
            .unwrap_or((Decimal::ONE, d("0.5")))
    }
}

#[derive(Clone, Debug, Default)]
pub struct FutAcct {
    /// USD in the futures wallet, including reservations for open orders but not isolated margin.
    pub wallet: Decimal,
    /// Reserved for open perp orders (initial margin + fees).
    pub locked: Decimal,
}

/// Outcome of a leverage change (see `Engine::plan_leverage`). Margin figures are the isolated
/// margin, or for cross the initial margin tied up.
#[derive(Clone, Debug, Default)]
pub struct LevPlan {
    pub has_position: bool,
    pub margin_before: Decimal,
    pub margin_after: Decimal,
    pub liq_before: Option<Decimal>,
    pub liq_after: Option<Decimal>,
    /// Change in what the market's open orders reserve.
    pub lock_delta: Decimal,
}

impl LevPlan {
    pub fn json(&self) -> Value {
        json!({
            "has_position": self.has_position, "margin_before": s8(self.margin_before), "margin_after": s8(self.margin_after),
            "liq_before": self.liq_before.map(s8), "liq_after": self.liq_after.map(s8), "orders_lock_delta": s8(self.lock_delta),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Position {
    pub id: i64,
    pub user_id: i64,
    pub symbol: String,
    pub long: bool,
    /// Contracts (one contract = one coin); positive is long, negative short.
    pub size: Decimal,
    pub entry: Decimal,
    pub mode: MarginMode,
    pub leverage: Decimal,
    /// Margin held by an isolated position; always zero for cross.
    pub margin: Decimal,
    pub realized: Decimal,
    pub fees: Decimal,
    /// Funding received (+) or paid (-).
    pub funding: Decimal,
    pub closed_qty: Decimal,
    pub closed_value: Decimal,
    pub opened_ms: i64,
    pub updated_ms: i64,
}

impl Position {
    pub fn upnl(&self, mark: Decimal) -> Decimal {
        self.size * (mark - self.entry)
    }
}

const POS_COLS: &str = "id, user_id, season_id, symbol, size, entry, mode, leverage, margin, realized, fees, funding, \
     closed_qty, closed_value, opened_ms, updated_ms, side";

fn row_to_pos(r: &rusqlite::Row) -> rusqlite::Result<Position> {
    let s = |i: usize| -> rusqlite::Result<Decimal> { Ok(parse_dec(&r.get::<_, String>(i)?).unwrap_or(Decimal::ZERO)) };
    Ok(Position {
        id: r.get(0)?,
        user_id: r.get(1)?,
        symbol: r.get(3)?,
        size: s(4)?,
        entry: s(5)?,
        mode: MarginMode::parse(&r.get::<_, String>(6)?).unwrap_or(MarginMode::Isolated),
        leverage: s(7)?,
        margin: s(8)?,
        realized: s(9)?,
        fees: s(10)?,
        funding: s(11)?,
        closed_qty: s(12)?,
        closed_value: s(13)?,
        opened_ms: r.get(14)?,
        updated_ms: r.get(15)?,
        long: r.get::<_, String>(16)? == "long",
    })
}

fn sign(side: Side) -> Decimal {
    if side == Side::Buy {
        Decimal::ONE
    } else {
        -Decimal::ONE
    }
}

/// -1, 0 or 1
fn sgn(x: Decimal) -> Decimal {
    if x > Decimal::ZERO {
        Decimal::ONE
    } else if x < Decimal::ZERO {
        -Decimal::ONE
    } else {
        Decimal::ZERO
    }
}

fn s8(x: Decimal) -> String {
    x.round_dp(8).normalize().to_string()
}

impl Engine {
    // ---------------------------------------------------------------- market data

    pub fn fut_feed_ok(&self, now: i64) -> bool {
        self.fut_feed_connected && now - self.fut_last_feed_ms < self.cfg.stale_ms
    }

    pub fn fut_feed_up(&mut self, now: i64) {
        self.fut_feed_connected = true;
        self.fut_last_feed_ms = now;
    }

    pub fn fut_feed_down(&mut self) {
        self.fut_feed_connected = false;
        for (s, b) in self.books.iter_mut() {
            if self.perps.contains_key(s) {
                b.ready = false;
            }
        }
    }

    pub fn set_perp_rules(&mut self, symbol: &str, tiers: Vec<(Decimal, Decimal, Decimal)>, max_position: Option<Decimal>) {
        if let Some(p) = self.perps.get_mut(symbol) {
            p.tiers = tiers;
            p.max_position = max_position;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_perp_ticker(
        &mut self,
        symbol: &str,
        mark: Option<Decimal>,
        index: Option<Decimal>,
        funding_rate: Option<Decimal>,
        next_funding_ms: Option<i64>,
        open_interest: Option<Decimal>,
        t: Ticker,
    ) {
        let Some(p) = self.perps.get_mut(symbol) else { return };
        // Right after the hour Kraken moves next_funding_rate_time on and publishes the next
        // period's rate. Settle the period that just ended at the rate shown for it, before
        // that information is overwritten, or the once-a-second tick could miss it.
        if let (Some(old_ts), Some(old_rate), Some(new_ts)) = (p.next_funding_ms, p.funding_rate, next_funding_ms) {
            if new_ts > old_ts {
                self.funding_due.push((symbol.to_string(), old_ts, old_rate));
            }
        }
        let Some(p) = self.perps.get_mut(symbol) else { return };
        p.mark = mark.or(p.mark);
        p.index = index.or(p.index);
        p.funding_rate = funding_rate.or(p.funding_rate);
        p.next_funding_ms = next_funding_ms.or(p.next_funding_ms);
        p.open_interest = open_interest.or(p.open_interest);
        self.on_ticker(symbol, t);
    }

    /// Kraken's mark price (what P&L, margin and liquidation use), else the book mid.
    pub fn fut_mark(&self, symbol: &str) -> Option<Decimal> {
        self.perps
            .get(symbol)
            .and_then(|p| p.mark)
            .or_else(|| self.books.get(symbol).filter(|b| b.ready).and_then(|b| b.mid()))
            .or_else(|| self.tickers.get(symbol).and_then(|t| t.last))
    }

    pub fn fut_tickers(&self, now: i64) -> Vec<Value> {
        let st = |x: Option<Decimal>| x.map(|v| v.normalize().to_string());
        self.cfg
            .perps
            .iter()
            .filter_map(|s| {
                let p = self.perps.get(s)?;
                let t = self.tickers.get(s).cloned().unwrap_or_default();
                let b = self.books.get(s).filter(|b| b.ready);
                Some(json!({
                    "symbol": s, "kind": "perp", "last": st(t.last),
                    "bid": st(b.and_then(|b| b.best(Side::Buy)).map(|l| l.0)),
                    "ask": st(b.and_then(|b| b.best(Side::Sell)).map(|l| l.0)),
                    "high": st(t.high), "low": st(t.low), "volume": st(t.volume), "change_pct": st(t.change_pct),
                    "mark": st(p.mark.map(|m| m.round_dp(8))), "index": st(p.index),
                    "funding_rate": st(p.funding_rate), "next_funding_ms": p.next_funding_ms,
                    "open_interest": st(p.open_interest), "max_leverage": p.max_leverage().to_string(),
                    "tradable": self.tradable(s, now),
                }))
            })
            .collect()
    }

    // ---------------------------------------------------------------- settings and margin math

    /// (margin mode, leverage) for the user's next order or current position on a perp.
    pub fn fut_settings(&self, uid: i64, symbol: &str) -> (MarginMode, Decimal) {
        let key = (uid, symbol.to_string());
        if let Some(p) = self.positions.get(&key) {
            return (p.mode, p.leverage);
        }
        self.fut_prefs.get(&key).copied().unwrap_or_else(|| {
            let max = self.perps.get(symbol).map(|p| p.max_leverage()).unwrap_or(Decimal::ONE);
            (MarginMode::Isolated, self.cfg.fut_default_leverage.min(max).max(Decimal::ONE))
        })
    }

    /// Change a market's margin mode or leverage. The mode can only change with no position and
    /// no orders. Leverage can change at any time; see `plan_leverage` for what that does.
    pub fn set_fut_settings(&mut self, uid: i64, symbol: &str, mode: MarginMode, leverage: Decimal, now: i64) -> Result<(), String> {
        let max = self.perps.get(symbol).ok_or("unknown futures market")?.max_leverage();
        if leverage < Decimal::ONE || leverage > max || !leverage.fract().is_zero() {
            return Err(format!("leverage must be a whole number from 1 to {max}"));
        }
        let key = (uid, symbol.to_string());
        let (cur_mode, cur_lev) = self.fut_settings(uid, symbol);
        if mode == cur_mode && leverage == cur_lev {
            return Ok(()); // nothing to change (spinner clicks and repeated requests are free)
        }
        if mode != cur_mode
            && (self.positions.contains_key(&key) || self.orders.values().any(|o| o.user_id == uid && o.symbol == symbol))
        {
            return Err("close the position and cancel open orders on this market before switching margin mode".into());
        }
        if leverage != cur_lev {
            let plan = self.plan_leverage(uid, symbol, leverage, now)?;
            if let Some(mut p) = self.positions.get(&key).cloned() {
                if p.mode == MarginMode::Isolated {
                    self.move_margin(&mut p, plan.margin_after - plan.margin_before, now);
                }
                p.leverage = leverage;
                p.updated_ms = now;
                self.save_pos(&p);
                self.positions.insert(key.clone(), p);
            }
        }
        self.fut_prefs.insert(key, (mode, leverage));
        self.db
            .execute(
                "INSERT INTO fut_settings(user_id, symbol, mode, leverage) VALUES(?,?,?,?)
                 ON CONFLICT(user_id, symbol) DO UPDATE SET mode=excluded.mode, leverage=excluded.leverage",
                params![uid, symbol, mode.as_str(), leverage.to_string()],
            )
            .unwrap();
        // open orders on this market were reserved at the old leverage: re-size them
        let ids: Vec<i64> = self.orders.values().filter(|o| o.user_id == uid && o.symbol == symbol).map(|o| o.id).collect();
        for id in ids {
            let need = self.fut_order_need(&self.orders[&id]);
            let o = self.orders.get_mut(&id).unwrap();
            let delta = need - o.locked;
            o.locked = need;
            let snap = o.clone();
            self.fut.entry(uid).or_default().locked += delta;
            self.save_order(&snap);
            self.emit_order(&snap);
        }
        self.save_fut(uid);
        self.commit();
        self.emit_fut(uid);
        Ok(())
    }

    /// What setting `leverage` on a market would do, without doing it. The same rules apply to
    /// a dry run (the UI's preview) and to the real change:
    /// * isolated, lower leverage: margin is topped up to entry value / leverage if it is below
    ///   that; margin added by hand stays;
    /// * isolated, higher leverage: margin is released down to entry value / leverage, but never
    ///   further than "remove margin" would allow (initial margin at today's mark, unrealised
    ///   profit not counted);
    /// * cross: only the initial margin tied up changes; raising is always allowed;
    /// * open orders on the market are re-reserved at the new leverage.
    /// Anything that needs more USD than is available is refused.
    pub fn plan_leverage(&self, uid: i64, symbol: &str, leverage: Decimal, now: i64) -> Result<LevPlan, String> {
        let mut plan = LevPlan::default();
        for o in self.orders.values().filter(|o| o.user_id == uid && o.symbol == symbol) {
            plan.lock_delta += self.fut_order_need_at(o, leverage) - o.locked;
        }
        let mut need = plan.lock_delta;
        if let Some(p) = self.positions.get(&(uid, symbol.to_string())) {
            if !self.tradable(symbol, now) {
                return Err("market data for this market is unavailable right now; try again shortly".into());
            }
            let mark = self.fut_mark(symbol).ok_or("no mark price right now; try again shortly")?;
            let value = p.size.abs() * mark;
            let (im_new, _) = self.rates(symbol, value, leverage);
            plan.has_position = true;
            plan.liq_before = self.liq_price(p);
            match p.mode {
                MarginMode::Isolated => {
                    let target = round8(p.size.abs() * p.entry * im_new);
                    let after = if leverage < p.leverage {
                        p.margin.max(target)
                    } else if leverage > p.leverage {
                        (p.margin - self.removable_at(p, leverage, mark)).max(target).min(p.margin)
                    } else {
                        p.margin
                    };
                    plan.margin_before = p.margin;
                    plan.margin_after = after;
                    need += after - p.margin;
                    let mut q = p.clone();
                    q.margin = after;
                    q.leverage = leverage;
                    plan.liq_after = self.liq_price(&q);
                }
                MarginMode::Cross => {
                    let (im_old, _) = self.rates(symbol, value, p.leverage);
                    plan.margin_before = round8(value * im_old);
                    plan.margin_after = round8(value * im_new);
                    need += plan.margin_after - plan.margin_before;
                    plan.liq_after = plan.liq_before; // cross liquidation depends on maintenance margin only
                }
            }
        }
        if need > Decimal::ZERO {
            let avail = self.fut_available(uid);
            if need > avail {
                return Err(format!(
                    "{leverage}x needs {} more {} than is available ({}); transfer USD in or choose a higher leverage",
                    ceil_dp(need, 2),
                    self.cfg.quote,
                    floor_dp(avail, 2)
                ));
            }
        }
        Ok(plan)
    }

    /// Add (positive) or remove (negative) margin on an isolated position.
    pub fn adjust_margin(&mut self, uid: i64, symbol: &str, amount: Decimal, now: i64) -> Result<(), String> {
        if amount.is_zero() || round8(amount) != amount {
            return Err("amount must be non-zero with at most 8 decimals".into());
        }
        let key = (uid, symbol.to_string());
        let mut p = self.positions.get(&key).cloned().ok_or("no open position on this market")?;
        if p.mode != MarginMode::Isolated {
            return Err("only isolated positions have their own margin; a cross position is backed by the whole futures wallet".into());
        }
        if !self.tradable(symbol, now) {
            return Err("market data for this market is unavailable right now; try again shortly".into());
        }
        if amount > Decimal::ZERO {
            let avail = self.fut_available(uid);
            if amount > avail {
                return Err(format!("only {} {} is available in the futures wallet", floor_dp(avail, 2), self.cfg.quote));
            }
        } else {
            let max = self.margin_removable(&p);
            if -amount > max {
                return Err(format!("at most {} can be removed while keeping the initial margin for {}x", floor_dp(max, 2), p.leverage));
            }
        }
        self.move_margin(&mut p, amount, now);
        p.updated_ms = now;
        self.save_pos(&p);
        self.positions.insert(key, p);
        self.commit();
        self.emit_fut(uid);
        Ok(())
    }

    /// How much margin an isolated position can give back while still holding the initial margin
    /// for its leverage at today's mark. Unrealised profit does not count.
    pub fn margin_removable(&self, p: &Position) -> Decimal {
        match self.fut_mark(&p.symbol) {
            Some(mark) => self.removable_at(p, p.leverage, mark),
            None => Decimal::ZERO,
        }
    }

    /// `margin_removable` at a given leverage and mark: the one floor both "remove margin" and a
    /// leverage increase respect.
    fn removable_at(&self, p: &Position, leverage: Decimal, mark: Decimal) -> Decimal {
        let value = p.size.abs() * mark;
        let (im, _) = self.rates(&p.symbol, value, leverage);
        let floor = ceil8(value * im) - p.upnl(mark).min(Decimal::ZERO);
        floor_dp((p.margin - floor).min(p.margin), 8).max(Decimal::ZERO)
    }

    /// Move USD between the futures wallet and an isolated position (positive = into the position).
    fn move_margin(&mut self, p: &mut Position, amount: Decimal, now: i64) {
        if amount.is_zero() {
            return;
        }
        let quote = self.cfg.quote.clone();
        let (wallet, posm) = (format!("fut:{}", p.user_id), format!("posm:{}", p.id));
        let txn = self.new_txn();
        if amount > Decimal::ZERO {
            self.transfer(txn, now, &wallet, &posm, &quote, amount, "margin", p.id);
        } else {
            self.transfer(txn, now, &posm, &wallet, &quote, -amount, "margin", p.id);
        }
        p.margin += amount;
        self.fut.entry(p.user_id).or_default().wallet -= amount;
        self.save_fut(p.user_id);
    }

    /// (initial, maintenance) margin rates: the leverage the user picked, but never
    /// below Kraken's tier for a position of this value.
    fn rates(&self, symbol: &str, value: Decimal, leverage: Decimal) -> (Decimal, Decimal) {
        let (im, mm) = self.perps.get(symbol).map(|p| p.rates(value)).unwrap_or((Decimal::ONE, d("0.5")));
        ((Decimal::ONE / leverage.max(Decimal::ONE)).max(im), mm)
    }

    /// (unrealised P&L, initial margin, maintenance margin) over the user's cross positions.
    pub fn cross_state(&self, uid: i64) -> (Decimal, Decimal, Decimal) {
        let (mut upnl, mut im, mut mm) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
        for p in self.positions.values().filter(|p| p.user_id == uid && p.mode == MarginMode::Cross) {
            let Some(mark) = self.fut_mark(&p.symbol) else { continue };
            let value = p.size.abs() * mark;
            let (i, m) = self.rates(&p.symbol, value, p.leverage);
            upnl += p.upnl(mark);
            im += value * i;
            mm += value * m;
        }
        (upnl, im, mm)
    }

    /// USD free for new orders or for moving back to spot. Unrealised profit does not count
    /// until it is realised; unrealised loss on cross positions does.
    pub fn fut_available(&self, uid: i64) -> Decimal {
        self.fut_free_raw(uid).max(Decimal::ZERO)
    }

    /// `fut_available` before clamping at zero: negative means cross positions are short of initial margin.
    fn fut_free_raw(&self, uid: i64) -> Decimal {
        let a = self.fut.get(&uid).cloned().unwrap_or_default();
        let (upnl, im, _) = self.cross_state(uid);
        a.wallet - a.locked - im + upnl.min(Decimal::ZERO)
    }

    /// Futures part of the user's equity: wallet + isolated margin + unrealised P&L.
    pub fn fut_equity(&self, uid: i64) -> Decimal {
        let mut eq = self.fut.get(&uid).map(|a| a.wallet).unwrap_or_default();
        for p in self.positions.values().filter(|p| p.user_id == uid) {
            eq += p.margin;
            if let Some(m) = self.fut_mark(&p.symbol) {
                eq += p.upnl(m);
            }
        }
        eq
    }

    /// Mark price at which the position would be liquidated (other cross positions held at today's marks).
    pub fn liq_price(&self, p: &Position) -> Option<Decimal> {
        let mark = self.fut_mark(&p.symbol)?;
        let q = p.size.abs();
        if q.is_zero() {
            return None;
        }
        let (_, m) = self.rates(&p.symbol, q * mark, p.leverage);
        let (equity_other, maint_other) = match p.mode {
            MarginMode::Isolated => (p.margin, Decimal::ZERO),
            MarginMode::Cross => {
                let (upnl, _, maint) = self.cross_state(p.user_id);
                let wallet = self.fut.get(&p.user_id).map(|a| a.wallet).unwrap_or_default();
                (wallet + upnl - p.upnl(mark), maint - q * mark * m)
            }
        };
        // solve equity_other + size * (L - entry) = maint_other + q * L * m for L
        let l = if p.size > Decimal::ZERO {
            (q * p.entry + maint_other - equity_other) / (q * (Decimal::ONE - m))
        } else {
            (q * p.entry + equity_other - maint_other) / (q * (Decimal::ONE + m))
        };
        (l > Decimal::ZERO).then_some(l)
    }

    pub fn fut_account_json(&self, uid: i64) -> Value {
        let a = self.fut.get(&uid).cloned().unwrap_or_default();
        let mut keys: Vec<&(i64, String)> = self.positions.keys().filter(|k| k.0 == uid).collect();
        keys.sort();
        let (mut upnl_all, mut margin_used, mut maint_all) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
        let mut positions = vec![];
        for k in keys {
            let p = &self.positions[k];
            let mark = self.fut_mark(&p.symbol).unwrap_or(p.entry);
            let value = p.size.abs() * mark;
            let (im, mm) = self.rates(&p.symbol, value, p.leverage);
            let upnl = p.upnl(mark);
            let margin = if p.mode == MarginMode::Isolated { p.margin } else { value * im };
            upnl_all += upnl;
            margin_used += margin;
            maint_all += value * mm;
            positions.push(json!({
                "id": p.id, "symbol": p.symbol, "side": if p.size > Decimal::ZERO { "long" } else { "short" },
                "size": s8(p.size.abs()), "entry": s8(p.entry), "mark": s8(mark), "value": s8(value),
                "liq_price": self.liq_price(p).map(s8), "margin_mode": p.mode.as_str(), "leverage": p.leverage.to_string(),
                "margin_removable": s8(if p.mode == MarginMode::Isolated { self.margin_removable(p) } else { Decimal::ZERO }),
                "maint_rate": mm.normalize().to_string(),
                "margin": s8(margin), "maint_margin": s8(value * mm), "unrealized_pnl": s8(upnl),
                "roe_pct": if margin > Decimal::ZERO { s8(upnl / margin * Decimal::ONE_HUNDRED) } else { "0".into() },
                "realized": s8(p.realized), "fees": s8(p.fees), "funding": s8(p.funding), "opened_ms": p.opened_ms,
            }));
        }
        let settings: serde_json::Map<String, Value> = self
            .cfg
            .perps
            .iter()
            .map(|s| {
                let (mode, lev) = self.fut_settings(uid, s);
                let max = self.perps.get(s).map(|p| p.max_leverage()).unwrap_or(Decimal::ONE);
                (s.clone(), json!({"margin_mode": mode.as_str(), "leverage": lev.to_string(), "max_leverage": max.to_string()}))
            })
            .collect();
        json!({
            "wallet": s8(a.wallet), "locked": s8(a.locked), "available": s8(self.fut_available(uid)),
            "unrealized_pnl": s8(upnl_all), "margin_used": s8(margin_used), "maintenance": s8(maint_all),
            "equity": s8(self.fut_equity(uid)), "positions": positions, "settings": settings,
        })
    }

    // ---------------------------------------------------------------- wallet transfers

    pub fn fut_transfer(&mut self, uid: i64, amount: Decimal, to_futures: bool, now: i64) -> Result<(), String> {
        if amount <= Decimal::ZERO || round8(amount) != amount {
            return Err("amount must be positive with at most 8 decimals".into());
        }
        let quote = self.cfg.quote.clone();
        let (from, to) = (format!("user:{uid}"), format!("fut:{uid}"));
        if to_futures {
            let free = self.free(uid, &quote);
            if amount > free {
                return Err(format!("insufficient spot {quote}: available {}", free.round_dp(2)));
            }
            self.balances.entry((uid, quote.clone())).or_default().total -= amount;
            self.fut.entry(uid).or_default().wallet += amount;
        } else {
            if !self.fut_feed_ok(now) && self.positions.keys().any(|k| k.0 == uid) {
                return Err("futures market data is unavailable, so open positions can't be valued; try again shortly".into());
            }
            let avail = self.fut_available(uid);
            if amount > avail {
                return Err(format!(
                    "only {} {quote} can leave the futures account now (the rest backs positions and open orders)",
                    floor_dp(avail, 2)
                ));
            }
            self.fut.entry(uid).or_default().wallet -= amount;
            self.balances.entry((uid, quote.clone())).or_default().total += amount;
        }
        let txn = self.new_txn();
        let (a, b) = if to_futures { (&from, &to) } else { (&to, &from) };
        self.transfer(txn, now, a, b, &quote, amount, "transfer", 0);
        self.save_bal(uid, &quote);
        self.save_fut(uid);
        self.commit();
        self.emit_fut(uid);
        Ok(())
    }

    // ---------------------------------------------------------------- orders

    /// Quantity of an order on `side` that would reduce the user's current position.
    pub fn fut_closable(&self, uid: i64, symbol: &str, side: Side) -> Decimal {
        match self.positions.get(&(uid, symbol.to_string())) {
            Some(p) if sgn(p.size) == -sign(side) => p.size.abs(),
            _ => Decimal::ZERO,
        }
    }

    /// Validate a perp order and return the USD to reserve for it: initial margin for the
    /// part that opens or grows a position, plus fees on the whole order.
    pub(crate) fn fut_check_order(&self, uid: i64, req: &OrderReq, ref_price: Decimal) -> Result<Decimal, String> {
        let perp = self.perps.get(&req.symbol).ok_or("unknown futures market")?;
        if perp.tiers.is_empty() {
            return Err("futures margin rules are still loading; try again in a moment".into());
        }
        let closable = self.fut_closable(uid, &req.symbol, req.side);
        if req.reduce_only {
            if closable.is_zero() {
                return Err("reduce-only: you have no position on the other side to reduce".into());
            }
            if req.qty > closable {
                return Err(format!("reduce-only: quantity is larger than the position ({})", closable.normalize()));
            }
        }
        let open_qty = (req.qty - closable).max(Decimal::ZERO);
        let held = self.positions.get(&(uid, req.symbol.clone())).map(|p| p.size.abs()).unwrap_or_default();
        let new_size = if closable.is_zero() { held + open_qty } else { open_qty };
        if let Some(maxp) = perp.max_position {
            if new_size > maxp {
                return Err(format!("the position would exceed Kraken's limit of {} contracts", maxp.normalize()));
            }
        }
        let (_, lev) = self.fut_settings(uid, &req.symbol);
        let (im, _) = self.rates(&req.symbol, new_size * ref_price, lev);
        // a closing part needs no reservation: its fee is paid from what the close hands back
        let reserve = ceil8(open_qty * ref_price * (im + self.cfg.fut_max_fee()));
        let avail = self.fut_available(uid);
        if reserve > avail {
            return Err(format!(
                "insufficient futures margin: need {}, available {} {}; transfer USD from spot to your futures account first",
                reserve.round_dp(2),
                avail.round_dp(2),
                self.cfg.quote
            ));
        }
        Ok(reserve)
    }

    /// What an open perp order still needs reserved for its unfilled part.
    fn fut_order_need(&self, o: &Order) -> Decimal {
        self.fut_order_need_at(o, self.fut_settings(o.user_id, &o.symbol).1)
    }

    /// The same at a given leverage: initial margin and fees for the part that would open or grow a position.
    fn fut_order_need_at(&self, o: &Order, leverage: Decimal) -> Decimal {
        let rem = o.remaining();
        if rem <= Decimal::ZERO || o.reduce_only {
            return Decimal::ZERO;
        }
        let px = o.limit_price.or(o.trigger_price).or_else(|| self.fut_mark(&o.symbol)).unwrap_or_default();
        let closable = self.fut_closable(o.user_id, &o.symbol, o.side);
        let open_qty = (rem - closable).max(Decimal::ZERO);
        let held = self.positions.get(&(o.user_id, o.symbol.clone())).map(|p| p.size.abs()).unwrap_or_default();
        let (im, _) = self.rates(&o.symbol, (held + open_qty) * px, leverage);
        ceil8(open_qty * px * (im + self.cfg.fut_max_fee()))
    }

    /// A resting perp order filled as maker may open or grow a position only as far as its margin
    /// allows: it may have been placed to close a position that is gone by now. With no margin
    /// for any of it, it is cancelled rather than left to fill unbacked later.
    pub(crate) fn fut_maker_cap(&mut self, oid: i64, price: Decimal, qty: Decimal, now: i64) -> Decimal {
        let cap = self.fut_max_fill(oid, price);
        if cap.is_zero() {
            self.finish(oid, Status::Canceled, Some("not enough futures margin to open a position with this order"), now);
        }
        qty.min(cap)
    }

    pub(crate) fn fut_lock(&mut self, uid: i64, amount: Decimal) {
        self.fut.entry(uid).or_default().locked += amount;
        self.save_fut(uid);
    }

    pub(crate) fn fut_unlock(&mut self, uid: i64, amount: Decimal) {
        self.fut.entry(uid).or_default().locked -= amount;
        self.save_fut(uid);
    }

    /// Largest quantity a perp order can fill at `price` with the margin it has.
    pub(crate) fn fut_max_fill(&self, oid: i64, price: Decimal) -> Decimal {
        let o = &self.orders[&oid];
        let closable = self.fut_closable(o.user_id, &o.symbol, o.side);
        if o.reduce_only {
            return closable;
        }
        let held = self.positions.get(&(o.user_id, o.symbol.clone())).map(|p| p.size.abs()).unwrap_or_default();
        let (_, lev) = self.fut_settings(o.user_id, &o.symbol);
        let (im, _) = self.rates(&o.symbol, (held + o.remaining()) * price, lev);
        let fee = self.cfg.fut_taker_fee;
        let funds = self.fut_available(o.user_id) + o.locked - closable * price * fee - d("0.00000002");
        let qinc = self.instruments.get(&o.symbol).map(|i| i.qty_increment).unwrap_or_default();
        closable + floor_to(funds.max(Decimal::ZERO) / (price * (im + fee)), qinc)
    }

    /// Settle a perp fill: reduce the opposite position (realising P&L and releasing
    /// isolated margin), then open or grow with the rest, then charge the fee.
    pub(crate) fn fut_apply_fill(&mut self, oid: i64, price: Decimal, qty: Decimal, taker: bool, now: i64) {
        let cfg = self.cfg.clone();
        let quote = cfg.quote.clone();
        let liquidation = self.liquidating;
        let (uid, symbol, side, reduce_only) = {
            let o = &self.orders[&oid];
            (o.user_id, o.symbol.clone(), o.side, o.reduce_only)
        };
        let closable = self.fut_closable(uid, &symbol, side);
        let qty = if reduce_only { qty.min(closable) } else { qty };
        if qty <= Decimal::ZERO {
            self.finish(oid, Status::Canceled, Some("reduce-only: the position is already closed"), now);
            return;
        }
        let (mode, lev) = self.fut_settings(uid, &symbol);
        let dir = sign(side);
        let cost = round8(price * qty);
        let fee = round8(cost * if taker { cfg.fut_taker_fee } else { cfg.fut_maker_fee });
        let txn = self.new_txn();
        let wallet_acct = format!("fut:{uid}");
        let mut wallet_delta = Decimal::ZERO;
        let mut lock_used = Decimal::ZERO;
        let mut pos = self.positions.remove(&(uid, symbol.clone()));
        let mut rest = qty;

        // 1. reduce an opposite position
        if let Some(p) = pos.as_mut().filter(|p| sgn(p.size) == -dir) {
            let c = rest.min(p.size.abs());
            let pnl = round8(c * (price - p.entry) * sgn(p.size));
            let release = if c == p.size.abs() { p.margin } else { round8(p.margin * c / p.size.abs()) };
            p.margin -= release;
            p.size += dir * c;
            p.realized += pnl;
            p.closed_qty += c;
            p.closed_value += round8(c * price);
            p.updated_ms = now;
            rest -= c;
            self.transfer(txn, now, &format!("posm:{}", p.id), &wallet_acct, &quote, release, "margin", p.id);
            self.pnl_entry(txn, now, &wallet_acct, pnl, p.id);
            let mut net = release + pnl;
            if p.mode == MarginMode::Isolated && net < Decimal::ZERO {
                // a loss bigger than the margin released for this part comes out of the margin the
                // rest of the position still holds; only past that does the insurance fund pay
                let more = (-net).min(p.margin);
                if more > Decimal::ZERO {
                    self.transfer(txn, now, &format!("posm:{}", p.id), &wallet_acct, &quote, more, "margin", p.id);
                    p.margin -= more;
                    net += more;
                }
                if net < Decimal::ZERO {
                    self.transfer(txn, now, "insurance", &wallet_acct, &quote, -net, "insurance", p.id);
                    net = Decimal::ZERO;
                }
            }
            wallet_delta += net;
        }
        let mut closed = None;
        if pos.as_ref().map(|p| p.size.is_zero()).unwrap_or(false) {
            closed = pos.take();
        }

        // 2. open or grow with what is left
        if rest > Decimal::ZERO {
            if pos.is_none() {
                pos = Some(self.new_position(uid, &symbol, dir > Decimal::ZERO, mode, lev, price, now));
            }
            let p = pos.as_mut().unwrap();
            let held = p.size.abs();
            p.entry = ((held * p.entry + rest * price) / (held + rest)).round_dp(12);
            p.size += dir * rest;
            p.updated_ms = now;
            let (im_rate, _) = self.rates(&symbol, p.size.abs() * price, p.leverage);
            let im = round8(rest * price * im_rate);
            if p.mode == MarginMode::Isolated {
                self.transfer(txn, now, &wallet_acct, &format!("posm:{}", p.id), &quote, im, "margin", p.id);
                p.margin += im;
                wallet_delta -= im;
            }
            lock_used += im;
        }

        // 3. fee
        self.transfer(txn, now, &wallet_acct, "fees", &quote, fee, "fee", oid);
        wallet_delta -= fee;
        lock_used += fee;
        if let Some(p) = pos.as_mut().or(closed.as_mut()) {
            p.fees += fee;
        }

        if let Some(c) = closed {
            self.close_position_row(&c, if liquidation { "liquidated" } else { "closed" }, now);
        }
        if let Some(p) = pos {
            self.save_pos(&p);
            self.positions.insert((uid, symbol.clone()), p);
        }
        let acct = self.fut.entry(uid).or_default();
        acct.wallet += wallet_delta;
        let o = self.orders.get_mut(&oid).unwrap();
        let used = lock_used.min(o.locked);
        o.locked -= used;
        acct.locked -= used;
        o.filled += qty;
        o.cum_cost += cost;
        o.fee += fee;
        o.updated_ms = now;
        let done = o.remaining() <= Decimal::ZERO;
        if done {
            o.status = Status::Filled;
        }
        let snap = o.clone();
        let need = if done { Decimal::ZERO } else { self.fut_order_need(&snap) };
        if snap.locked > need {
            let extra = snap.locked - need;
            self.orders.get_mut(&oid).unwrap().locked = need;
            self.fut.entry(uid).or_default().locked -= extra;
        }
        let snap = self.orders[&oid].clone();
        self.save_fut(uid);
        let fid = self.record_fill(&snap, price, qty, cost, fee, taker, now);
        self.save_order(&snap);
        if done {
            self.orders.remove(&oid);
        }
        self.publish_fill(&snap, fid, price, qty, cost, fee, taker, now);
        self.emit_fut(uid);
        if !done && reduce_only && self.fut_closable(uid, &symbol, side).is_zero() {
            self.finish(oid, Status::Canceled, Some("reduce-only: the position is closed"), now);
        }
    }

    /// Realised P&L against the market: profit comes from it, loss goes to it.
    fn pnl_entry(&mut self, txn: i64, now: i64, wallet: &str, pnl: Decimal, rf: i64) {
        let quote = self.cfg.quote.clone();
        if pnl > Decimal::ZERO {
            self.transfer(txn, now, "market", wallet, &quote, pnl, "pnl", rf);
        } else if pnl < Decimal::ZERO {
            self.transfer(txn, now, wallet, "market", &quote, -pnl, "pnl", rf);
        }
    }

    // ---------------------------------------------------------------- funding and liquidation

    /// Once a second: pay funding that has come due, then liquidate under-margined positions.
    pub fn fut_tick(&mut self, now: i64) {
        if !self.fut_feed_ok(now) {
            return; // without live marks nothing can be valued fairly
        }
        self.run_funding(now);
        self.check_liquidations(now);
        self.commit();
    }

    fn run_funding(&mut self, now: i64) {
        // periods the exchange has already rolled past (whatever our clock says),
        // then any that are due by our clock
        let mut due: Vec<(String, i64, Decimal)> = std::mem::take(&mut self.funding_due);
        due.extend(self.perps.values().filter_map(|p| {
            let (ts, rate) = (p.next_funding_ms?, p.funding_rate?);
            (now >= ts).then(|| (p.symbol.clone(), ts, rate))
        }));
        let quote = self.cfg.quote.clone();
        for (sym, ts, rate) in due {
            if self.funding_done.get(&sym).copied().unwrap_or(0) >= ts {
                continue;
            }
            let Some(mark) = self.fut_mark(&sym) else { continue };
            let keys: Vec<(i64, String)> = self.positions.keys().filter(|k| k.1 == sym).cloned().collect();
            for k in keys {
                let mut p = self.positions.remove(&k).unwrap();
                let amount = -round8(p.size * mark * rate); // received (+) or paid (-)
                if !amount.is_zero() {
                    let txn = self.new_txn();
                    let acct = match p.mode {
                        MarginMode::Isolated => format!("posm:{}", p.id),
                        MarginMode::Cross => format!("fut:{}", p.user_id),
                    };
                    if amount > Decimal::ZERO {
                        self.transfer(txn, ts, "funding", &acct, &quote, amount, "funding", p.id);
                    } else {
                        self.transfer(txn, ts, &acct, "funding", &quote, -amount, "funding", p.id);
                    }
                    match p.mode {
                        MarginMode::Isolated => p.margin += amount,
                        MarginMode::Cross => {
                            self.fut.entry(p.user_id).or_default().wallet += amount;
                            self.save_fut(p.user_id);
                        }
                    }
                    p.funding += amount;
                    p.updated_ms = now;
                    self.ensure_tx();
                    self.db
                        .execute(
                            "INSERT INTO funding(user_id, position_id, season_id, symbol, ts_ms, rate, mark, amount)
                             VALUES(?,?,?,?,?,?,?,?)",
                            params![
                                p.user_id,
                                p.id,
                                self.season_id,
                                sym,
                                ts,
                                rate.normalize().to_string(),
                                mark.normalize().to_string(),
                                amount.normalize().to_string()
                            ],
                        )
                        .unwrap();
                    self.save_pos(&p);
                    self.emit_fut(p.user_id);
                }
                self.positions.insert(k, p);
            }
            self.funding_done.insert(sym.clone(), ts);
            self.ensure_tx();
            self.db
                .execute(
                    "INSERT INTO funding_runs(symbol, ts_ms) VALUES(?,?) ON CONFLICT(symbol) DO UPDATE SET ts_ms=excluded.ts_ms",
                    params![sym, ts],
                )
                .unwrap();
        }
    }

    fn check_liquidations(&mut self, now: i64) {
        let mut isolated = vec![];
        let mut cross = BTreeSet::new();
        for p in self.positions.values() {
            if !self.tradable(&p.symbol, now) {
                continue;
            }
            match p.mode {
                MarginMode::Isolated => {
                    let Some(mark) = self.fut_mark(&p.symbol) else { continue };
                    let value = p.size.abs() * mark;
                    let (_, mm) = self.rates(&p.symbol, value, p.leverage);
                    if p.margin + p.upnl(mark) <= value * mm {
                        isolated.push((p.user_id, p.symbol.clone()));
                    }
                }
                MarginMode::Cross => {
                    cross.insert(p.user_id);
                }
            }
        }
        for (uid, sym) in isolated {
            self.liquidate(uid, vec![sym], now);
        }
        for uid in cross {
            let (upnl, _, mm) = self.cross_state(uid);
            let wallet = self.fut.get(&uid).map(|a| a.wallet).unwrap_or_default();
            if wallet + upnl <= mm {
                let syms: Vec<String> = self
                    .positions
                    .values()
                    .filter(|p| p.user_id == uid && p.mode == MarginMode::Cross && self.tradable(&p.symbol, now))
                    .map(|p| p.symbol.clone())
                    .collect();
                self.liquidate(uid, syms, now);
            }
        }
    }

    /// Close positions at market against the real book, then charge the liquidation fee.
    fn liquidate(&mut self, uid: i64, symbols: Vec<String>, now: i64) {
        let quote = self.cfg.quote.clone();
        for sym in symbols {
            let Some(p) = self.positions.get(&(uid, sym.clone())).cloned() else { continue };
            // the user's other orders on this market could reopen the position
            let ids: Vec<i64> = self.orders.values().filter(|o| o.user_id == uid && o.symbol == sym).map(|o| o.id).collect();
            for id in ids {
                self.finish(id, Status::Canceled, Some("cancelled by liquidation"), now);
            }
            let mark = self.fut_mark(&sym).unwrap_or(p.entry);
            let before = self.fut.get(&uid).map(|a| a.wallet).unwrap_or_default();
            let closed = self.force_close(uid, &sym, now);
            if closed.is_zero() {
                continue; // nothing to sell into right now; the next check retries
            }
            let wallet = self.fut.get(&uid).map(|a| a.wallet).unwrap_or_default();
            // the fee comes out of what the position handed back (isolated) or the wallet (cross)
            let room = if p.mode == MarginMode::Isolated { wallet - before } else { wallet };
            let liq_fee = round8(closed * mark * self.cfg.liq_fee).min(room.max(Decimal::ZERO));
            if liq_fee > Decimal::ZERO {
                let txn = self.new_txn();
                self.transfer(txn, now, &format!("fut:{uid}"), "insurance", &quote, liq_fee, "liquidation_fee", p.id);
                self.fut.entry(uid).or_default().wallet -= liq_fee;
                self.add_position_fee(p.id, &sym, uid, liq_fee);
            }
            tracing::info!("liquidated user {uid} {sym}: {} contracts at mark {mark}, fee {liq_fee}", closed);
            let payload = json!({
                "type": "liquidation", "symbol": sym, "side": if p.size > Decimal::ZERO { "long" } else { "short" },
                "size": s8(closed), "mark": s8(mark), "fee": s8(liq_fee), "margin_mode": p.mode.as_str(),
            })
            .to_string();
            self.publish(crate::engine::Event::User { user_id: uid, payload: payload.into() });
        }
        // a cross account pushed below zero is topped back up by the insurance fund
        let open_cross = self.positions.values().any(|p| p.user_id == uid && p.mode == MarginMode::Cross);
        let wallet = self.fut.get(&uid).map(|a| a.wallet).unwrap_or_default();
        if wallet < Decimal::ZERO && !open_cross {
            let txn = self.new_txn();
            self.transfer(txn, now, "insurance", &format!("fut:{uid}"), &quote, -wallet, "insurance", 0);
            self.fut.entry(uid).or_default().wallet = Decimal::ZERO;
        }
        self.save_fut(uid);
        self.emit_fut(uid);
        self.commit();
    }

    /// Market-close a position immediately (no latency). Returns the quantity closed;
    /// a thin book may leave some open, to be retried on the next check.
    fn force_close(&mut self, uid: i64, symbol: &str, now: i64) -> Decimal {
        let Some(p) = self.positions.get(&(uid, symbol.to_string())) else { return Decimal::ZERO };
        let (side, qty) = (if p.size > Decimal::ZERO { Side::Sell } else { Side::Buy }, p.size.abs());
        let o = self.insert_order(Order {
            id: 0,
            user_id: uid,
            season_id: self.season_id,
            symbol: symbol.to_string(),
            side,
            otype: OType::Market,
            tif: crate::engine::Tif::Gtc,
            post_only: false,
            reduce_only: true,
            qty,
            limit_price: None,
            trigger_price: None,
            filled: Decimal::ZERO,
            cum_cost: Decimal::ZERO,
            fee: Decimal::ZERO,
            status: Status::Pending,
            locked: Decimal::ZERO,
            queue_ahead: None,
            note: Some("liquidation".into()),
            reason: None,
            created_ms: now,
            updated_ms: now,
        });
        self.liquidating = true;
        self.take(o.id, None, now);
        self.liquidating = false;
        if self.orders.contains_key(&o.id) {
            self.finish(o.id, Status::Canceled, Some("liquidation: the book was too thin; retrying"), now);
        }
        let left = self.positions.get(&(uid, symbol.to_string())).map(|p| p.size.abs()).unwrap_or_default();
        qty - left
    }

    // ---------------------------------------------------------------- persistence

    #[allow(clippy::too_many_arguments)]
    fn new_position(
        &mut self,
        uid: i64,
        symbol: &str,
        long: bool,
        mode: MarginMode,
        leverage: Decimal,
        price: Decimal,
        now: i64,
    ) -> Position {
        self.ensure_tx();
        self.db
            .execute(
                "INSERT INTO positions(user_id, season_id, symbol, side, size, entry, mode, leverage, margin, realized, fees,
                   funding, closed_qty, closed_value, opened_ms, updated_ms)
                 VALUES(?,?,?,?,'0',?,?,?,'0','0','0','0','0','0',?,?)",
                params![
                    uid,
                    self.season_id,
                    symbol,
                    if long { "long" } else { "short" },
                    price.to_string(),
                    mode.as_str(),
                    leverage.to_string(),
                    now,
                    now
                ],
            )
            .expect("insert position");
        Position {
            id: self.db.last_insert_rowid(),
            user_id: uid,
            symbol: symbol.to_string(),
            long,
            size: Decimal::ZERO,
            entry: price,
            mode,
            leverage,
            margin: Decimal::ZERO,
            realized: Decimal::ZERO,
            fees: Decimal::ZERO,
            funding: Decimal::ZERO,
            closed_qty: Decimal::ZERO,
            closed_value: Decimal::ZERO,
            opened_ms: now,
            updated_ms: now,
        }
    }

    /// Book a fee on a position whether it is still open (partly liquidated) or already closed.
    fn add_position_fee(&mut self, id: i64, symbol: &str, uid: i64, fee: Decimal) {
        if let Some(p) = self.positions.get_mut(&(uid, symbol.to_string())).filter(|p| p.id == id) {
            p.fees += fee;
            let p = p.clone();
            self.save_pos(&p);
            return;
        }
        let cur: String = self.db.query_row("SELECT fees FROM positions WHERE id=?", [id], |r| r.get(0)).unwrap();
        let total = parse_dec(&cur).unwrap_or_default() + fee;
        self.ensure_tx();
        self.db.execute("UPDATE positions SET fees=? WHERE id=?", params![total.to_string(), id]).unwrap();
    }

    fn save_pos(&mut self, p: &Position) {
        self.ensure_tx();
        self.db
            .execute(
                "UPDATE positions SET size=?, entry=?, margin=?, realized=?, fees=?, funding=?, closed_qty=?,
                   closed_value=?, updated_ms=?, leverage=? WHERE id=?",
                params![
                    p.size.to_string(),
                    p.entry.to_string(),
                    p.margin.to_string(),
                    p.realized.to_string(),
                    p.fees.to_string(),
                    p.funding.to_string(),
                    p.closed_qty.to_string(),
                    p.closed_value.to_string(),
                    p.updated_ms,
                    p.leverage.to_string(),
                    p.id
                ],
            )
            .expect("save position");
    }

    fn close_position_row(&mut self, p: &Position, reason: &str, now: i64) {
        self.save_pos(p);
        self.db
            .execute("UPDATE positions SET closed_ms=?, close_reason=? WHERE id=?", params![now, reason, p.id])
            .expect("close position");
        self.emit_fut(p.user_id);
    }

    pub(crate) fn save_fut(&mut self, uid: i64) {
        self.ensure_tx();
        let a = self.fut.get(&uid).cloned().unwrap_or_default();
        self.db
            .execute(
                "INSERT INTO fut_accounts(user_id, wallet, locked) VALUES(?,?,?)
                 ON CONFLICT(user_id) DO UPDATE SET wallet=excluded.wallet, locked=excluded.locked",
                params![uid, a.wallet.to_string(), a.locked.to_string()],
            )
            .expect("save futures account");
    }

    fn emit_fut(&self, uid: i64) {
        self.publish(crate::engine::Event::User { user_id: uid, payload: r#"{"type":"futures"}"#.into() });
    }

    pub(crate) fn fut_load(&mut self) {
        let accts: Vec<(i64, String, String)> = {
            let mut st = self.db.prepare("SELECT user_id, wallet, locked FROM fut_accounts").unwrap();
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for (u, w, l) in accts {
            self.fut.insert(u, FutAcct { wallet: parse_dec(&w).unwrap_or_default(), locked: parse_dec(&l).unwrap_or_default() });
        }
        let prefs: Vec<(i64, String, String, String)> = {
            let mut st = self.db.prepare("SELECT user_id, symbol, mode, leverage FROM fut_settings").unwrap();
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for (u, s, m, l) in prefs {
            if let (Some(m), Some(l)) = (MarginMode::parse(&m), parse_dec(&l)) {
                self.fut_prefs.insert((u, s), (m, l));
            }
        }
        let open: Vec<Position> = {
            let mut st = self.db.prepare(&format!("SELECT {POS_COLS} FROM positions WHERE closed_ms IS NULL")).unwrap();
            let rows = st.query_map([], row_to_pos).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for p in open {
            self.positions.insert((p.user_id, p.symbol.clone()), p);
        }
        let runs: Vec<(String, i64)> = {
            let mut st = self.db.prepare("SELECT symbol, ts_ms FROM funding_runs").unwrap();
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        self.funding_done.extend(runs);
    }

    // ---------------------------------------------------------------- seasons, audit, history

    /// New season: settle every position at the mark price, then sweep futures wallets to the treasury.
    pub(crate) fn fut_season_reset(&mut self, now: i64) {
        let quote = self.cfg.quote.clone();
        let keys: Vec<(i64, String)> = self.positions.keys().cloned().collect();
        for k in keys {
            let mut p = self.positions.remove(&k).unwrap();
            let mark = self.fut_mark(&p.symbol).unwrap_or(p.entry);
            let pnl = round8(p.upnl(mark));
            let wallet = format!("fut:{}", p.user_id);
            let txn = self.new_txn();
            self.transfer(txn, now, &format!("posm:{}", p.id), &wallet, &quote, p.margin, "margin", p.id);
            self.pnl_entry(txn, now, &wallet, pnl, p.id);
            let mut delta = p.margin + pnl;
            if p.mode == MarginMode::Isolated && delta < Decimal::ZERO {
                self.transfer(txn, now, "insurance", &wallet, &quote, -delta, "insurance", p.id);
                delta = Decimal::ZERO;
            }
            self.fut.entry(p.user_id).or_default().wallet += delta;
            p.realized += pnl;
            p.closed_qty += p.size.abs();
            p.closed_value += round8(p.size.abs() * mark);
            p.size = Decimal::ZERO;
            p.margin = Decimal::ZERO;
            p.updated_ms = now;
            self.close_position_row(&p, "season ended", now);
        }
        let uids: Vec<i64> = self.fut.keys().copied().collect();
        for uid in uids {
            let wallet = self.fut[&uid].wallet;
            let txn = self.new_txn();
            if wallet > Decimal::ZERO {
                self.transfer(txn, now, &format!("fut:{uid}"), "treasury", &quote, wallet, "season_reset", self.season_id);
            } else if wallet < Decimal::ZERO {
                self.transfer(txn, now, "insurance", &format!("fut:{uid}"), &quote, -wallet, "insurance", 0);
            }
            self.fut.insert(uid, FutAcct::default());
            self.save_fut(uid);
        }
    }

    pub(crate) fn fut_reconcile(&self, sums: &HashMap<(String, String), Decimal>, problems: &mut Vec<String>) {
        let q = &self.cfg.quote;
        let ledger = |acct: String| sums.get(&(acct, q.clone())).copied().unwrap_or(Decimal::ZERO);
        for (uid, a) in &self.fut {
            let l = ledger(format!("fut:{uid}"));
            if l != a.wallet {
                problems.push(format!("futures wallet of user {uid}: {} != ledger {l}", a.wallet));
            }
            let locked: Decimal =
                self.orders.values().filter(|o| o.user_id == *uid && self.perps.contains_key(&o.symbol)).map(|o| o.locked).sum();
            if a.wallet < Decimal::ZERO && !self.positions.values().any(|p| p.user_id == *uid && p.mode == MarginMode::Cross) {
                problems.push(format!("futures wallet of user {uid} is negative ({}) with no cross position", a.wallet));
            }
            if locked != a.locked || a.locked < Decimal::ZERO {
                problems.push(format!("futures wallet of user {uid}: locked {} != open-order reservations {locked}", a.locked));
            }
        }
        for p in self.positions.values() {
            let l = ledger(format!("posm:{}", p.id));
            if l != p.margin {
                problems.push(format!("position {} margin {} != ledger {l}", p.id, p.margin));
            }
            if p.mode == MarginMode::Cross && !p.margin.is_zero() {
                problems.push(format!("cross position {} holds isolated margin {}", p.id, p.margin));
            }
        }
        let open: BTreeSet<String> = self.positions.values().map(|p| format!("posm:{}", p.id)).collect();
        for ((acct, asset), v) in sums {
            if asset != q || v.is_zero() {
                continue;
            }
            if acct.starts_with("posm:") && !open.contains(acct) {
                problems.push(format!("closed position account {acct} still holds {v}"));
            }
            if let Some(uid) = acct.strip_prefix("fut:").and_then(|u| u.parse::<i64>().ok()) {
                if !self.fut.contains_key(&uid) {
                    problems.push(format!("ledger account {acct} holds {v} but the user has no futures wallet"));
                }
            }
        }
    }

    pub fn position_history(&self, uid: i64, limit: i64) -> Vec<Value> {
        let mut st = self
            .db
            .prepare(&format!(
                "SELECT {POS_COLS}, closed_ms, close_reason FROM positions
                 WHERE user_id=? AND season_id=? AND closed_ms IS NOT NULL ORDER BY closed_ms DESC LIMIT ?"
            ))
            .unwrap();
        let rows = st
            .query_map(params![uid, self.season_id, limit], |r| {
                let p = row_to_pos(r)?;
                let closed: i64 = r.get(17)?;
                let reason: Option<String> = r.get(18)?;
                Ok((p, closed, reason))
            })
            .unwrap();
        rows.map(|r| r.unwrap())
            .map(|(p, closed, reason)| {
                let exit = (!p.closed_qty.is_zero()).then(|| s8(p.closed_value / p.closed_qty));
                json!({
                    "id": p.id, "symbol": p.symbol, "side": if p.long { "long" } else { "short" },
                    "size": s8(p.closed_qty), "entry": s8(p.entry), "exit": exit, "margin_mode": p.mode.as_str(),
                    "leverage": p.leverage.to_string(), "realized": s8(p.realized), "fees": s8(p.fees),
                    "funding": s8(p.funding), "net": s8(p.realized - p.fees + p.funding),
                    "opened_ms": p.opened_ms, "closed_ms": closed, "reason": reason,
                })
            })
            .collect()
    }

    pub fn funding_history(&self, uid: i64, limit: i64) -> Vec<Value> {
        let mut st = self
            .db
            .prepare(
                "SELECT symbol, ts_ms, rate, mark, amount FROM funding WHERE user_id=? AND season_id=? ORDER BY id DESC LIMIT ?",
            )
            .unwrap();
        let rows = st
            .query_map(params![uid, self.season_id, limit], |r| {
                Ok(json!({
                    "symbol": r.get::<_, String>(0)?, "ts": r.get::<_, i64>(1)?, "rate": r.get::<_, String>(2)?,
                    "mark": r.get::<_, String>(3)?, "amount": r.get::<_, String>(4)?,
                }))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }
}
