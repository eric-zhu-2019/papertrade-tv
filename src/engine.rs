//! Matching simulator + ledger.
//!
//! Student orders never reach a real exchange. They are executed against a live
//! mirror of the real exchange's order book and trade stream:
//!
//! * Taker orders walk the real L2 book level by level (real slippage), and the
//!   liquidity they eat is remembered for a while so it cannot be hit twice.
//! * Resting limit orders join the back of the real queue at their price. They
//!   fill only when real trades have consumed the quantity that was ahead of
//!   them, or when the real market trades/crosses through their price.
//! * Every balance change is written to a double-entry ledger.

use crate::book::{Book, Level, Side};
use crate::candles::CandleStore;
use crate::config::Config;
use crate::futures::{FutAcct, MarginMode, Perp, Position};
use crate::util::*;
use rusqlite::{params, Connection};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub type Shared = Arc<Mutex<Engine>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OType {
    Market,
    Limit,
    StopLoss,
    TakeProfit,
    StopLossLimit,
    TakeProfitLimit,
}

impl OType {
    pub fn as_str(self) -> &'static str {
        match self {
            OType::Market => "market",
            OType::Limit => "limit",
            OType::StopLoss => "stop-loss",
            OType::TakeProfit => "take-profit",
            OType::StopLossLimit => "stop-loss-limit",
            OType::TakeProfitLimit => "take-profit-limit",
        }
    }
    pub fn parse(s: &str) -> Option<OType> {
        Some(match s {
            "market" => OType::Market,
            "limit" => OType::Limit,
            "stop-loss" => OType::StopLoss,
            "take-profit" => OType::TakeProfit,
            "stop-loss-limit" => OType::StopLossLimit,
            "take-profit-limit" => OType::TakeProfitLimit,
            _ => return None,
        })
    }
    pub fn is_stop(self) -> bool {
        !matches!(self, OType::Market | OType::Limit)
    }
    pub fn has_limit(self) -> bool {
        matches!(self, OType::Limit | OType::StopLossLimit | OType::TakeProfitLimit)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tif {
    Gtc,
    Ioc,
    Fok,
}

impl Tif {
    pub fn as_str(self) -> &'static str {
        match self {
            Tif::Gtc => "gtc",
            Tif::Ioc => "ioc",
            Tif::Fok => "fok",
        }
    }
    pub fn parse(s: &str) -> Option<Tif> {
        Some(match s {
            "gtc" => Tif::Gtc,
            "ioc" => Tif::Ioc,
            "fok" => Tif::Fok,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Untriggered,
    Open,
    Filled,
    Canceled,
    Rejected,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Untriggered => "untriggered",
            Status::Open => "open",
            Status::Filled => "filled",
            Status::Canceled => "canceled",
            Status::Rejected => "rejected",
        }
    }
    pub fn parse(s: &str) -> Status {
        match s {
            "pending" => Status::Pending,
            "untriggered" => Status::Untriggered,
            "open" => Status::Open,
            "filled" => Status::Filled,
            "rejected" => Status::Rejected,
            _ => Status::Canceled,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Order {
    pub id: i64,
    pub user_id: i64,
    pub season_id: i64,
    pub symbol: String,
    pub side: Side,
    #[serde(rename = "type")]
    pub otype: OType,
    pub tif: Tif,
    pub post_only: bool,
    /// Perps only: may shrink or close the position but never open or grow one.
    pub reduce_only: bool,
    pub qty: Decimal,
    pub limit_price: Option<Decimal>,
    pub trigger_price: Option<Decimal>,
    pub filled: Decimal,
    pub cum_cost: Decimal,
    pub fee: Decimal,
    pub status: Status,
    /// Funds still reserved for this order (spot: quote for buys, base for sells;
    /// perps: USD in the futures wallet for initial margin and fees).
    pub locked: Decimal,
    /// Real-exchange quantity still ahead of this order at its price level.
    pub queue_ahead: Option<Decimal>,
    pub note: Option<String>,
    pub reason: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl Order {
    pub fn remaining(&self) -> Decimal {
        self.qty - self.filled
    }
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap();
        let avg = if self.filled.is_zero() {
            Value::Null
        } else {
            json!((self.cum_cost / self.filled).round_dp(8).normalize().to_string())
        };
        v["avg_price"] = avg;
        v
    }
}

pub struct OrderReq {
    pub symbol: String,
    pub side: Side,
    pub otype: OType,
    pub qty: Decimal,
    pub limit_price: Option<Decimal>,
    pub trigger_price: Option<Decimal>,
    pub tif: Tif,
    pub post_only: bool,
    pub reduce_only: bool,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Instrument {
    pub symbol: String,
    /// "spot" or "perp"
    pub kind: &'static str,
    pub base: String,
    pub quote: String,
    pub price_increment: Decimal,
    pub qty_increment: Decimal,
    pub qty_min: Decimal,
    pub cost_min: Decimal,
    pub price_precision: u32,
    pub qty_precision: u32,
    pub status: String,
}

#[derive(Clone, Debug, Default)]
pub struct Bal {
    pub total: Decimal,
    pub locked: Decimal,
}

#[derive(Clone, Debug, Default)]
pub struct Ticker {
    pub last: Option<Decimal>,
    pub high: Option<Decimal>,
    pub low: Option<Decimal>,
    pub volume: Option<Decimal>,
    pub change_pct: Option<Decimal>,
}

#[derive(Clone)]
pub enum Event {
    /// Book / trade data for one symbol.
    Public { symbol: String, payload: Arc<str> },
    /// Tickers and exchange status, sent to everyone.
    Global { payload: Arc<str> },
    /// Private order / fill updates.
    User { user_id: i64, payload: Arc<str> },
}

pub struct Engine {
    pub cfg: Arc<Config>,
    pub db: Connection,
    pub tx: broadcast::Sender<Event>,
    pub instruments: HashMap<String, Instrument>,
    pub books: HashMap<String, Book>,
    pub tickers: HashMap<String, Ticker>,
    pub tape: HashMap<String, VecDeque<Value>>,
    pub candles: CandleStore,
    pub balances: HashMap<(i64, String), Bal>,
    /// Live orders only (pending, untriggered, open).
    pub orders: BTreeMap<i64, Order>,
    pub season_id: i64,
    pub season_name: String,
    pub season_start_ms: i64,
    pub feed_connected: bool,
    pub last_feed_ms: i64,
    /// user -> (peak equity, max drawdown fraction) for the current season
    pub stats: HashMap<i64, (Decimal, f64)>,
    pub(crate) login_fails: HashMap<String, (u32, i64)>,
    next_txn: i64,
    in_tx: bool,
    book_pub_ms: HashMap<String, i64>,
    rate: HashMap<i64, (f64, i64)>,
    // perpetual futures (see futures.rs)
    pub perps: HashMap<String, Perp>,
    pub fut: HashMap<i64, FutAcct>,
    /// Open positions by (user, symbol).
    pub positions: HashMap<(i64, String), Position>,
    pub(crate) fut_prefs: HashMap<(i64, String), (MarginMode, Decimal)>,
    pub fut_feed_connected: bool,
    pub fut_last_feed_ms: i64,
    pub(crate) funding_done: HashMap<String, i64>,
    /// Funding periods that ended, captured from tickers: (symbol, time, rate).
    pub(crate) funding_due: Vec<(String, i64, Decimal)>,
    /// Set while the engine itself is closing a position for liquidation.
    pub(crate) liquidating: bool,
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS users(
  id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE COLLATE NOCASE, pw_hash TEXT NOT NULL,
  is_admin INTEGER NOT NULL DEFAULT 0, disabled INTEGER NOT NULL DEFAULT 0, created_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS tokens(
  hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL, kind TEXT NOT NULL, label TEXT, prefix TEXT,
  created_ms INTEGER NOT NULL, expires_ms INTEGER);
CREATE TABLE IF NOT EXISTS seasons(
  id INTEGER PRIMARY KEY, name TEXT NOT NULL, start_ms INTEGER NOT NULL, end_ms INTEGER, start_balance TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS season_results(
  season_id INTEGER, rank INTEGER, user_id INTEGER, username TEXT, equity TEXT,
  return_pct REAL, max_dd_pct REAL, sharpe REAL, trades INTEGER);
CREATE TABLE IF NOT EXISTS balances(
  user_id INTEGER, asset TEXT, total TEXT NOT NULL, locked TEXT NOT NULL, PRIMARY KEY(user_id, asset));
CREATE TABLE IF NOT EXISTS orders(
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, season_id INTEGER NOT NULL, symbol TEXT NOT NULL,
  side TEXT NOT NULL, otype TEXT NOT NULL, tif TEXT NOT NULL, post_only INTEGER NOT NULL,
  qty TEXT NOT NULL, limit_price TEXT, trigger_price TEXT, filled TEXT NOT NULL, cum_cost TEXT NOT NULL,
  fee TEXT NOT NULL, status TEXT NOT NULL, locked TEXT NOT NULL, note TEXT, reason TEXT,
  created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS orders_user ON orders(user_id, season_id, id);
CREATE INDEX IF NOT EXISTS orders_status ON orders(status);
CREATE TABLE IF NOT EXISTS fills(
  id INTEGER PRIMARY KEY, order_id INTEGER NOT NULL, user_id INTEGER NOT NULL, season_id INTEGER NOT NULL,
  symbol TEXT NOT NULL, side TEXT NOT NULL, price TEXT NOT NULL, qty TEXT NOT NULL, cost TEXT NOT NULL,
  fee TEXT NOT NULL, liquidity TEXT NOT NULL, ts_ms INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS fills_user ON fills(user_id, season_id, id);
CREATE TABLE IF NOT EXISTS ledger(
  id INTEGER PRIMARY KEY, txn INTEGER NOT NULL, ts_ms INTEGER NOT NULL, account TEXT NOT NULL,
  asset TEXT NOT NULL, amount TEXT NOT NULL, kind TEXT NOT NULL, ref INTEGER);
CREATE INDEX IF NOT EXISTS ledger_account ON ledger(account, asset);
CREATE TABLE IF NOT EXISTS equity_snapshots(user_id INTEGER, season_id INTEGER, ts_ms INTEGER, equity TEXT);
CREATE INDEX IF NOT EXISTS equity_idx ON equity_snapshots(user_id, season_id, ts_ms);
CREATE TABLE IF NOT EXISTS equity_daily(
  user_id INTEGER, season_id INTEGER, day INTEGER, equity TEXT, PRIMARY KEY(user_id, season_id, day));
CREATE TABLE IF NOT EXISTS user_stats(
  user_id INTEGER, season_id INTEGER, peak TEXT, max_dd REAL, PRIMARY KEY(user_id, season_id));
CREATE TABLE IF NOT EXISTS invites(
  code TEXT PRIMARY KEY, note TEXT, created_ms INTEGER NOT NULL, used_by INTEGER, used_ms INTEGER);
CREATE TABLE IF NOT EXISTS fut_accounts(user_id INTEGER PRIMARY KEY, wallet TEXT NOT NULL, locked TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS fut_settings(
  user_id INTEGER, symbol TEXT, mode TEXT NOT NULL, leverage TEXT NOT NULL, PRIMARY KEY(user_id, symbol));
CREATE TABLE IF NOT EXISTS positions(
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, season_id INTEGER NOT NULL, symbol TEXT NOT NULL, side TEXT NOT NULL,
  size TEXT NOT NULL, entry TEXT NOT NULL, mode TEXT NOT NULL, leverage TEXT NOT NULL, margin TEXT NOT NULL,
  realized TEXT NOT NULL, fees TEXT NOT NULL, funding TEXT NOT NULL, closed_qty TEXT NOT NULL, closed_value TEXT NOT NULL,
  opened_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL, closed_ms INTEGER, close_reason TEXT);
CREATE INDEX IF NOT EXISTS positions_user ON positions(user_id, season_id, id);
CREATE TABLE IF NOT EXISTS funding(
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, position_id INTEGER NOT NULL, season_id INTEGER NOT NULL,
  symbol TEXT NOT NULL, ts_ms INTEGER NOT NULL, rate TEXT NOT NULL, mark TEXT NOT NULL, amount TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS funding_user ON funding(user_id, season_id, id);
CREATE TABLE IF NOT EXISTS funding_runs(symbol TEXT PRIMARY KEY, ts_ms INTEGER NOT NULL);
";

fn sdec(s: String) -> Decimal {
    parse_dec(&s).unwrap_or(Decimal::ZERO)
}

fn odec(s: Option<String>) -> Option<Decimal> {
    s.and_then(|x| parse_dec(&x))
}

impl Engine {
    pub fn open(cfg: Arc<Config>, tx: broadcast::Sender<Event>, path: &str) -> Engine {
        if path != ":memory:" {
            if let Some(dir) = std::path::Path::new(path).parent() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
        let db = Connection::open(path).expect("open database");
        db.execute_batch(SCHEMA).expect("create schema");
        // columns added after the first release
        let has_reduce_only: bool = db
            .prepare("SELECT 1 FROM pragma_table_info('orders') WHERE name='reduce_only'")
            .and_then(|mut st| st.exists([]))
            .unwrap_or(false);
        if !has_reduce_only {
            db.execute_batch("ALTER TABLE orders ADD COLUMN reduce_only INTEGER NOT NULL DEFAULT 0").expect("migrate orders");
        }
        let mut e = Engine {
            cfg: cfg.clone(),
            db,
            tx,
            instruments: HashMap::new(),
            // Kraken Futures sends the whole book and no "level left your depth" deletions, so perp books keep every level
            books: cfg
                .markets()
                .map(|s| (s.clone(), Book::new(if cfg.is_perp(s) { usize::MAX } else { cfg.book_depth })))
                .collect(),
            tickers: cfg.markets().map(|s| (s.clone(), Ticker::default())).collect(),
            tape: HashMap::new(),
            candles: CandleStore::default(),
            balances: HashMap::new(),
            orders: BTreeMap::new(),
            season_id: 0,
            season_name: String::new(),
            season_start_ms: 0,
            feed_connected: false,
            last_feed_ms: 0,
            stats: HashMap::new(),
            login_fails: HashMap::new(),
            next_txn: 1,
            in_tx: false,
            book_pub_ms: HashMap::new(),
            rate: HashMap::new(),
            perps: cfg.perps.iter().map(|s| (s.clone(), Perp::new(s))).collect(),
            fut: HashMap::new(),
            positions: HashMap::new(),
            fut_prefs: HashMap::new(),
            fut_feed_connected: false,
            fut_last_feed_ms: 0,
            funding_done: HashMap::new(),
            funding_due: Vec::new(),
            liquidating: false,
        };
        e.load();
        e
    }

    fn load(&mut self) {
        let now = now_ms();
        // season
        let cur: Option<(i64, String, i64)> = self
            .db
            .query_row(
                "SELECT id, name, start_ms FROM seasons WHERE end_ms IS NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        match cur {
            Some((id, name, start)) => {
                self.season_id = id;
                self.season_name = name;
                self.season_start_ms = start;
            }
            None => {
                self.db
                    .execute(
                        "INSERT INTO seasons(name, start_ms, start_balance) VALUES('Season 1', ?, ?)",
                        params![now, self.cfg.start_balance.to_string()],
                    )
                    .unwrap();
                self.season_id = self.db.last_insert_rowid();
                self.season_name = "Season 1".into();
                self.season_start_ms = now;
            }
        }
        self.next_txn =
            self.db.query_row("SELECT COALESCE(MAX(txn),0)+1 FROM ledger", [], |r| r.get(0)).unwrap();
        // balances
        {
            let mut st = self.db.prepare("SELECT user_id, asset, total, locked FROM balances").unwrap();
            let rows = st
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
                })
                .unwrap();
            for row in rows {
                let (u, a, t, l) = row.unwrap();
                self.balances.insert((u, a), Bal { total: sdec(t), locked: sdec(l) });
            }
        }
        self.fut_load();
        // stats
        {
            let mut st =
                self.db.prepare("SELECT user_id, peak, max_dd FROM user_stats WHERE season_id=?").unwrap();
            let rows = st
                .query_map([self.season_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?))
                })
                .unwrap();
            for row in rows {
                let (u, p, m) = row.unwrap();
                self.stats.insert(u, (sdec(p), m));
            }
        }
        // live orders
        let live: Vec<Order> = {
            let mut st = self
                .db
                .prepare(&format!(
                    "SELECT {ORDER_COLS} FROM orders WHERE status IN ('pending','untriggered','open')"
                ))
                .unwrap();
            let rows = st.query_map([], row_to_order).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for o in live {
            self.orders.insert(o.id, o);
        }
        // orders caught mid-latency by a restart never reached the book
        let pending: Vec<i64> =
            self.orders.values().filter(|o| o.status == Status::Pending).map(|o| o.id).collect();
        for id in pending {
            self.finish(id, Status::Canceled, Some("server restarted before the order reached the book"), now);
        }
        self.commit();
    }

    // ---------------------------------------------------------------- db helpers

    pub(crate) fn ensure_tx(&mut self) {
        if !self.in_tx {
            self.db.execute_batch("BEGIN").expect("begin");
            self.in_tx = true;
        }
    }

    pub fn commit(&mut self) {
        if self.in_tx {
            self.db.execute_batch("COMMIT").expect("commit");
            self.in_tx = false;
        }
    }

    pub(crate) fn save_bal(&mut self, uid: i64, asset: &str) {
        self.ensure_tx();
        let b = self.balances.get(&(uid, asset.to_string())).cloned().unwrap_or_default();
        self.db
            .execute(
                "INSERT INTO balances(user_id, asset, total, locked) VALUES(?,?,?,?)
                 ON CONFLICT(user_id, asset) DO UPDATE SET total=excluded.total, locked=excluded.locked",
                params![uid, asset, b.total.to_string(), b.locked.to_string()],
            )
            .expect("save balance");
    }

    pub(crate) fn save_order(&mut self, o: &Order) {
        self.ensure_tx();
        self.db
            .execute(
                "UPDATE orders SET filled=?, cum_cost=?, fee=?, status=?, locked=?, reason=?, updated_ms=? WHERE id=?",
                params![
                    o.filled.to_string(),
                    o.cum_cost.to_string(),
                    o.fee.to_string(),
                    o.status.as_str(),
                    o.locked.to_string(),
                    o.reason,
                    o.updated_ms,
                    o.id
                ],
            )
            .expect("save order");
    }

    pub(crate) fn new_txn(&mut self) -> i64 {
        let t = self.next_txn;
        self.next_txn += 1;
        t
    }

    pub(crate) fn ledger(&mut self, txn: i64, ts: i64, account: &str, asset: &str, amount: Decimal, kind: &str, rf: i64) {
        if amount.is_zero() {
            return;
        }
        self.ensure_tx();
        self.db
            .execute(
                "INSERT INTO ledger(txn, ts_ms, account, asset, amount, kind, ref) VALUES(?,?,?,?,?,?,?)",
                params![txn, ts, account, asset, amount.to_string(), kind, rf],
            )
            .expect("ledger insert");
    }

    /// Balanced two-line journal entry: `amount` of `asset` moves from one account to another.
    pub(crate) fn transfer(&mut self, txn: i64, ts: i64, from: &str, to: &str, asset: &str, amount: Decimal, kind: &str, rf: i64) {
        self.ledger(txn, ts, from, asset, -amount, kind, rf);
        self.ledger(txn, ts, to, asset, amount, kind, rf);
    }

    pub(crate) fn publish(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    pub(crate) fn emit_order(&self, o: &Order) {
        let payload = json!({"type": "order", "order": o.to_json()}).to_string();
        self.publish(Event::User { user_id: o.user_id, payload: payload.into() });
    }

    // ---------------------------------------------------------------- balances

    pub fn free(&self, uid: i64, asset: &str) -> Decimal {
        self.balances.get(&(uid, asset.to_string())).map(|b| b.total - b.locked).unwrap_or(Decimal::ZERO)
    }

    /// Credit a new account (or a new season) with the starting balance.
    pub fn grant(&mut self, uid: i64, now: i64) {
        let quote = self.cfg.quote.clone();
        let amt = self.cfg.start_balance;
        self.balances.entry((uid, quote.clone())).or_default().total += amt;
        self.save_bal(uid, &quote);
        let txn = self.new_txn();
        self.transfer(txn, now, "treasury", &format!("user:{uid}"), &quote, amt, "grant", self.season_id);
    }

    pub fn mark(&self, symbol: &str) -> Option<Decimal> {
        self.books
            .get(symbol)
            .filter(|b| b.ready)
            .and_then(|b| b.mid())
            .or_else(|| self.tickers.get(symbol).and_then(|t| t.last))
    }

    pub fn equity(&self, uid: i64) -> Decimal {
        let mut eq = Decimal::ZERO;
        for ((u, asset), b) in &self.balances {
            if *u != uid || b.total.is_zero() {
                continue;
            }
            if *asset == self.cfg.quote {
                eq += b.total;
            } else if let Some(p) = self.mark(&format!("{asset}/{}", self.cfg.quote)) {
                eq += b.total * p;
            }
        }
        eq + self.fut_equity(uid)
    }

    // ---------------------------------------------------------------- market data in

    pub fn feed_ok(&self, now: i64) -> bool {
        self.feed_connected && now - self.last_feed_ms < self.cfg.stale_ms
    }

    pub fn tradable(&self, symbol: &str, now: i64) -> bool {
        let feed = if self.perps.contains_key(symbol) { self.fut_feed_ok(now) } else { self.feed_ok(now) };
        feed && self.books.get(symbol).map(|b| b.ready).unwrap_or(false)
            && self.instruments.get(symbol).map(|i| i.status == "online").unwrap_or(false)
    }

    pub fn feed_up(&mut self, now: i64) {
        self.feed_connected = true;
        self.last_feed_ms = now;
    }

    pub fn feed_down(&mut self) {
        self.feed_connected = false;
        for (s, b) in self.books.iter_mut() {
            if !self.perps.contains_key(s) {
                b.ready = false;
            }
        }
    }

    pub fn set_instrument(&mut self, inst: Instrument) {
        self.instruments.insert(inst.symbol.clone(), inst);
    }

    pub fn on_ticker(&mut self, symbol: &str, t: Ticker) {
        if let Some(cur) = self.tickers.get_mut(symbol) {
            let last = cur.last;
            *cur = t;
            if cur.last.is_none() {
                cur.last = last;
            }
        }
    }

    pub fn on_book_snapshot(&mut self, symbol: &str, bids: &[Level], asks: &[Level], now: i64) {
        let Some(book) = self.books.get_mut(symbol) else { return };
        book.snapshot(bids, asks);
        // We may have missed trades while disconnected, so resting orders keep
        // at most their old queue position and never more than the level holds.
        let book = &self.books[symbol];
        for o in self.orders.values_mut().filter(|o| o.symbol == symbol && o.status == Status::Open) {
            if let Some(p) = o.limit_price {
                let level = book.qty_at(o.side, p);
                o.queue_ahead = Some(o.queue_ahead.map(|q| q.min(level)).unwrap_or(level));
            }
        }
        self.check_cross(symbol, now);
        self.publish_book(symbol, now, true);
        self.commit();
    }

    pub fn on_book_update(&mut self, symbol: &str, bids: &[Level], asks: &[Level], now: i64) {
        let Some(book) = self.books.get_mut(symbol) else { return };
        if !book.ready {
            return;
        }
        book.update(bids, asks);
        if !self.orders.is_empty() {
            // A level shrinking below our queue position means orders ahead of us were cancelled.
            for o in self.orders.values_mut().filter(|o| o.symbol == symbol && o.status == Status::Open) {
                let Some(p) = o.limit_price else { continue };
                let changed = match o.side {
                    Side::Buy => bids.iter().find(|l| l.0 == p),
                    Side::Sell => asks.iter().find(|l| l.0 == p),
                };
                if let (Some((_, q)), Some(qa)) = (changed, o.queue_ahead) {
                    o.queue_ahead = Some(qa.min(*q));
                }
            }
            self.check_cross(symbol, now);
            self.commit();
        }
        self.publish_book(symbol, now, false);
    }

    fn publish_book(&mut self, symbol: &str, now: i64, force: bool) {
        let last = self.book_pub_ms.get(symbol).copied().unwrap_or(0);
        if !force && now - last < 250 {
            return;
        }
        self.book_pub_ms.insert(symbol.to_string(), now);
        let payload = self.book_json(symbol, 25).to_string();
        self.publish(Event::Public { symbol: symbol.to_string(), payload: payload.into() });
    }

    pub fn book_json(&self, symbol: &str, depth: usize) -> Value {
        let lv = |v: Vec<Level>| -> Vec<Value> {
            v.into_iter().map(|(p, q)| json!([p.to_string(), q.to_string()])).collect()
        };
        match self.books.get(symbol) {
            Some(b) if b.ready => json!({
                "type": "book", "symbol": symbol,
                "bids": lv(b.top(Side::Buy, depth)), "asks": lv(b.top(Side::Sell, depth)),
            }),
            _ => json!({"type": "book", "symbol": symbol, "bids": [], "asks": []}),
        }
    }

    /// A real trade printed on the exchange. `live` is false for the historical
    /// trades sent when subscribing, which must not fill or trigger anything.
    pub fn on_trade(&mut self, symbol: &str, price: Decimal, qty: Decimal, taker: Side, ts_ms: i64, live: bool, now: i64) {
        if !self.books.contains_key(symbol) {
            return;
        }
        if let Some(t) = self.tickers.get_mut(symbol) {
            t.last = Some(price);
        }
        self.candles.on_trade(symbol, price, qty, ts_ms);
        let tick = json!({"price": price.to_string(), "qty": qty.to_string(), "side": taker.as_str(), "ts": ts_ms});
        let tape = self.tape.entry(symbol.to_string()).or_default();
        tape.push_front(tick.clone());
        tape.truncate(60);
        if !live {
            return;
        }
        let payload = json!({"type": "trade", "symbol": symbol, "trade": tick}).to_string();
        self.publish(Event::Public { symbol: symbol.to_string(), payload: payload.into() });
        if self.orders.is_empty() || !self.books[symbol].ready {
            return;
        }
        let qty_inc = self.instruments.get(symbol).map(|i| i.qty_increment).unwrap_or(Decimal::ZERO);

        // 1. resting orders on the passive side of this trade, best price first then oldest
        let passive = taker.opposite();
        let mut ids: Vec<(Decimal, i64)> = self
            .orders
            .values()
            .filter(|o| o.symbol == symbol && o.status == Status::Open && o.side == passive)
            .filter_map(|o| o.limit_price.map(|p| (p, o.id)))
            .collect();
        match passive {
            Side::Buy => ids.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1))),
            Side::Sell => ids.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1))),
        }
        let mut budget = qty; // one real trade can only fill `qty` in total
        for (p, id) in ids {
            let through = match passive {
                Side::Buy => price < p,
                Side::Sell => price > p,
            };
            if !through && price != p {
                continue;
            }
            let o = self.orders.get_mut(&id).unwrap();
            let Some(qa) = o.queue_ahead else { continue };
            let mut avail = budget;
            if through {
                // the market traded through our price: everything ahead of us is gone
                o.queue_ahead = Some(Decimal::ZERO);
            } else {
                let eaten = qa.min(qty);
                o.queue_ahead = Some(qa - eaten);
                avail = avail.min(qty - eaten);
            }
            let mut f = floor_to(avail.min(o.remaining()), qty_inc);
            if f > Decimal::ZERO && self.perps.contains_key(symbol) {
                f = self.fut_maker_cap(id, p, f, now);
            }
            if f > Decimal::ZERO {
                self.apply_fill(id, p, f, false, now);
                budget -= f;
            }
        }

        // 2. stop / take-profit triggers on the last traded price
        let trig: Vec<i64> = self
            .orders
            .values()
            .filter(|o| o.symbol == symbol && o.status == Status::Untriggered)
            .filter(|o| {
                let Some(t) = o.trigger_price else { return false };
                let up = matches!(
                    (o.otype, o.side),
                    (OType::StopLoss | OType::StopLossLimit, Side::Buy)
                        | (OType::TakeProfit | OType::TakeProfitLimit, Side::Sell)
                );
                if up { price >= t } else { price <= t }
            })
            .map(|o| o.id)
            .collect();
        for id in trig {
            if let Some(o) = self.orders.get_mut(&id) {
                o.status = Status::Pending;
            }
            self.execute(id, now);
        }
        self.commit();
    }

    /// Resting orders that the real book has crossed (e.g. after a gap or a
    /// reconnect) are filled at their own price as maker.
    fn check_cross(&mut self, symbol: &str, now: i64) {
        let ids: Vec<i64> = self
            .orders
            .values()
            .filter(|o| o.symbol == symbol && o.status == Status::Open)
            .map(|o| o.id)
            .collect();
        for id in ids {
            let (side, p) = {
                let o = &self.orders[&id];
                (o.side, o.limit_price.unwrap_or(Decimal::ZERO))
            };
            let opp = side.opposite();
            // most orders are nowhere near the other side of the book: skip them cheaply
            let crossed = match (side, self.books[symbol].best(opp)) {
                (Side::Buy, Some((best, _))) => best <= p,
                (Side::Sell, Some((best, _))) => best >= p,
                _ => false,
            };
            if !crossed {
                continue;
            }
            let levels = self.books[symbol].avail_levels(opp, now);
            for (lp, avail) in levels {
                let crosses = match side {
                    Side::Buy => lp <= p,
                    Side::Sell => lp >= p,
                };
                if !crosses {
                    break;
                }
                let Some(o) = self.orders.get(&id) else { break };
                let mut f = o.remaining().min(avail);
                if f > Decimal::ZERO && self.perps.contains_key(symbol) {
                    f = self.fut_maker_cap(id, p, f, now);
                }
                if f <= Decimal::ZERO {
                    break;
                }
                self.apply_fill(id, p, f, false, now);
                let ttl = self.cfg.liquidity_ttl_ms;
                self.books.get_mut(symbol).unwrap().consume(opp, lp, f, now, ttl);
            }
        }
    }

    // ---------------------------------------------------------------- orders in

    fn take_token(&mut self, uid: i64, now: i64) -> bool {
        let rate = self.cfg.orders_per_sec;
        let cap = (rate * 2.0).max(5.0);
        let e = self.rate.entry(uid).or_insert((cap, now));
        e.0 = (e.0 + (now - e.1) as f64 / 1000.0 * rate).min(cap);
        e.1 = now;
        if e.0 < 1.0 {
            return false;
        }
        e.0 -= 1.0;
        true
    }

    /// Validate, reserve funds and accept an order. Market and limit orders are
    /// returned as `pending`; the caller must call `activate` after the
    /// simulated network latency. Stop orders wait as `untriggered`.
    pub fn place_order(&mut self, uid: i64, req: OrderReq, now: i64) -> Result<Order, String> {
        let cfg = self.cfg.clone();
        let inst = self.instruments.get(&req.symbol).cloned().ok_or("unknown symbol")?;
        if !self.tradable(&req.symbol, now) {
            return Err("market data is unavailable right now; trading is paused".into());
        }
        if !self.take_token(uid, now) {
            return Err("rate limit exceeded; slow down".into());
        }
        if self.orders.values().filter(|o| o.user_id == uid).count() >= cfg.max_open_orders {
            return Err(format!("too many open orders (max {})", cfg.max_open_orders));
        }
        let note = req.note.as_ref().map(|n| n.trim().chars().take(500).collect::<String>()).filter(|n| !n.is_empty());
        if cfg.require_note && note.is_none() {
            return Err("a trade note is required: write one sentence on why you are placing this order".into());
        }
        if req.qty <= Decimal::ZERO {
            return Err("quantity must be positive".into());
        }
        if !is_multiple(req.qty, inst.qty_increment) {
            return Err(format!("quantity must be a multiple of {}", inst.qty_increment.normalize()));
        }
        if req.qty < inst.qty_min {
            return Err(format!("minimum order quantity is {} {}", inst.qty_min.normalize(), inst.base));
        }
        let check_price = |name: &str, p: Option<Decimal>| -> Result<Decimal, String> {
            let p = p.ok_or(format!("{name} is required for this order type"))?;
            if p <= Decimal::ZERO {
                return Err(format!("{name} must be positive"));
            }
            if !is_multiple(p, inst.price_increment) {
                return Err(format!("{name} must be a multiple of {}", inst.price_increment.normalize()));
            }
            Ok(p)
        };
        let limit = if req.otype.has_limit() { Some(check_price("limit_price", req.limit_price)?) } else { None };
        let trigger = if req.otype.is_stop() { Some(check_price("trigger_price", req.trigger_price)?) } else { None };
        if req.post_only && req.otype != OType::Limit {
            return Err("post_only is only valid for limit orders".into());
        }
        if req.tif != Tif::Gtc && req.otype != OType::Limit {
            return Err("time_in_force other than gtc is only valid for limit orders".into());
        }
        if req.post_only && req.tif != Tif::Gtc {
            return Err("post_only cannot be combined with ioc/fok".into());
        }
        let book = &self.books[&req.symbol];
        let ref_price = match (limit, trigger) {
            (Some(l), _) => l,
            (None, Some(t)) => t,
            _ => book.best(req.side.opposite()).map(|l| l.0).ok_or("order book is empty")?,
        };
        if req.qty * ref_price < inst.cost_min {
            return Err(format!("order value is below the minimum of {} {}", inst.cost_min.normalize(), inst.quote));
        }

        let perp = self.perps.contains_key(&req.symbol);
        if req.reduce_only && !perp {
            return Err("reduce_only is only for futures".into());
        }

        // reserve funds
        let (lock_asset, lock_amt) = if perp {
            (String::new(), self.fut_check_order(uid, &req, ref_price)?)
        } else {
            let (lock_asset, lock_amt) = match req.side {
                Side::Sell => (inst.base.clone(), req.qty),
                Side::Buy => {
                    let amt = match req.otype {
                        OType::Market => Decimal::ZERO,
                        _ => ceil8(req.qty * ref_price * (Decimal::ONE + cfg.max_fee())),
                    };
                    (inst.quote.clone(), amt)
                }
            };
            let free = self.free(uid, &lock_asset);
            let needed = if req.otype == OType::Market && req.side == Side::Buy {
                // rough pre-check; the real cost is decided when the order walks the book
                req.qty * ref_price * (Decimal::ONE + cfg.taker_fee)
            } else {
                lock_amt
            };
            if needed > free {
                return Err(format!(
                    "insufficient {}: need {}, available {}",
                    lock_asset,
                    needed.round_dp(8).normalize(),
                    free.round_dp(8).normalize()
                ));
            }
            (lock_asset, lock_amt)
        };

        let status = if req.otype.is_stop() { Status::Untriggered } else { Status::Pending };
        let o = self.insert_order(Order {
            id: 0,
            user_id: uid,
            season_id: self.season_id,
            symbol: req.symbol,
            side: req.side,
            otype: req.otype,
            tif: req.tif,
            post_only: req.post_only,
            reduce_only: req.reduce_only,
            qty: req.qty,
            limit_price: limit,
            trigger_price: trigger,
            filled: Decimal::ZERO,
            cum_cost: Decimal::ZERO,
            fee: Decimal::ZERO,
            status,
            locked: lock_amt,
            queue_ahead: None,
            note,
            reason: None,
            created_ms: now,
            updated_ms: now,
        });
        if !lock_amt.is_zero() {
            if perp {
                self.fut_lock(uid, lock_amt);
            } else {
                self.balances.entry((uid, lock_asset.clone())).or_default().locked += lock_amt;
                self.save_bal(uid, &lock_asset);
            }
        }
        self.commit();
        self.emit_order(&o);
        Ok(o)
    }

    /// Store a new live order and give it its id.
    pub(crate) fn insert_order(&mut self, mut o: Order) -> Order {
        self.ensure_tx();
        self.db
            .execute(
                "INSERT INTO orders(user_id, season_id, symbol, side, otype, tif, post_only, qty, limit_price,
                   trigger_price, filled, cum_cost, fee, status, locked, note, reason, created_ms, updated_ms, reduce_only)
                 VALUES(?,?,?,?,?,?,?,?,?,?,'0','0','0',?,?,?,NULL,?,?,?)",
                params![
                    o.user_id,
                    o.season_id,
                    o.symbol,
                    o.side.as_str(),
                    o.otype.as_str(),
                    o.tif.as_str(),
                    o.post_only as i64,
                    o.qty.to_string(),
                    o.limit_price.map(|x| x.to_string()),
                    o.trigger_price.map(|x| x.to_string()),
                    o.status.as_str(),
                    o.locked.to_string(),
                    o.note,
                    o.created_ms,
                    o.updated_ms,
                    o.reduce_only as i64
                ],
            )
            .expect("insert order");
        o.id = self.db.last_insert_rowid();
        self.orders.insert(o.id, o.clone());
        o
    }

    /// The order "arrives at the exchange" after its simulated latency.
    pub fn activate(&mut self, oid: i64, now: i64) {
        if self.orders.get(&oid).map(|o| o.status) != Some(Status::Pending) {
            return;
        }
        self.execute(oid, now);
        self.commit();
    }

    fn execute(&mut self, oid: i64, now: i64) {
        let Some(o) = self.orders.get(&oid).cloned() else { return };
        if !self.tradable(&o.symbol, now) {
            self.finish(oid, Status::Rejected, Some("market data became unavailable before the order reached the book"), now);
            return;
        }
        if o.reduce_only && self.fut_closable(o.user_id, &o.symbol, o.side).is_zero() {
            self.finish(oid, Status::Canceled, Some("reduce-only: there is no position left to reduce"), now);
            return;
        }
        let opp = o.side.opposite();
        if !o.otype.has_limit() {
            // market order (or a triggered stop-loss / take-profit)
            self.take(oid, None, now);
            if let Some(o2) = self.orders.get(&oid) {
                let reason = if o2.filled.is_zero() {
                    "no liquidity or insufficient funds at execution"
                } else {
                    "partially filled; the rest could not be filled (liquidity or funds ran out)"
                };
                self.finish(oid, Status::Canceled, Some(reason), now);
            }
            return;
        }
        let limit = o.limit_price.unwrap();
        let within = |p: Decimal| match o.side {
            Side::Buy => p <= limit,
            Side::Sell => p >= limit,
        };
        let book = &self.books[&o.symbol];
        if o.post_only && book.best(opp).map(|l| within(l.0)).unwrap_or(false) {
            self.finish(oid, Status::Canceled, Some("post-only order would have taken liquidity"), now);
            return;
        }
        if o.tif == Tif::Fok {
            let fillable: Decimal =
                book.avail_levels(opp, now).into_iter().take_while(|l| within(l.0)).map(|l| l.1).sum();
            if fillable < o.qty {
                self.finish(oid, Status::Canceled, Some("fill-or-kill order could not be filled completely"), now);
                return;
            }
        }
        self.take(oid, Some(limit), now);
        let Some(o2) = self.orders.get_mut(&oid) else { return };
        if o2.tif != Tif::Gtc {
            self.finish(oid, Status::Canceled, Some("unfilled remainder of an immediate order was cancelled"), now);
            return;
        }
        // rest on the book at the back of the real queue
        o2.status = Status::Open;
        o2.queue_ahead = Some(self.books[&o2.symbol].qty_at(o2.side, limit));
        o2.updated_ms = now;
        let snap = o2.clone();
        self.save_order(&snap);
        self.emit_order(&snap);
    }

    /// Walk the opposite side of the real book.
    pub(crate) fn take(&mut self, oid: i64, limit: Option<Decimal>, now: i64) {
        let cfg = self.cfg.clone();
        let (uid, symbol, side) = {
            let o = &self.orders[&oid];
            (o.user_id, o.symbol.clone(), o.side)
        };
        let inst = self.instruments[&symbol].clone();
        let perp = self.perps.contains_key(&symbol);
        let opp = side.opposite();
        let levels = self.books[&symbol].avail_levels(opp, now);
        for (p, avail) in levels {
            let Some(o) = self.orders.get(&oid) else { break };
            if let Some(l) = limit {
                let ok = match side {
                    Side::Buy => p <= l,
                    Side::Sell => p >= l,
                };
                if !ok {
                    break;
                }
            }
            let mut f = o.remaining().min(avail);
            if perp {
                f = f.min(self.fut_max_fill(oid, p));
            } else if side == Side::Buy {
                let funds = self.free(uid, &inst.quote) + o.locked;
                let unit = p * (Decimal::ONE + cfg.taker_fee);
                let affordable = floor_to((funds - d("0.00000002")).max(Decimal::ZERO) / unit, inst.qty_increment);
                f = f.min(affordable);
            }
            f = floor_to(f, inst.qty_increment);
            if f <= Decimal::ZERO {
                break;
            }
            self.apply_fill(oid, p, f, true, now);
            self.books.get_mut(&symbol).unwrap().consume(opp, p, f, now, cfg.liquidity_ttl_ms);
        }
    }

    fn apply_fill(&mut self, oid: i64, price: Decimal, qty: Decimal, taker: bool, now: i64) {
        let cfg = self.cfg.clone();
        let (uid, symbol, side) = {
            let o = &self.orders[&oid];
            (o.user_id, o.symbol.clone(), o.side)
        };
        if self.perps.contains_key(&symbol) {
            return self.fut_apply_fill(oid, price, qty, taker, now);
        }
        let inst = self.instruments[&symbol].clone();
        let cost = round8(price * qty);
        let mut fee = round8(cost * if taker { cfg.taker_fee } else { cfg.maker_fee });
        let o = self.orders.get_mut(&oid).unwrap();
        match side {
            Side::Buy => {
                let qb = self.balances.entry((uid, inst.quote.clone())).or_default();
                let due = cost + fee;
                let from_lock = due.min(o.locked);
                let mut rest = due - from_lock;
                let free = qb.total - qb.locked;
                if rest > free {
                    // sub-cent rounding dust: waive it from the fee rather than overdraw
                    let waive = (rest - free).min(fee);
                    fee -= waive;
                    rest -= waive;
                }
                qb.total -= from_lock + rest;
                qb.locked -= from_lock;
                o.locked -= from_lock;
                self.balances.entry((uid, inst.base.clone())).or_default().total += qty;
            }
            Side::Sell => {
                let bb = self.balances.entry((uid, inst.base.clone())).or_default();
                bb.total -= qty;
                bb.locked -= qty;
                o.locked -= qty;
                self.balances.entry((uid, inst.quote.clone())).or_default().total += cost - fee;
            }
        }
        o.filled += qty;
        o.cum_cost += cost;
        o.fee += fee;
        o.updated_ms = now;
        let done = o.remaining() <= Decimal::ZERO;
        // release whatever reservation is no longer needed
        let need = if done {
            Decimal::ZERO
        } else if side == Side::Buy && o.otype.has_limit() {
            ceil8(o.remaining() * o.limit_price.unwrap() * (Decimal::ONE + cfg.max_fee()))
        } else {
            o.locked
        };
        if o.locked > need {
            let extra = o.locked - need;
            o.locked = need;
            let asset = if side == Side::Buy { &inst.quote } else { &inst.base };
            self.balances.entry((uid, asset.clone())).or_default().locked -= extra;
        }
        if done {
            o.status = Status::Filled;
        }
        let snap = o.clone();
        let fid = self.record_fill(&snap, price, qty, cost, fee, taker, now);
        let txn = self.new_txn();
        let user = format!("user:{uid}");
        match side {
            Side::Buy => {
                self.transfer(txn, now, &user, "market", &inst.quote, cost, "trade", fid);
                self.transfer(txn, now, "market", &user, &inst.base, qty, "trade", fid);
            }
            Side::Sell => {
                self.transfer(txn, now, &user, "market", &inst.base, qty, "trade", fid);
                self.transfer(txn, now, "market", &user, &inst.quote, cost, "trade", fid);
            }
        }
        self.transfer(txn, now, &user, "fees", &inst.quote, fee, "fee", fid);
        self.save_bal(uid, &inst.quote);
        self.save_bal(uid, &inst.base);
        self.save_order(&snap);
        if done {
            self.orders.remove(&oid);
        }
        self.publish_fill(&snap, fid, price, qty, cost, fee, taker, now);
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn record_fill(&mut self, o: &Order, price: Decimal, qty: Decimal, cost: Decimal, fee: Decimal, taker: bool, now: i64) -> i64 {
        self.ensure_tx();
        self.db
            .execute(
                "INSERT INTO fills(order_id, user_id, season_id, symbol, side, price, qty, cost, fee, liquidity, ts_ms)
                 VALUES(?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    o.id,
                    o.user_id,
                    o.season_id,
                    o.symbol,
                    o.side.as_str(),
                    price.to_string(),
                    qty.to_string(),
                    cost.to_string(),
                    fee.to_string(),
                    if taker { "taker" } else { "maker" },
                    now
                ],
            )
            .expect("insert fill");
        self.db.last_insert_rowid()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn publish_fill(&self, o: &Order, fid: i64, price: Decimal, qty: Decimal, cost: Decimal, fee: Decimal, taker: bool, now: i64) {
        let payload = json!({
            "type": "fill",
            "fill": {"id": fid, "order_id": o.id, "symbol": o.symbol, "side": o.side.as_str(),
                     "price": price.to_string(), "qty": qty.to_string(), "cost": cost.to_string(),
                     "fee": fee.to_string(), "liquidity": if taker { "taker" } else { "maker" }, "ts": now},
            "order": o.to_json(),
        })
        .to_string();
        self.publish(Event::User { user_id: o.user_id, payload: payload.into() });
    }

    /// Close a live order: release its reservation and take it off the book.
    pub(crate) fn finish(&mut self, oid: i64, status: Status, reason: Option<&str>, now: i64) {
        let Some(mut o) = self.orders.remove(&oid) else { return };
        if !o.locked.is_zero() && self.perps.contains_key(&o.symbol) {
            self.fut_unlock(o.user_id, o.locked);
            o.locked = Decimal::ZERO;
        }
        if !o.locked.is_zero() {
            let (base, quote) = o.symbol.split_once('/').unwrap_or((&o.symbol, &self.cfg.quote));
            let asset = if o.side == Side::Buy { quote.to_string() } else { base.to_string() };
            self.balances.entry((o.user_id, asset.clone())).or_default().locked -= o.locked;
            o.locked = Decimal::ZERO;
            self.save_bal(o.user_id, &asset);
        }
        o.status = status;
        o.reason = reason.map(|s| s.to_string());
        o.updated_ms = now;
        o.queue_ahead = None;
        self.save_order(&o);
        self.emit_order(&o);
    }

    pub fn cancel_order(&mut self, uid: i64, oid: i64, now: i64) -> Result<(), String> {
        match self.orders.get(&oid) {
            Some(o) if o.user_id == uid => {}
            _ => return Err("order not found or no longer open".into()),
        }
        self.finish(oid, Status::Canceled, Some("cancelled by user"), now);
        self.commit();
        Ok(())
    }

    pub fn cancel_all(&mut self, uid: i64, symbol: Option<&str>, reason: &str, now: i64) -> usize {
        let ids: Vec<i64> = self
            .orders
            .values()
            .filter(|o| o.user_id == uid && symbol.map(|s| s == o.symbol).unwrap_or(true))
            .map(|o| o.id)
            .collect();
        for id in &ids {
            self.finish(*id, Status::Canceled, Some(reason), now);
        }
        self.commit();
        ids.len()
    }

    // ---------------------------------------------------------------- queries

    pub fn open_orders(&self, uid: i64) -> Vec<Value> {
        self.orders.values().rev().filter(|o| o.user_id == uid).map(|o| o.to_json()).collect()
    }

    pub fn order_history(&self, uid: i64, limit: i64) -> Vec<Value> {
        let mut st = self
            .db
            .prepare(&format!(
                "SELECT {ORDER_COLS} FROM orders WHERE user_id=? AND season_id=?
                 AND status NOT IN ('pending','untriggered','open') ORDER BY id DESC LIMIT ?"
            ))
            .unwrap();
        let rows = st.query_map(params![uid, self.season_id, limit], row_to_order).unwrap();
        rows.map(|r| r.unwrap().to_json()).collect()
    }

    pub fn get_order(&self, uid: i64, oid: i64) -> Option<Value> {
        if let Some(o) = self.orders.get(&oid) {
            return (o.user_id == uid).then(|| o.to_json());
        }
        self.db
            .query_row(&format!("SELECT {ORDER_COLS} FROM orders WHERE id=? AND user_id=?"), params![oid, uid], row_to_order)
            .ok()
            .map(|o| o.to_json())
    }

    pub fn fills(&self, uid: i64, limit: i64) -> Vec<Value> {
        let mut st = self
            .db
            .prepare(
                "SELECT f.id, f.order_id, f.symbol, f.side, f.price, f.qty, f.cost, f.fee, f.liquidity, f.ts_ms, o.note
                 FROM fills f JOIN orders o ON o.id=f.order_id
                 WHERE f.user_id=? AND f.season_id=? ORDER BY f.id DESC LIMIT ?",
            )
            .unwrap();
        let rows = st
            .query_map(params![uid, self.season_id, limit], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "order_id": r.get::<_, i64>(1)?, "symbol": r.get::<_, String>(2)?,
                    "side": r.get::<_, String>(3)?, "price": r.get::<_, String>(4)?, "qty": r.get::<_, String>(5)?,
                    "cost": r.get::<_, String>(6)?, "fee": r.get::<_, String>(7)?, "liquidity": r.get::<_, String>(8)?,
                    "ts": r.get::<_, i64>(9)?, "note": r.get::<_, Option<String>>(10)?,
                }))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// Balances with mark value and average entry cost (fees included) per asset.
    pub fn portfolio(&self, uid: i64) -> Value {
        // average cost from this season's fills
        let mut basis: HashMap<String, (Decimal, Decimal)> = HashMap::new(); // base -> (qty, cost)
        {
            let mut st = self
                .db
                .prepare("SELECT symbol, side, qty, cost, fee FROM fills WHERE user_id=? AND season_id=? ORDER BY id")
                .unwrap();
            let rows = st
                .query_map(params![uid, self.season_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })
                .unwrap();
            for row in rows {
                let (sym, side, q, c, f) = row.unwrap();
                let base = sym.split('/').next().unwrap_or("").to_string();
                let (q, c, f) = (sdec(q), sdec(c), sdec(f));
                let e = basis.entry(base).or_insert((Decimal::ZERO, Decimal::ZERO));
                if side == "buy" {
                    e.0 += q;
                    e.1 += c + f;
                } else if e.0 > Decimal::ZERO {
                    let frac = (q / e.0).min(Decimal::ONE);
                    e.1 -= e.1 * frac;
                    e.0 -= q.min(e.0);
                }
            }
        }
        let mut assets = vec![];
        let mut equity = Decimal::ZERO;
        let mut keys: Vec<&(i64, String)> = self.balances.keys().filter(|k| k.0 == uid).collect();
        keys.sort_by_key(|k| (k.1 != self.cfg.quote, k.1.clone()));
        for k in keys {
            let b = &self.balances[k];
            let is_quote = k.1 == self.cfg.quote;
            if b.total.is_zero() && !is_quote {
                continue;
            }
            let price = if is_quote { Some(Decimal::ONE) } else { self.mark(&format!("{}/{}", k.1, self.cfg.quote)) };
            let value = price.map(|p| b.total * p);
            equity += value.unwrap_or(Decimal::ZERO);
            let avg = basis.get(&k.1).filter(|e| e.0 > Decimal::ZERO).map(|e| e.1 / e.0);
            let pnl = match (avg, price, is_quote) {
                (Some(a), Some(p), false) => Some((p - a) * b.total),
                _ => None,
            };
            let s = |x: Decimal| x.round_dp(8).normalize().to_string();
            assets.push(json!({
                "asset": k.1, "total": s(b.total), "locked": s(b.locked), "available": s(b.total - b.locked),
                "price": price.map(s), "value": value.map(s), "avg_cost": avg.map(s), "unrealized_pnl": pnl.map(s),
            }));
        }
        // the futures account counts too: wallet + isolated margin + unrealised P&L
        let futures = self.fut_equity(uid);
        equity += futures;
        json!({"equity": equity.round_dp(2).to_string(), "futures_equity": futures.round_dp(2).to_string(), "assets": assets,
               "start_balance": self.cfg.start_balance.normalize().to_string()})
    }

    pub fn tickers_json(&self, now: i64) -> Value {
        let mut out = vec![];
        for s in &self.cfg.symbols {
            let t = self.tickers.get(s).cloned().unwrap_or_default();
            let b = self.books.get(s).filter(|b| b.ready);
            let st = |x: Option<Decimal>| x.map(|v| v.normalize().to_string());
            out.push(json!({
                "symbol": s,
                "last": st(t.last),
                "bid": st(b.and_then(|b| b.best(Side::Buy)).map(|l| l.0)),
                "ask": st(b.and_then(|b| b.best(Side::Sell)).map(|l| l.0)),
                "high": st(t.high), "low": st(t.low), "volume": st(t.volume), "change_pct": st(t.change_pct),
                "tradable": self.tradable(s, now),
            }));
        }
        out.extend(self.fut_tickers(now));
        json!({"type": "tickers", "feed_ok": self.feed_ok(now), "fut_feed_ok": self.fut_feed_ok(now), "ts": now, "tickers": out})
    }

    // ---------------------------------------------------------------- stats, seasons, audit

    /// Record every account's equity; tracks peak / max drawdown and the daily close.
    pub fn snapshot_equity(&mut self, now: i64) {
        if !self.cfg.symbols.iter().any(|s| self.mark(s).is_some()) {
            return;
        }
        let uids: Vec<i64> = {
            let mut st = self.db.prepare("SELECT id FROM users WHERE disabled=0").unwrap();
            let rows = st.query_map([], |r| r.get::<_, i64>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        let day = now / 86_400_000;
        for uid in uids {
            let eq = self.equity(uid);
            let e = self.stats.entry(uid).or_insert((self.cfg.start_balance, 0.0));
            if eq > e.0 {
                e.0 = eq;
            }
            if e.0 > Decimal::ZERO {
                let dd: f64 = ((e.0 - eq) / e.0).try_into().unwrap_or(0.0);
                if dd > e.1 {
                    e.1 = dd;
                }
            }
            let (peak, mdd) = *e;
            self.ensure_tx();
            let eqs = eq.round_dp(4).to_string();
            self.db
                .execute(
                    "INSERT INTO equity_snapshots(user_id, season_id, ts_ms, equity) VALUES(?,?,?,?)",
                    params![uid, self.season_id, now, eqs],
                )
                .unwrap();
            self.db
                .execute(
                    "INSERT INTO equity_daily(user_id, season_id, day, equity) VALUES(?,?,?,?)
                     ON CONFLICT(user_id, season_id, day) DO UPDATE SET equity=excluded.equity",
                    params![uid, self.season_id, day, eqs],
                )
                .unwrap();
            self.db
                .execute(
                    "INSERT INTO user_stats(user_id, season_id, peak, max_dd) VALUES(?,?,?,?)
                     ON CONFLICT(user_id, season_id) DO UPDATE SET peak=excluded.peak, max_dd=excluded.max_dd",
                    params![uid, self.season_id, peak.to_string(), mdd],
                )
                .unwrap();
        }
        self.commit();
    }

    pub fn equity_curve(&self, uid: i64) -> Vec<Value> {
        let mut st = self
            .db
            .prepare("SELECT ts_ms, equity FROM equity_snapshots WHERE user_id=? AND season_id=? ORDER BY ts_ms")
            .unwrap();
        let rows: Vec<(i64, String)> = st
            .query_map(params![uid, self.season_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        // thin long histories to ~1500 points
        let step = (rows.len() / 1500).max(1);
        let n = rows.len();
        rows.into_iter()
            .enumerate()
            .filter(|(i, _)| i % step == 0 || *i == n - 1)
            .map(|(_, (t, e))| json!([t, e]))
            .collect()
    }

    fn sharpe(&self, uid: i64) -> Option<f64> {
        let mut st = self
            .db
            .prepare("SELECT equity FROM equity_daily WHERE user_id=? AND season_id=? ORDER BY day")
            .ok()?;
        let eq: Vec<f64> = st
            .query_map(params![uid, self.season_id], |r| r.get::<_, String>(0))
            .ok()?
            .filter_map(|r| r.ok().and_then(|s| s.parse::<f64>().ok()))
            .collect();
        if eq.len() < 6 {
            return None; // need at least 5 daily returns to say anything
        }
        let rets: Vec<f64> = eq.windows(2).filter(|w| w[0] > 0.0).map(|w| w[1] / w[0] - 1.0).collect();
        let n = rets.len() as f64;
        let mean = rets.iter().sum::<f64>() / n;
        let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        if var <= 0.0 {
            return None;
        }
        Some(mean / var.sqrt() * 365f64.sqrt())
    }

    pub fn leaderboard(&self) -> Vec<Value> {
        let users: Vec<(i64, String)> = {
            let mut st = self.db.prepare("SELECT id, username FROM users WHERE disabled=0").unwrap();
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        let mut trades: HashMap<i64, i64> = HashMap::new();
        {
            let mut st = self
                .db
                .prepare("SELECT user_id, COUNT(DISTINCT order_id) FROM fills WHERE season_id=? GROUP BY user_id")
                .unwrap();
            let rows = st.query_map([self.season_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).unwrap();
            for r in rows {
                let (u, c) = r.unwrap();
                trades.insert(u, c);
            }
        }
        let start = self.cfg.start_balance;
        let mut rows: Vec<(f64, Value)> = users
            .into_iter()
            .map(|(uid, name)| {
                let eq = self.equity(uid);
                let ret: f64 = ((eq / start - Decimal::ONE) * Decimal::ONE_HUNDRED).try_into().unwrap_or(0.0);
                let (peak, mdd) = self.stats.get(&uid).copied().unwrap_or((start, 0.0));
                let peak = peak.max(eq);
                let live_dd: f64 = if peak > Decimal::ZERO { ((peak - eq) / peak).try_into().unwrap_or(0.0) } else { 0.0 };
                let mdd = mdd.max(live_dd) * 100.0;
                // return per unit of worst drawdown; a floor keeps tiny drawdowns from exploding it
                let risk_adj = ret / mdd.max(1.0);
                (
                    ret,
                    json!({
                        "user_id": uid, "username": name, "equity": eq.round_dp(2).to_string(),
                        "return_pct": ret, "max_dd_pct": mdd, "risk_adj": risk_adj,
                        "sharpe": self.sharpe(uid), "trades": trades.get(&uid).copied().unwrap_or(0),
                    }),
                )
            })
            .collect();
        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        rows.into_iter()
            .enumerate()
            .map(|(i, (_, mut v))| {
                v["rank"] = json!(i + 1);
                v
            })
            .collect()
    }

    /// Archive the standings, cancel every order, and reset everyone to the starting balance.
    pub fn new_season(&mut self, name: &str, now: i64) {
        let board = self.leaderboard();
        self.ensure_tx();
        for row in &board {
            self.db
                .execute(
                    "INSERT INTO season_results(season_id, rank, user_id, username, equity, return_pct, max_dd_pct, sharpe, trades)
                     VALUES(?,?,?,?,?,?,?,?,?)",
                    params![
                        self.season_id,
                        row["rank"].as_i64(),
                        row["user_id"].as_i64(),
                        row["username"].as_str(),
                        row["equity"].as_str(),
                        row["return_pct"].as_f64(),
                        row["max_dd_pct"].as_f64(),
                        row["sharpe"].as_f64(),
                        row["trades"].as_i64()
                    ],
                )
                .unwrap();
        }
        let ids: Vec<i64> = self.orders.keys().copied().collect();
        for id in ids {
            self.finish(id, Status::Canceled, Some("season ended"), now);
        }
        self.fut_season_reset(now);
        // sweep all balances back to the treasury
        let keys: Vec<(i64, String)> = self.balances.keys().cloned().collect();
        for (uid, asset) in keys {
            let total = self.balances[&(uid, asset.clone())].total;
            if !total.is_zero() {
                let txn = self.new_txn();
                self.transfer(txn, now, &format!("user:{uid}"), "treasury", &asset, total, "season_reset", self.season_id);
            }
            self.balances.insert((uid, asset.clone()), Bal::default());
            self.save_bal(uid, &asset);
        }
        self.db.execute("UPDATE seasons SET end_ms=? WHERE id=?", params![now, self.season_id]).unwrap();
        self.db
            .execute(
                "INSERT INTO seasons(name, start_ms, start_balance) VALUES(?,?,?)",
                params![name, now, self.cfg.start_balance.to_string()],
            )
            .unwrap();
        self.season_id = self.db.last_insert_rowid();
        self.season_name = name.to_string();
        self.season_start_ms = now;
        self.stats.clear();
        let uids: Vec<i64> = {
            let mut st = self.db.prepare("SELECT id FROM users").unwrap();
            let rows = st.query_map([], |r| r.get::<_, i64>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        for uid in uids {
            self.grant(uid, now);
        }
        self.commit();
    }

    pub fn past_seasons(&self) -> Vec<Value> {
        let mut st = self
            .db
            .prepare(
                "SELECT s.id, s.name, s.start_ms, s.end_ms, r.rank, r.username, r.equity, r.return_pct, r.max_dd_pct, r.trades
                 FROM seasons s JOIN season_results r ON r.season_id=s.id
                 WHERE s.end_ms IS NOT NULL ORDER BY s.id DESC, r.rank",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok(json!({
                    "season_id": r.get::<_, i64>(0)?, "season": r.get::<_, String>(1)?,
                    "start_ms": r.get::<_, i64>(2)?, "end_ms": r.get::<_, Option<i64>>(3)?,
                    "rank": r.get::<_, i64>(4)?, "username": r.get::<_, String>(5)?, "equity": r.get::<_, String>(6)?,
                    "return_pct": r.get::<_, f64>(7)?, "max_dd_pct": r.get::<_, f64>(8)?, "trades": r.get::<_, i64>(9)?,
                }))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// Audit: the ledger must sum to zero per asset, every user balance must
    /// equal the sum of its ledger lines, and reservations must match live orders.
    pub fn reconcile(&self) -> Value {
        let mut sums: HashMap<(String, String), Decimal> = HashMap::new();
        let mut per_asset: HashMap<String, Decimal> = HashMap::new();
        let mut lines = 0i64;
        {
            let mut st = self.db.prepare("SELECT account, asset, amount FROM ledger").unwrap();
            let rows = st
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
                .unwrap();
            for row in rows {
                let (acct, asset, amt) = row.unwrap();
                let a = sdec(amt);
                *sums.entry((acct, asset.clone())).or_default() += a;
                *per_asset.entry(asset).or_default() += a;
                lines += 1;
            }
        }
        let mut problems = vec![];
        for (asset, s) in &per_asset {
            if !s.is_zero() {
                problems.push(format!("ledger for {asset} does not sum to zero: {s}"));
            }
        }
        for ((uid, asset), b) in &self.balances {
            let l = sums.get(&(format!("user:{uid}"), asset.clone())).copied().unwrap_or(Decimal::ZERO);
            if l != b.total {
                problems.push(format!("user {uid} {asset}: balance {} != ledger {}", b.total, l));
            }
            let locked: Decimal = self
                .orders
                .values()
                .filter(|o| o.user_id == *uid)
                .filter(|o| {
                    let (base, quote) = o.symbol.split_once('/').unwrap_or(("", ""));
                    (if o.side == Side::Buy { quote } else { base }) == asset
                })
                .map(|o| o.locked)
                .sum();
            if locked != b.locked {
                problems.push(format!("user {uid} {asset}: locked {} != open-order reservations {}", b.locked, locked));
            }
            if b.total < Decimal::ZERO || b.locked < Decimal::ZERO || b.locked > b.total {
                problems.push(format!("user {uid} {asset}: invalid balance total {} locked {}", b.total, b.locked));
            }
        }
        self.fut_reconcile(&sums, &mut problems);
        let sys = |name: &str| -> Value {
            let m: serde_json::Map<String, Value> = sums
                .iter()
                .filter(|(k, _)| k.0 == name)
                .map(|(k, v)| (k.1.clone(), json!(v.normalize().to_string())))
                .collect();
            Value::Object(m)
        };
        json!({
            "ok": problems.is_empty(), "problems": problems, "ledger_lines": lines,
            "fees_collected": sys("fees"), "market": sys("market"), "treasury": sys("treasury"),
            "insurance_fund": sys("insurance"), "funding": sys("funding"),
        })
    }
}

const ORDER_COLS: &str = "id, user_id, season_id, symbol, side, otype, tif, post_only, qty, limit_price, \
     trigger_price, filled, cum_cost, fee, status, locked, note, reason, created_ms, updated_ms, reduce_only";

fn row_to_order(r: &rusqlite::Row) -> rusqlite::Result<Order> {
    Ok(Order {
        id: r.get(0)?,
        user_id: r.get(1)?,
        season_id: r.get(2)?,
        symbol: r.get(3)?,
        side: Side::parse(&r.get::<_, String>(4)?).unwrap_or(Side::Buy),
        otype: OType::parse(&r.get::<_, String>(5)?).unwrap_or(OType::Limit),
        tif: Tif::parse(&r.get::<_, String>(6)?).unwrap_or(Tif::Gtc),
        post_only: r.get::<_, i64>(7)? != 0,
        qty: sdec(r.get(8)?),
        limit_price: odec(r.get(9)?),
        trigger_price: odec(r.get(10)?),
        filled: sdec(r.get(11)?),
        cum_cost: sdec(r.get(12)?),
        fee: sdec(r.get(13)?),
        status: Status::parse(&r.get::<_, String>(14)?),
        locked: sdec(r.get(15)?),
        queue_ahead: None,
        note: r.get(16)?,
        reason: r.get(17)?,
        created_ms: r.get(18)?,
        updated_ms: r.get(19)?,
        reduce_only: r.get::<_, i64>(20)? != 0,
    })
}
