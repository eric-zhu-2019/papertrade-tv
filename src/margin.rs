//! Margin trading on spot pairs (Kraken-style): an order with `leverage` (2x and up) opens a
//! position instead of moving coins, so a margin sell is a short. One net position per pair.
//!
//! Collateral is the user's spot USD. Positions are not moved out of it: their initial margin
//! (value ÷ leverage at entry) and any unrealised loss are held back, so spot buys and transfers
//! can't spend them. Fees follow Kraken: the trading fee, an opening fee on the value opened and a
//! rollover fee every 4 hours, both at the rate of the borrowed currency (USD for a long, the coin
//! for a short). When equity (USD + unrealised P&L) falls to Kraken's margin stop (40% of the
//! margin used) every position is closed at market, as on Kraken.
//!
//! Ledger, all in USD: realised P&L against `market`, fees to `fees`, the liquidation fee to
//! `insurance`, which also covers an account pushed below zero.

use crate::book::Side;
use crate::engine::{Engine, Event, OType, Order, OrderReq, Status, Tif};
use crate::util::*;
use rusqlite::params;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Clone, Debug)]
pub struct MarginRule {
    pub max_long: Decimal,
    pub max_short: Decimal,
    /// Opening and per-rollover fee as a fraction of position value.
    pub long_rate: Decimal,
    pub short_rate: Decimal,
    /// Margin level (equity ÷ margin used) of Kraken's margin call and liquidation.
    pub call_level: Decimal,
    pub stop_level: Decimal,
}

impl MarginRule {
    /// Kraken's published values, used until (or unless) the live ones are fetched.
    pub fn default_for(base: &str) -> MarginRule {
        MarginRule {
            max_long: Decimal::from(10),
            max_short: Decimal::from(10),
            long_rate: d("0.00025"), // borrowing USD
            short_rate: if base == "BTC" { d("0.0001") } else { d("0.0002") },
            call_level: d("0.8"),
            stop_level: d("0.4"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MPos {
    pub id: i64,
    pub user_id: i64,
    pub symbol: String,
    /// Coins; positive is long, negative short.
    pub size: Decimal,
    pub entry: Decimal,
    /// Leverage of the order that opened the position (for display; margin is what counts).
    pub leverage: Decimal,
    /// Initial margin held back from spot USD.
    pub margin: Decimal,
    pub realized: Decimal,
    /// Trading, opening, rollover and liquidation fees.
    pub fees: Decimal,
    pub rollover: Decimal,
    pub closed_qty: Decimal,
    pub closed_value: Decimal,
    pub opened_ms: i64,
    pub updated_ms: i64,
    pub next_rollover_ms: i64,
}

impl MPos {
    pub fn upnl(&self, mark: Decimal) -> Decimal {
        self.size * (mark - self.entry)
    }
}

const MPOS_COLS: &str = "id, user_id, symbol, size, entry, leverage, margin, realized, fees, rollover, \
     closed_qty, closed_value, opened_ms, updated_ms, next_rollover_ms";

fn row_to_mpos(r: &rusqlite::Row) -> rusqlite::Result<MPos> {
    let s = |i: usize| -> rusqlite::Result<Decimal> { Ok(parse_dec(&r.get::<_, String>(i)?).unwrap_or(Decimal::ZERO)) };
    Ok(MPos {
        id: r.get(0)?,
        user_id: r.get(1)?,
        symbol: r.get(2)?,
        size: s(3)?,
        entry: s(4)?,
        leverage: s(5)?,
        margin: s(6)?,
        realized: s(7)?,
        fees: s(8)?,
        rollover: s(9)?,
        closed_qty: s(10)?,
        closed_value: s(11)?,
        opened_ms: r.get(12)?,
        updated_ms: r.get(13)?,
        next_rollover_ms: r.get(14)?,
    })
}

fn sign(side: Side) -> Decimal {
    if side == Side::Buy {
        Decimal::ONE
    } else {
        -Decimal::ONE
    }
}

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
    // ---------------------------------------------------------------- rules and state

    pub fn margin_rule(&self, symbol: &str) -> MarginRule {
        self.margin_rules.get(symbol).cloned().unwrap_or_else(|| MarginRule::default_for(symbol.split('/').next().unwrap_or("")))
    }

    /// Opening / rollover fee rate: the borrowed currency's (USD for a long, the coin for a short).
    fn margin_rate(&self, symbol: &str, long: bool) -> Decimal {
        let r = self.margin_rule(symbol);
        if long {
            r.long_rate
        } else {
            r.short_rate
        }
    }

    /// (net unrealised P&L, margin used, unrealised losses) over the user's margin positions.
    pub fn margin_state(&self, uid: i64) -> (Decimal, Decimal, Decimal) {
        let (mut upnl, mut used, mut losses) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
        for p in self.mpos.values().filter(|p| p.user_id == uid) {
            let u = self.mark(&p.symbol).map(|m| p.upnl(m)).unwrap_or_default();
            upnl += u;
            used += p.margin;
            losses += (-u).max(Decimal::ZERO);
        }
        (upnl, used, losses)
    }

    /// Spot USD held back by margin positions: their margin plus each one's unrealised loss
    /// (a profit on one position does not free USD for another's loss).
    pub fn margin_hold(&self, uid: i64) -> Decimal {
        if !self.mpos.keys().any(|k| k.0 == uid) {
            return Decimal::ZERO;
        }
        let (_, used, losses) = self.margin_state(uid);
        used + losses
    }

    /// Margin equity: spot USD plus unrealised P&L of margin positions.
    pub fn margin_equity(&self, uid: i64) -> Decimal {
        let usd = self.balances.get(&(uid, self.cfg.quote.clone())).map(|b| b.total).unwrap_or_default();
        usd + self.margin_state(uid).0
    }

    /// Equity ÷ margin used; None without positions.
    pub fn margin_level(&self, uid: i64) -> Option<Decimal> {
        let (upnl, used, _) = self.margin_state(uid);
        let usd = self.balances.get(&(uid, self.cfg.quote.clone())).map(|b| b.total).unwrap_or_default();
        (used > Decimal::ZERO).then(|| (usd + upnl) / used)
    }

    /// Quantity of a margin order on `side` that would reduce the current position.
    pub fn margin_closable(&self, uid: i64, symbol: &str, side: Side) -> Decimal {
        match self.mpos.get(&(uid, symbol.to_string())) {
            Some(p) if sgn(p.size) == -sign(side) => p.size.abs(),
            _ => Decimal::ZERO,
        }
    }

    /// Mark price at which this position would take the account to the margin stop
    /// (other positions held at today's marks).
    pub fn margin_liq_price(&self, p: &MPos) -> Option<Decimal> {
        let mark = self.mark(&p.symbol)?;
        if p.size.is_zero() {
            return None;
        }
        let (upnl, used, _) = self.margin_state(p.user_id);
        let usd = self.balances.get(&(p.user_id, self.cfg.quote.clone())).map(|b| b.total).unwrap_or_default();
        let other = usd + upnl - p.upnl(mark);
        // other + size * (L - entry) = stop * used
        let l = p.entry + (self.margin_rule(&p.symbol).stop_level * used - other) / p.size;
        (l > Decimal::ZERO).then_some(l)
    }

    // ---------------------------------------------------------------- orders

    /// Validate a margin order and return the USD to reserve: initial margin, trading fee and
    /// opening fee for the part that opens or grows a position. A closing part needs nothing.
    pub(crate) fn margin_check_order(&self, uid: i64, req: &OrderReq, ref_price: Decimal) -> Result<Decimal, String> {
        let lev = req.leverage.ok_or("not a margin order")?;
        if lev < Decimal::TWO || !lev.fract().is_zero() {
            return Err("leverage must be a whole number, 2 or more (leave it out for a cash order)".into());
        }
        let closable = self.margin_closable(uid, &req.symbol, req.side);
        if req.reduce_only {
            if closable.is_zero() {
                return Err("reduce-only: you have no margin position on the other side to reduce".into());
            }
            if req.qty > closable {
                return Err(format!("reduce-only: quantity is larger than the position ({})", closable.normalize()));
            }
        }
        // Kraken's limits apply to what opens a position; closing one is always allowed
        if req.qty > closable {
            let rule = self.margin_rule(&req.symbol);
            let max = if req.side == Side::Buy { rule.max_long } else { rule.max_short };
            if max < Decimal::TWO {
                return Err(format!("Kraken offers no margin {} on {}", if req.side == Side::Buy { "longs" } else { "shorts" }, req.symbol));
            }
            if lev > max {
                return Err(format!("leverage on {} can be at most {}x", req.symbol, max.normalize()));
            }
        }
        let open_qty = (req.qty - closable).max(Decimal::ZERO);
        let rate = self.margin_rate(&req.symbol, req.side == Side::Buy);
        let reserve = ceil8(open_qty * ref_price * (Decimal::ONE / lev + self.cfg.max_fee() + rate));
        let avail = self.free(uid, &self.cfg.quote);
        if reserve > avail {
            return Err(format!(
                "insufficient margin: need {}, available {} {} (margin is backed by spot {})",
                reserve.round_dp(2),
                avail.max(Decimal::ZERO).round_dp(2),
                self.cfg.quote,
                self.cfg.quote
            ));
        }
        Ok(reserve)
    }

    /// What an open margin order still needs reserved for its unfilled part.
    pub(crate) fn margin_order_need(&self, o: &Order) -> Decimal {
        let rem = o.remaining();
        let Some(lev) = o.leverage else { return Decimal::ZERO };
        if rem <= Decimal::ZERO || o.reduce_only {
            return Decimal::ZERO;
        }
        let px = o.limit_price.or(o.trigger_price).or_else(|| self.mark(&o.symbol)).unwrap_or_default();
        let open_qty = (rem - self.margin_closable(o.user_id, &o.symbol, o.side)).max(Decimal::ZERO);
        let rate = self.margin_rate(&o.symbol, o.side == Side::Buy);
        ceil8(open_qty * px * (Decimal::ONE / lev + self.cfg.max_fee() + rate))
    }

    /// Largest quantity a margin order can fill at `price` with the USD it has.
    pub(crate) fn margin_max_fill(&self, oid: i64, price: Decimal) -> Decimal {
        let o = &self.orders[&oid];
        let closable = self.margin_closable(o.user_id, &o.symbol, o.side);
        if o.reduce_only {
            return closable;
        }
        let lev = o.leverage.unwrap_or(Decimal::ONE);
        let fee = self.cfg.taker_fee.max(self.cfg.maker_fee);
        let rate = self.margin_rate(&o.symbol, o.side == Side::Buy);
        let funds = self.free(o.user_id, &self.cfg.quote) + o.locked - closable * price * fee - d("0.00000002");
        let qinc = self.instruments.get(&o.symbol).map(|i| i.qty_increment).unwrap_or_default();
        closable + floor_to(funds.max(Decimal::ZERO) / (price * (Decimal::ONE / lev + fee + rate)), qinc)
    }

    /// A resting margin order filled as maker opens only as much as its margin allows; with
    /// none at all it is cancelled rather than left to fill unbacked later.
    pub(crate) fn margin_maker_cap(&mut self, oid: i64, price: Decimal, qty: Decimal, now: i64) -> Decimal {
        let cap = self.margin_max_fill(oid, price);
        if cap.is_zero() {
            self.finish(oid, Status::Canceled, Some("not enough margin to open a position with this order"), now);
        }
        qty.min(cap)
    }

    /// Settle a margin fill: reduce the opposite position (realising P&L and releasing its margin),
    /// open or grow with the rest (holding its margin and charging the opening fee), then the trading fee.
    pub(crate) fn margin_apply_fill(&mut self, oid: i64, price: Decimal, qty: Decimal, taker: bool, now: i64) {
        let cfg = self.cfg.clone();
        let quote = cfg.quote.clone();
        let liquidation = self.liquidating;
        let (uid, symbol, side, reduce_only, lev) = {
            let o = &self.orders[&oid];
            (o.user_id, o.symbol.clone(), o.side, o.reduce_only, o.leverage.unwrap_or(Decimal::TWO))
        };
        let closable = self.margin_closable(uid, &symbol, side);
        let qty = if reduce_only { qty.min(closable) } else { qty };
        if qty <= Decimal::ZERO {
            self.finish(oid, Status::Canceled, Some("reduce-only: the position is already closed"), now);
            return;
        }
        let dir = sign(side);
        let rate = self.margin_rate(&symbol, dir > Decimal::ZERO);
        let cost = round8(price * qty);
        let fee = round8(cost * if taker { cfg.taker_fee } else { cfg.maker_fee });
        let txn = self.new_txn();
        let user = format!("user:{uid}");
        let mut usd = Decimal::ZERO;
        let mut lock_used = Decimal::ZERO;
        let mut pos = self.mpos.remove(&(uid, symbol.clone()));
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
            self.pnl_entry(txn, now, &user, pnl, p.id);
            usd += pnl;
        }
        let mut closed = None;
        if pos.as_ref().map(|p| p.size.is_zero()).unwrap_or(false) {
            closed = pos.take();
        }

        // 2. open or grow with what is left
        if rest > Decimal::ZERO {
            if pos.is_none() {
                pos = Some(self.new_mpos(uid, &symbol, lev, price, now));
            }
            let p = pos.as_mut().unwrap();
            let held = p.size.abs();
            p.entry = ((held * p.entry + rest * price) / (held + rest)).round_dp(12);
            p.size += dir * rest;
            p.updated_ms = now;
            let im = round8(rest * price / lev);
            p.margin += im;
            let open_fee = round8(rest * price * rate);
            self.transfer(txn, now, &user, "fees", &quote, open_fee, "margin_open_fee", p.id);
            p.fees += open_fee;
            usd -= open_fee;
            lock_used += im + open_fee;
        }

        // 3. trading fee
        self.transfer(txn, now, &user, "fees", &quote, fee, "fee", oid);
        usd -= fee;
        lock_used += fee;
        if let Some(p) = pos.as_mut().or(closed.as_mut()) {
            p.fees += fee;
        }

        if let Some(c) = closed {
            self.close_mpos_row(&c, if liquidation { "liquidated" } else { "closed" }, now);
        }
        if let Some(p) = pos {
            self.save_mpos(&p);
            self.mpos.insert((uid, symbol.clone()), p);
        }
        let o = self.orders.get_mut(&oid).unwrap();
        let used = lock_used.min(o.locked);
        o.locked -= used;
        o.filled += qty;
        o.cum_cost += cost;
        o.fee += fee;
        o.updated_ms = now;
        let done = o.remaining() <= Decimal::ZERO;
        if done {
            o.status = Status::Filled;
        }
        let bal = self.balances.entry((uid, quote.clone())).or_default();
        bal.total += usd;
        bal.locked -= used;
        let snap = self.orders[&oid].clone();
        let need = if done { Decimal::ZERO } else { self.margin_order_need(&snap) };
        if snap.locked > need {
            let extra = snap.locked - need;
            self.orders.get_mut(&oid).unwrap().locked = need;
            self.balances.entry((uid, quote.clone())).or_default().locked -= extra;
        }
        let snap = self.orders[&oid].clone();
        self.save_bal(uid, &quote);
        let fid = self.record_fill(&snap, price, qty, cost, fee, taker, now);
        self.save_order(&snap);
        if done {
            self.orders.remove(&oid);
        }
        self.publish_fill(&snap, fid, price, qty, cost, fee, taker, now);
        self.margin_cover_deficit(uid, now);
        self.emit_margin(uid);
        if !done && reduce_only && self.margin_closable(uid, &symbol, side).is_zero() {
            self.finish(oid, Status::Canceled, Some("reduce-only: the position is closed"), now);
        }
    }

    /// Keep spot USD valid after a loss: orders reserving more USD than is left are cancelled,
    /// and a balance pushed below zero is topped back up by the insurance fund.
    fn margin_cover_deficit(&mut self, uid: i64, now: i64) {
        let quote = self.cfg.quote.clone();
        let b = self.balances.get(&(uid, quote.clone())).cloned().unwrap_or_default();
        if b.total < b.locked {
            let ids: Vec<i64> = self.orders.values().filter(|o| o.user_id == uid && !o.locked.is_zero() && self.locks_quote(o)).map(|o| o.id).collect();
            for id in ids {
                self.finish(id, Status::Canceled, Some("cancelled: not enough USD left after a margin loss"), now);
            }
        }
        let total = self.balances.get(&(uid, quote.clone())).map(|b| b.total).unwrap_or_default();
        if total < Decimal::ZERO {
            let txn = self.new_txn();
            self.transfer(txn, now, "insurance", &format!("user:{uid}"), &quote, -total, "insurance", 0);
            self.balances.entry((uid, quote.clone())).or_default().total = Decimal::ZERO;
            self.save_bal(uid, &quote);
        }
    }

    /// Does this live order reserve quote currency (a spot buy, or any margin order)?
    pub(crate) fn locks_quote(&self, o: &Order) -> bool {
        !self.perps.contains_key(&o.symbol) && (o.leverage.is_some() || o.side == Side::Buy)
    }

    // ---------------------------------------------------------------- rollover and liquidation

    /// Once a second: charge rollover that has come due, then liquidate accounts at the margin stop.
    pub fn margin_tick(&mut self, now: i64) {
        if self.mpos.is_empty() || !self.feed_ok(now) {
            return;
        }
        self.run_rollover(now);
        let mut users = BTreeSet::new();
        for p in self.mpos.values() {
            if self.tradable(&p.symbol, now) {
                users.insert(p.user_id);
            }
        }
        for uid in users {
            let (upnl, used, _) = self.margin_state(uid);
            let usd = self.balances.get(&(uid, self.cfg.quote.clone())).map(|b| b.total).unwrap_or_default();
            let stop = self
                .mpos
                .values()
                .filter(|p| p.user_id == uid)
                .map(|p| self.margin_rule(&p.symbol).stop_level)
                .max()
                .unwrap_or(d("0.4"));
            if used > Decimal::ZERO && usd + upnl <= used * stop {
                self.margin_liquidate(uid, now);
            }
        }
        self.commit();
    }

    fn run_rollover(&mut self, now: i64) {
        let quote = self.cfg.quote.clone();
        let period = self.cfg.margin_rollover_secs * 1000;
        let due: Vec<(i64, String)> = self.mpos.iter().filter(|(_, p)| now >= p.next_rollover_ms).map(|(k, _)| k.clone()).collect();
        for k in due {
            let Some(mark) = self.mark(&k.1) else { continue };
            let mut p = self.mpos.remove(&k).unwrap();
            let fee = round8(p.size.abs() * mark * self.margin_rate(&p.symbol, p.size > Decimal::ZERO));
            if fee > Decimal::ZERO {
                let txn = self.new_txn();
                self.transfer(txn, now, &format!("user:{}", p.user_id), "fees", &quote, fee, "rollover", p.id);
                self.balances.entry((p.user_id, quote.clone())).or_default().total -= fee;
                self.save_bal(p.user_id, &quote);
                p.fees += fee;
                p.rollover += fee;
            }
            p.next_rollover_ms += period * ((now - p.next_rollover_ms) / period + 1);
            p.updated_ms = now;
            self.save_mpos(&p);
            let uid = p.user_id;
            self.mpos.insert(k, p);
            self.margin_cover_deficit(uid, now);
            self.emit_margin(uid);
        }
    }

    /// Close every margin position of the user at market, then charge the liquidation fee.
    fn margin_liquidate(&mut self, uid: i64, now: i64) {
        let quote = self.cfg.quote.clone();
        // orders that reserve USD go first: the close may need all of it
        let ids: Vec<i64> = self.orders.values().filter(|o| o.user_id == uid && self.locks_quote(o)).map(|o| o.id).collect();
        for id in ids {
            self.finish(id, Status::Canceled, Some("cancelled by margin liquidation"), now);
        }
        let syms: Vec<String> = self.mpos.keys().filter(|k| k.0 == uid).map(|k| k.1.clone()).collect();
        for sym in syms {
            if !self.tradable(&sym, now) {
                continue;
            }
            let Some(p) = self.mpos.get(&(uid, sym.clone())).cloned() else { continue };
            let mark = self.mark(&sym).unwrap_or(p.entry);
            let closed = self.margin_force_close(uid, &sym, p.size, now);
            if closed.is_zero() {
                continue; // nothing to trade against right now; the next check retries
            }
            let usd = self.balances.get(&(uid, quote.clone())).map(|b| b.total).unwrap_or_default();
            let liq_fee = round8(closed * mark * self.cfg.liq_fee).min(usd.max(Decimal::ZERO));
            if liq_fee > Decimal::ZERO {
                let txn = self.new_txn();
                self.transfer(txn, now, &format!("user:{uid}"), "insurance", &quote, liq_fee, "liquidation_fee", p.id);
                self.balances.entry((uid, quote.clone())).or_default().total -= liq_fee;
                self.save_bal(uid, &quote);
                self.add_mpos_fee(p.id, &sym, uid, liq_fee);
            }
            tracing::info!("margin liquidation: user {uid} {sym} {} at mark {mark}, fee {liq_fee}", closed);
            let payload = json!({
                "type": "liquidation", "symbol": sym, "side": if p.size > Decimal::ZERO { "long" } else { "short" },
                "size": s8(closed), "mark": s8(mark), "fee": s8(liq_fee), "margin_mode": "margin",
            })
            .to_string();
            self.publish(Event::User { user_id: uid, payload: payload.into() });
        }
        self.margin_cover_deficit(uid, now);
        self.emit_margin(uid);
        self.commit();
    }

    /// Market-close a margin position immediately (no latency). Returns the quantity closed.
    fn margin_force_close(&mut self, uid: i64, symbol: &str, size: Decimal, now: i64) -> Decimal {
        let qty = size.abs();
        let lev = self.mpos.get(&(uid, symbol.to_string())).map(|p| p.leverage).unwrap_or(Decimal::TWO);
        let o = self.insert_order(Order {
            id: 0,
            user_id: uid,
            season_id: self.season_id,
            symbol: symbol.to_string(),
            side: if size > Decimal::ZERO { Side::Sell } else { Side::Buy },
            otype: OType::Market,
            tif: Tif::Gtc,
            post_only: false,
            reduce_only: true,
            leverage: Some(lev),
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
        let left = self.mpos.get(&(uid, symbol.to_string())).map(|p| p.size.abs()).unwrap_or_default();
        qty - left
    }

    // ---------------------------------------------------------------- views

    pub fn margin_account_json(&self, uid: i64) -> Value {
        let (upnl, used, _) = self.margin_state(uid);
        let mut keys: Vec<&(i64, String)> = self.mpos.keys().filter(|k| k.0 == uid).collect();
        keys.sort();
        let positions: Vec<Value> = keys
            .into_iter()
            .map(|k| {
                let p = &self.mpos[k];
                let mark = self.mark(&p.symbol).unwrap_or(p.entry);
                let u = p.upnl(mark);
                json!({
                    "id": p.id, "symbol": p.symbol, "side": if p.size > Decimal::ZERO { "long" } else { "short" },
                    "size": s8(p.size.abs()), "entry": s8(p.entry), "mark": s8(mark), "value": s8(p.size.abs() * mark),
                    "leverage": p.leverage.normalize().to_string(), "margin": s8(p.margin), "unrealized_pnl": s8(u),
                    "roe_pct": if p.margin > Decimal::ZERO { s8(u / p.margin * Decimal::ONE_HUNDRED) } else { "0".into() },
                    "liq_price": self.margin_liq_price(p).map(s8), "realized": s8(p.realized), "fees": s8(p.fees),
                    "rollover": s8(p.rollover), "next_rollover_ms": p.next_rollover_ms, "opened_ms": p.opened_ms,
                })
            })
            .collect();
        let rules: serde_json::Map<String, Value> = self
            .cfg
            .symbols
            .iter()
            .map(|s| {
                let r = self.margin_rule(s);
                (s.clone(), json!({
                    "max_long": r.max_long.normalize().to_string(), "max_short": r.max_short.normalize().to_string(),
                    "long_rate": r.long_rate.normalize().to_string(), "short_rate": r.short_rate.normalize().to_string(),
                    "call_level": r.call_level.normalize().to_string(), "stop_level": r.stop_level.normalize().to_string(),
                }))
            })
            .collect();
        json!({
            "equity": s8(self.margin_equity(uid)), "unrealized_pnl": s8(upnl), "margin_used": s8(used),
            "margin_level": self.margin_level(uid).map(|l| s8(l * Decimal::ONE_HUNDRED)),
            "available": s8(self.free(uid, &self.cfg.quote).max(Decimal::ZERO)),
            "rollover_hours": self.cfg.margin_rollover_secs as f64 / 3600.0,
            "positions": positions, "rules": rules,
        })
    }

    pub fn margin_history(&self, uid: i64, limit: i64) -> Vec<Value> {
        let mut st = self
            .db
            .prepare(&format!(
                "SELECT {MPOS_COLS}, side, closed_ms, close_reason FROM margin_positions
                 WHERE user_id=? AND season_id=? AND closed_ms IS NOT NULL ORDER BY closed_ms DESC LIMIT ?"
            ))
            .unwrap();
        let rows = st
            .query_map(params![uid, self.season_id, limit], |r| {
                let p = row_to_mpos(r)?;
                let side: String = r.get(15)?;
                let closed_ms: i64 = r.get(16)?;
                let reason: Option<String> = r.get(17)?;
                Ok(json!({
                    "id": p.id, "symbol": p.symbol, "side": side, "size": s8(p.closed_qty), "entry": s8(p.entry),
                    "exit": if p.closed_qty > Decimal::ZERO { Some(s8(p.closed_value / p.closed_qty)) } else { None },
                    "leverage": p.leverage.normalize().to_string(), "realized": s8(p.realized), "fees": s8(p.fees),
                    "rollover": s8(p.rollover), "net_pnl": s8(p.realized - p.fees), "opened_ms": p.opened_ms,
                    "closed_ms": closed_ms, "close_reason": reason,
                }))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    pub(crate) fn emit_margin(&self, uid: i64) {
        let payload = json!({"type": "margin", "account": self.margin_account_json(uid)}).to_string();
        self.publish(Event::User { user_id: uid, payload: payload.into() });
    }

    // ---------------------------------------------------------------- persistence

    fn new_mpos(&mut self, uid: i64, symbol: &str, leverage: Decimal, price: Decimal, now: i64) -> MPos {
        let next = now + self.cfg.margin_rollover_secs * 1000;
        self.ensure_tx();
        self.db
            .execute(
                "INSERT INTO margin_positions(user_id, season_id, symbol, side, size, entry, leverage, margin, realized,
                   fees, rollover, closed_qty, closed_value, opened_ms, updated_ms, next_rollover_ms)
                 VALUES(?,?,?,'',?,?,?,'0','0','0','0','0','0',?,?,?)",
                params![uid, self.season_id, symbol, "0", price.to_string(), leverage.to_string(), now, now, next],
            )
            .expect("insert margin position");
        MPos {
            id: self.db.last_insert_rowid(),
            user_id: uid,
            symbol: symbol.to_string(),
            size: Decimal::ZERO,
            entry: price,
            leverage,
            margin: Decimal::ZERO,
            realized: Decimal::ZERO,
            fees: Decimal::ZERO,
            rollover: Decimal::ZERO,
            closed_qty: Decimal::ZERO,
            closed_value: Decimal::ZERO,
            opened_ms: now,
            updated_ms: now,
            next_rollover_ms: next,
        }
    }

    fn save_mpos(&mut self, p: &MPos) {
        self.ensure_tx();
        self.db
            .execute(
                "UPDATE margin_positions SET size=?, entry=?, margin=?, realized=?, fees=?, rollover=?, closed_qty=?,
                   closed_value=?, updated_ms=?, next_rollover_ms=?,
                   side=CASE WHEN side='' THEN ? ELSE side END WHERE id=?",
                params![
                    p.size.to_string(),
                    p.entry.to_string(),
                    p.margin.to_string(),
                    p.realized.to_string(),
                    p.fees.to_string(),
                    p.rollover.to_string(),
                    p.closed_qty.to_string(),
                    p.closed_value.to_string(),
                    p.updated_ms,
                    p.next_rollover_ms,
                    if p.size > Decimal::ZERO { "long" } else { "short" },
                    p.id
                ],
            )
            .expect("save margin position");
    }

    fn close_mpos_row(&mut self, p: &MPos, reason: &str, now: i64) {
        self.save_mpos(p);
        self.db
            .execute("UPDATE margin_positions SET closed_ms=?, close_reason=? WHERE id=?", params![now, reason, p.id])
            .expect("close margin position");
    }

    /// Book a fee on a position whether it is still open or already closed.
    fn add_mpos_fee(&mut self, id: i64, symbol: &str, uid: i64, fee: Decimal) {
        if let Some(p) = self.mpos.get_mut(&(uid, symbol.to_string())).filter(|p| p.id == id) {
            p.fees += fee;
            let p = p.clone();
            self.save_mpos(&p);
            return;
        }
        let cur: String = self.db.query_row("SELECT fees FROM margin_positions WHERE id=?", [id], |r| r.get(0)).unwrap();
        let total = parse_dec(&cur).unwrap_or_default() + fee;
        self.ensure_tx();
        self.db.execute("UPDATE margin_positions SET fees=? WHERE id=?", params![total.to_string(), id]).unwrap();
    }

    pub(crate) fn margin_load(&mut self) {
        let open: Vec<MPos> = {
            let mut st = self.db.prepare(&format!("SELECT {MPOS_COLS} FROM margin_positions WHERE closed_ms IS NULL")).unwrap();
            let rows = st.query_map([], row_to_mpos).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for p in open {
            self.mpos.insert((p.user_id, p.symbol.clone()), p);
        }
    }

    /// New season: settle every margin position at the mark price into spot USD.
    pub(crate) fn margin_season_reset(&mut self, now: i64) {
        let quote = self.cfg.quote.clone();
        let keys: Vec<(i64, String)> = self.mpos.keys().cloned().collect();
        for k in keys {
            let mut p = self.mpos.remove(&k).unwrap();
            let mark = self.mark(&p.symbol).unwrap_or(p.entry);
            let pnl = round8(p.upnl(mark));
            let txn = self.new_txn();
            self.pnl_entry(txn, now, &format!("user:{}", p.user_id), pnl, p.id);
            self.balances.entry((p.user_id, quote.clone())).or_default().total += pnl;
            p.realized += pnl;
            p.closed_qty += p.size.abs();
            p.closed_value += round8(p.size.abs() * mark);
            p.size = Decimal::ZERO;
            p.margin = Decimal::ZERO;
            p.updated_ms = now;
            self.close_mpos_row(&p, "season ended", now);
            self.margin_cover_deficit(p.user_id, now);
        }
    }

    pub(crate) fn margin_reconcile(&self, problems: &mut Vec<String>) {
        for p in self.mpos.values() {
            if p.size.is_zero() || p.margin < Decimal::ZERO {
                problems.push(format!("margin position {}: size {} margin {}", p.id, p.size, p.margin));
            }
        }
    }
}
