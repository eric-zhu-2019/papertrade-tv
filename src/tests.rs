use crate::accounts::normalize_invite;
use crate::book::{Book, Side};
use crate::config::Config;
use crate::engine::*;
use crate::feed::handle_message;
use crate::futures::MarginMode;
use crate::kfeed::{apply_instruments, handle_fut_message};
use crate::util::*;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::broadcast;

const T0: i64 = 1_800_000_000_000;

fn cfg() -> Config {
    Config {
        bind: String::new(),
        db_path: ":memory:".into(),
        web_dir: String::new(),
        feed: "test".into(),
        symbols: vec!["BTC/USD".into()],
        quote: "USD".into(),
        club_name: "t".into(),
        invite_code: "x".into(),
        admin_user: "admin".into(),
        admin_password: String::new(),
        start_balance: d("100000"),
        maker_fee: d("0.0025"),
        taker_fee: d("0.004"),
        latency_min_ms: 0,
        latency_max_ms: 0,
        liquidity_ttl_ms: 30_000,
        stale_ms: 5_000,
        book_depth: 100,
        checksum: "enforce".into(),
        require_note: false,
        snapshot_secs: 300,
        max_open_orders: 100,
        orders_per_sec: 1000.0,
        perps: vec!["BTC-PERP".into()],
        fut_maker_fee: d("0.0002"),
        fut_taker_fee: d("0.0005"),
        liq_fee: d("0.005"),
        fut_default_leverage: d("10"),
        sim_funding_secs: 3600,
    }
}

fn lv(v: &[(&str, &str)]) -> Vec<(Decimal, Decimal)> {
    v.iter().map(|(p, q)| (d(p), d(q))).collect()
}

/// Engine with one user and a BTC/USD book: bids 99.9 x1, 99.8 x2 / asks 100.0 x1, 100.1 x2, 100.2 x5
fn setup() -> (Engine, i64) {
    let (tx, _) = broadcast::channel(64);
    let mut e = Engine::open(Arc::new(cfg()), tx, ":memory:");
    e.set_instrument(Instrument {
        symbol: "BTC/USD".into(),
        kind: "spot",
        base: "BTC".into(),
        quote: "USD".into(),
        price_increment: d("0.1"),
        qty_increment: d("0.00000001"),
        qty_min: d("0.0001"),
        cost_min: d("0.5"),
        price_precision: 1,
        qty_precision: 8,
        status: "online".into(),
    });
    e.feed_up(T0);
    e.on_book_snapshot("BTC/USD", &lv(&[("99.9", "1"), ("99.8", "2")]), &lv(&[("100.0", "1"), ("100.1", "2"), ("100.2", "5")]), T0);
    let uid = e.create_user("alice", "x", false, T0).unwrap();
    (e, uid)
}

fn req(side: Side, otype: OType, qty: &str, limit: Option<&str>) -> OrderReq {
    OrderReq {
        symbol: "BTC/USD".into(),
        side,
        otype,
        qty: d(qty),
        limit_price: limit.map(d),
        trigger_price: None,
        tif: Tif::Gtc,
        post_only: false,
        reduce_only: false,
        note: None,
    }
}

/// place + let the order reach the book
fn go(e: &mut Engine, uid: i64, r: OrderReq) -> i64 {
    let o = e.place_order(uid, r, T0).unwrap();
    e.activate(o.id, T0);
    o.id
}

fn order(e: &Engine, uid: i64, id: i64) -> serde_json::Value {
    e.get_order(uid, id).unwrap()
}

fn bal(e: &Engine, uid: i64, asset: &str) -> (Decimal, Decimal) {
    let b = e.balances.get(&(uid, asset.to_string())).cloned().unwrap_or_default();
    (b.total, b.locked)
}

fn audit(e: &Engine) {
    let r = e.reconcile();
    assert_eq!(r["ok"], true, "ledger audit failed: {}", r["problems"]);
}

#[test]
fn kraken_checksum_matches_documented_example() {
    let mut b = Book::new(10);
    let asks = lv(&[
        ("45285.2", "0.00100000"), ("45286.4", "1.54571953"), ("45286.6", "1.54571109"), ("45289.6", "1.54560911"),
        ("45290.2", "0.15890660"), ("45291.8", "1.54553491"), ("45294.7", "0.04454749"), ("45296.1", "0.35380000"),
        ("45297.5", "0.09945542"), ("45299.5", "0.18772827"),
    ]);
    let bids = lv(&[
        ("45283.5", "0.10000000"), ("45283.4", "1.54582015"), ("45282.1", "0.10000000"), ("45281.0", "0.10000000"),
        ("45280.3", "1.54592586"), ("45279.0", "0.07990000"), ("45277.6", "0.03310103"), ("45277.5", "0.30000000"),
        ("45277.3", "1.54602737"), ("45276.6", "0.15445238"),
    ]);
    b.snapshot(&bids, &asks);
    assert_eq!(b.checksum(1, 8), 3310070434);
}

#[test]
fn rfc3339() {
    assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_rfc3339_ms("2023-09-25T07:49:37.708706Z"), Some(1695628177708));
    assert_eq!(parse_rfc3339_ms("2024-02-29T12:00:00.5Z"), Some(1709208000500));
}

#[test]
fn market_buy_walks_the_real_book_with_slippage_and_fees() {
    let (mut e, u) = setup();
    let id = go(&mut e, u, req(Side::Buy, OType::Market, "2.5", None));
    let o = order(&e, u, id);
    assert_eq!(o["status"], "filled");
    // 1 @ 100.0 + 1.5 @ 100.1 = 250.15 ; taker fee 0.4% = 1.0006
    assert_eq!(d(o["cum_cost"].as_str().unwrap()), d("250.15"));
    assert_eq!(d(o["fee"].as_str().unwrap()), d("1.0006"));
    assert_eq!(o["avg_price"], "100.06");
    assert_eq!(bal(&e, u, "BTC"), (d("2.5"), d("0")));
    assert_eq!(bal(&e, u, "USD"), (d("100000") - d("251.1506"), d("0")));
    assert_eq!(e.fills(u, 10).len(), 2);
    audit(&e);
}

#[test]
fn eaten_liquidity_cannot_be_hit_twice_until_it_replenishes() {
    let (mut e, u) = setup();
    go(&mut e, u, req(Side::Buy, OType::Market, "1", None)); // takes all of 100.0
    let id = go(&mut e, u, req(Side::Buy, OType::Market, "1", None));
    assert_eq!(order(&e, u, id)["avg_price"], "100.1", "second order must pay the next level");
    // after the TTL the level counts as replenished
    let o = e.place_order(u, req(Side::Buy, OType::Market, "1", None), T0).unwrap();
    e.feed_up(T0 + 31_000);
    e.activate(o.id, T0 + 31_000);
    assert_eq!(order(&e, u, o.id)["avg_price"], "100");
    audit(&e);
}

#[test]
fn market_order_larger_than_the_book_is_partially_filled() {
    let (mut e, u) = setup();
    let id = go(&mut e, u, req(Side::Buy, OType::Market, "20", None));
    let o = order(&e, u, id);
    assert_eq!(o["status"], "canceled");
    assert_eq!(d(o["filled"].as_str().unwrap()), d("8"));
    audit(&e);
}

#[test]
fn market_buy_cannot_spend_more_than_the_balance() {
    let (mut e, u) = setup();
    e.on_book_snapshot("BTC/USD", &lv(&[("99999.9", "5")]), &lv(&[("100000.0", "5")]), T0);
    assert!(e.place_order(u, req(Side::Buy, OType::Market, "1", None), T0).is_err());
    // the price moves up between acceptance and arrival: fill only what is affordable
    let o = e.place_order(u, req(Side::Buy, OType::Market, "0.99", None), T0).unwrap();
    e.on_book_snapshot("BTC/USD", &lv(&[("109999.9", "5")]), &lv(&[("110000.0", "5")]), T0);
    e.activate(o.id, T0);
    let (usd, locked) = bal(&e, u, "USD");
    assert!(usd >= Decimal::ZERO && usd < d("0.01"), "left over: {usd}");
    assert_eq!(locked, d("0"));
    assert!(bal(&e, u, "BTC").0 < d("0.99"));
    audit(&e);
}

#[test]
fn resting_limit_order_waits_behind_the_real_queue() {
    let (mut e, u) = setup();
    // join the bid at 99.9 behind 1 BTC of real orders
    let id = go(&mut e, u, req(Side::Buy, OType::Limit, "0.5", Some("99.9")));
    assert_eq!(order(&e, u, id)["status"], "open");
    assert_eq!(e.orders[&id].queue_ahead, Some(d("1")));
    let (_, locked) = bal(&e, u, "USD");
    assert_eq!(locked, ceil8(d("0.5") * d("99.9") * d("1.004")));

    // a real seller hits 99.9 for 0.6: all of it goes to the queue ahead of us
    e.on_trade("BTC/USD", d("99.9"), d("0.6"), Side::Sell, T0, true, T0);
    assert_eq!(e.orders[&id].queue_ahead, Some(d("0.4")));
    assert_eq!(e.orders[&id].filled, d("0"));
    // a buy-side trade at our price is not a sell into the bid: no effect
    e.on_trade("BTC/USD", d("99.9"), d("5"), Side::Buy, T0, true, T0);
    assert_eq!(e.orders[&id].filled, d("0"));
    // 0.6 more: 0.4 clears the queue, 0.2 reaches us
    e.on_trade("BTC/USD", d("99.9"), d("0.6"), Side::Sell, T0, true, T0);
    assert_eq!(e.orders[&id].filled, d("0.2"));
    assert_eq!(e.orders[&id].queue_ahead, Some(d("0")));
    // the rest
    e.on_trade("BTC/USD", d("99.9"), d("1"), Side::Sell, T0, true, T0);
    let o = order(&e, u, id);
    assert_eq!(o["status"], "filled");
    // maker fee 0.25% of 49.95
    assert_eq!(d(o["fee"].as_str().unwrap()), d("0.124875"));
    assert_eq!(bal(&e, u, "USD"), (d("100000") - d("49.95") - d("0.124875"), d("0")));
    audit(&e);
}

#[test]
fn cancellations_ahead_advance_the_queue_and_trade_through_fills() {
    let (mut e, u) = setup();
    let id = go(&mut e, u, req(Side::Buy, OType::Limit, "0.5", Some("99.9")));
    // the level shrinks to 0.3 without trades: at most 0.3 can still be ahead
    e.on_book_update("BTC/USD", &lv(&[("99.9", "0.3")]), &[], T0);
    assert_eq!(e.orders[&id].queue_ahead, Some(d("0.3")));
    // level grows again: new orders queue behind us
    e.on_book_update("BTC/USD", &lv(&[("99.9", "4")]), &[], T0);
    assert_eq!(e.orders[&id].queue_ahead, Some(d("0.3")));
    // a trade prints below our bid: the market went through us, but only for the traded size
    e.on_trade("BTC/USD", d("99.8"), d("0.2"), Side::Sell, T0, true, T0);
    assert_eq!(e.orders[&id].filled, d("0.2"));
    let f = &e.fills(u, 1)[0];
    assert_eq!(f["price"], "99.9", "filled at our limit price, as maker");
    assert_eq!(f["liquidity"], "maker");
    audit(&e);
}

#[test]
fn one_real_trade_is_shared_not_duplicated_between_students() {
    let (mut e, u) = setup();
    let u2 = e.create_user("bob", "x", false, T0).unwrap();
    // both improve the bid to 99.95? tick is 0.1, so both sit alone at a new level inside the spread
    e.on_book_snapshot("BTC/USD", &lv(&[("99.8", "1")]), &lv(&[("100.0", "1")]), T0);
    let a = go(&mut e, u, req(Side::Buy, OType::Limit, "1", Some("99.9")));
    let b = go(&mut e, u2, req(Side::Buy, OType::Limit, "1", Some("99.9")));
    e.on_trade("BTC/USD", d("99.8"), d("1.5"), Side::Sell, T0, true, T0);
    assert_eq!(order(&e, u, a)["status"], "filled");
    assert_eq!(e.orders[&b].filled, d("0.5"));
    audit(&e);
}

#[test]
fn marketable_limit_takes_then_rests_and_releases_price_improvement() {
    let (mut e, u) = setup();
    // limit 100.1 for 4: takes 1 @ 100.0 and 2 @ 100.1, rests 1 @ 100.1
    let id = go(&mut e, u, req(Side::Buy, OType::Limit, "4", Some("100.1")));
    let o = &e.orders[&id];
    assert_eq!(o.status, Status::Open);
    assert_eq!(o.filled, d("3"));
    assert_eq!(o.queue_ahead, Some(d("0")));
    assert_eq!(bal(&e, u, "USD").1, ceil8(d("1") * d("100.1") * d("1.004")));
    e.cancel_order(u, id, T0).unwrap();
    assert_eq!(bal(&e, u, "USD").1, d("0"));
    assert_eq!(order(&e, u, id)["status"], "canceled");
    audit(&e);
}

#[test]
fn post_only_ioc_and_fok() {
    let (mut e, u) = setup();
    let mut r = req(Side::Buy, OType::Limit, "1", Some("100.0"));
    r.post_only = true;
    let id = go(&mut e, u, r);
    assert_eq!(order(&e, u, id)["status"], "canceled");
    assert_eq!(bal(&e, u, "USD"), (d("100000"), d("0")));

    let mut r = req(Side::Buy, OType::Limit, "5", Some("100.1"));
    r.tif = Tif::Fok;
    let id = go(&mut e, u, r);
    assert_eq!(order(&e, u, id)["filled"], "0", "FOK must not partially fill");

    let mut r = req(Side::Buy, OType::Limit, "5", Some("100.1"));
    r.tif = Tif::Ioc;
    let id = go(&mut e, u, r);
    let o = order(&e, u, id);
    assert_eq!(o["status"], "canceled");
    assert_eq!(d(o["filled"].as_str().unwrap()), d("3"));
    assert_eq!(bal(&e, u, "USD").1, d("0"));
    audit(&e);
}

#[test]
fn sell_side_and_stop_loss() {
    let (mut e, u) = setup();
    go(&mut e, u, req(Side::Buy, OType::Market, "1", None));
    // cannot sell more than we hold
    assert!(e.place_order(u, req(Side::Sell, OType::Limit, "1.5", Some("101.0")), T0).is_err());
    let mut r = req(Side::Sell, OType::StopLoss, "1", None);
    r.trigger_price = Some(d("99.5"));
    let o = e.place_order(u, r, T0).unwrap();
    assert_eq!(o.status, Status::Untriggered);
    assert_eq!(bal(&e, u, "BTC"), (d("1"), d("1")));
    // coins are reserved by the stop
    assert!(e.place_order(u, req(Side::Sell, OType::Limit, "0.5", Some("101.0")), T0).is_err());
    e.on_trade("BTC/USD", d("99.9"), d("0.1"), Side::Sell, T0, true, T0);
    assert_eq!(e.orders[&o.id].status, Status::Untriggered);
    // the market drops through the trigger: sells at market into the bids (1 @ 99.9)
    e.on_trade("BTC/USD", d("99.5"), d("0.1"), Side::Sell, T0, true, T0);
    let done = order(&e, u, o.id);
    assert_eq!(done["status"], "filled");
    assert_eq!(done["avg_price"], "99.9");
    assert_eq!(bal(&e, u, "BTC"), (d("0"), d("0")));
    audit(&e);
}

#[test]
fn validation_and_halts() {
    let (mut e, u) = setup();
    assert!(e.place_order(u, req(Side::Buy, OType::Limit, "1", Some("99.95")), T0).is_err(), "off-tick price");
    assert!(e.place_order(u, req(Side::Buy, OType::Limit, "0.00001", Some("99.9")), T0).is_err(), "below min qty");
    assert!(e.place_order(u, req(Side::Buy, OType::Limit, "2000", Some("99.9")), T0).is_err(), "insufficient funds");
    assert!(e.place_order(u, req(Side::Buy, OType::Limit, "1", None), T0).is_err(), "limit without price");
    // stale feed: no new orders
    assert!(e.place_order(u, req(Side::Buy, OType::Market, "1", None), T0 + 6_000).is_err());
    // feed dies while an order is in flight: rejected, funds released
    let o = e.place_order(u, req(Side::Buy, OType::Limit, "1", Some("100.0")), T0).unwrap();
    e.feed_down();
    e.activate(o.id, T0 + 100);
    assert_eq!(order(&e, u, o.id)["status"], "rejected");
    assert_eq!(bal(&e, u, "USD"), (d("100000"), d("0")));
    audit(&e);
}

#[test]
fn crossed_resting_order_fills_after_a_gap() {
    let (mut e, u) = setup();
    let id = go(&mut e, u, req(Side::Buy, OType::Limit, "1", Some("99.9")));
    e.feed_down();
    e.feed_up(T0);
    // the market fell while we were disconnected; asks now sit below our bid
    e.on_book_snapshot("BTC/USD", &lv(&[("98.0", "1")]), &lv(&[("98.1", "0.4"), ("98.2", "3")]), T0);
    let o = order(&e, u, id);
    assert_eq!(o["status"], "filled");
    assert_eq!(o["avg_price"], "99.9");
    audit(&e);
}

#[test]
fn new_season_resets_everyone_and_keeps_the_books_balanced() {
    let (mut e, u) = setup();
    go(&mut e, u, req(Side::Buy, OType::Market, "2", None));
    go(&mut e, u, req(Side::Buy, OType::Limit, "1", Some("99.0")));
    e.snapshot_equity(T0);
    e.new_season("Spring", T0 + 1);
    assert_eq!(bal(&e, u, "USD"), (d("100000"), d("0")));
    assert_eq!(bal(&e, u, "BTC"), (d("0"), d("0")));
    assert!(e.orders.is_empty());
    assert_eq!(e.past_seasons().len(), 1);
    assert_eq!(e.leaderboard()[0]["return_pct"], 0.0);
    audit(&e);
}

#[test]
fn kraken_messages_drive_the_engine() {
    let (tx, _) = broadcast::channel(64);
    let mut c = cfg();
    c.symbols = vec!["MATIC/USD".into()];
    let mut e = Engine::open(Arc::new(c), tx, ":memory:");
    e.feed_up(T0);
    let inst = r#"{"channel":"instrument","type":"snapshot","data":{"assets":[],"pairs":[
      {"symbol":"MATIC/USD","base":"MATIC","quote":"USD","status":"online","qty_precision":8,"qty_increment":1e-08,
       "price_precision":4,"cost_precision":6,"marginable":false,"has_index":true,"cost_min":0.43,
       "tick_size":0.0001,"price_increment":0.0001,"qty_min":4.0},
      {"symbol":"BTC/USD","base":"BTC","quote":"USD","status":"online","qty_precision":8,"qty_increment":1e-08,
       "price_precision":1,"price_increment":0.1,"qty_min":0.0001,"cost_min":0.5}]}}"#;
    handle_message(&mut e, inst, T0, true).unwrap();
    assert_eq!(e.instruments.len(), 1);
    assert_eq!(e.instruments["MATIC/USD"].qty_increment, d("0.00000001"));
    // snapshot copied from the Kraken docs, including its checksum
    let snap = r#"{"channel":"book","type":"snapshot","data":[{"symbol":"MATIC/USD",
      "bids":[{"price":0.5666,"qty":4831.75496356},{"price":0.5665,"qty":6658.22734739},{"price":0.5664,"qty":18724.91513344},
              {"price":0.5663,"qty":11563.92544914},{"price":0.5662,"qty":14006.65365711},{"price":0.5661,"qty":17454.85679807},
              {"price":0.566,"qty":18097.1547},{"price":0.5659,"qty":33644.89175666},{"price":0.5658,"qty":148.3464},
              {"price":0.5657,"qty":606.70854372}],
      "asks":[{"price":0.5668,"qty":4410.79769741},{"price":0.5669,"qty":4655.40412487},{"price":0.567,"qty":49844.89424998},
              {"price":0.5671,"qty":24306.41678},{"price":0.5672,"qty":29783.25223475},{"price":0.5673,"qty":57234.71239278},
              {"price":0.5674,"qty":45065.04744},{"price":0.5675,"qty":5912.76380354},{"price":0.5676,"qty":42514.92434778},
              {"price":0.5677,"qty":36304.0847022}],
      "checksum":2439117997,"timestamp":"2023-10-06T17:35:55.440295Z"}]}"#;
    handle_message(&mut e, snap, T0, true).expect("documented snapshot must pass its checksum");
    assert!(e.tradable("MATIC/USD", T0));
    let trade = r#"{"channel":"trade","type":"update","data":[{"symbol":"MATIC/USD","side":"sell","price":0.5117,
      "qty":40.0,"ord_type":"market","trade_id":4665906,"timestamp":"2023-09-25T07:49:37.708706Z"}]}"#;
    handle_message(&mut e, trade, T0, true).unwrap();
    assert_eq!(e.tickers["MATIC/USD"].last, Some(d("0.5117")));
    // a corrupted update must be detected
    let bad = r#"{"channel":"book","type":"update","data":[{"symbol":"MATIC/USD","bids":[{"price":0.5657,"qty":1.5}],
      "asks":[],"checksum":1,"timestamp":"2023-10-06T17:35:55.440295Z"}]}"#;
    assert!(handle_message(&mut e, bad, T0, true).is_err());
    handle_message(&mut e, r#"{"channel":"heartbeat"}"#, T0, true).unwrap();
}

#[test]
fn one_time_invites_work_once_and_only_when_the_account_is_created() {
    let (mut e, _) = setup();
    let codes = e.create_invites(2, "october", T0);
    assert_eq!(codes.len(), 2);
    assert_ne!(codes[0], codes[1]);
    assert_eq!(e.unused_invites(), 2);
    let c = &codes[0];
    assert_eq!(normalize_invite(&c.to_lowercase().replace('-', " ")), *c);

    // a taken nickname must not use the code up
    assert!(e.register_with_invite(c, "ALICE", "x", T0).is_err());
    assert!(e.invite_usable(c));

    let bob = e.register_with_invite(c, "bob", "x", T0).unwrap();
    assert!(!e.invite_usable(c));
    assert!(e.register_with_invite(c, "carol", "x", T0).is_err());
    let row = e.list_invites().into_iter().find(|i| i["code"] == c.as_str()).unwrap();
    assert_eq!(row["used_by"], bob);
    assert_eq!(row["used_by_name"], "bob");

    // used codes stay on record; unused ones can be revoked and then stop working
    assert!(e.revoke_invite(c).is_err());
    e.revoke_invite(&codes[1]).unwrap();
    assert!(e.register_with_invite(&codes[1], "carol", "x", T0).is_err());
    assert_eq!(e.unused_invites(), 0);
    assert_eq!(bal(&e, bob, "USD").0, d("100000"));
    audit(&e);
}

// ------------------------------------------------------------------ perpetual futures

const PERP: &str = "BTC-PERP";

/// A perp priced around 100 (100 contracts at bid 100.0 and ask 100.1, more behind),
/// Kraken's BTC tiers, and 10,000 USD moved into alice's futures wallet.
fn fut_setup() -> (Engine, i64) {
    let (mut e, uid) = setup();
    e.set_instrument(Instrument {
        symbol: PERP.into(),
        kind: "perp",
        base: "BTC".into(),
        quote: "USD".into(),
        price_increment: d("0.1"),
        qty_increment: d("0.001"),
        qty_min: d("0.001"),
        cost_min: Decimal::ZERO,
        price_precision: 1,
        qty_precision: 3,
        status: "online".into(),
    });
    e.set_perp_rules(PERP, vec![(d("0"), d("0.01"), d("0.005")), (d("1000000"), d("0.02"), d("0.01"))], Some(d("1000")));
    e.fut_feed_up(T0);
    perp_book(&mut e, "100.0", "100.1");
    perp_mark(&mut e, "100");
    e.fut_transfer(uid, d("10000"), true, T0).unwrap();
    (e, uid)
}

/// Replace the perp book with deep levels starting at `bid` / `ask`.
fn perp_book(e: &mut Engine, bid: &str, ask: &str) {
    let (b, a) = (d(bid), d(ask));
    let bids = [(b, d("100")), (b - d("0.1"), d("20")), (b - d("1"), d("1000"))];
    let asks = [(a, d("100")), (a + d("0.1"), d("20")), (a + d("1"), d("1000"))];
    e.on_book_snapshot(PERP, &bids, &asks, T0);
}

fn perp_mark(e: &mut Engine, mark: &str) {
    e.on_perp_ticker(PERP, Some(d(mark)), Some(d(mark)), None, None, None, Ticker::default());
}

fn preq(side: Side, otype: OType, qty: &str, limit: Option<&str>) -> OrderReq {
    OrderReq { symbol: PERP.into(), ..req(side, otype, qty, limit) }
}

fn wallet(e: &Engine, uid: i64) -> Decimal {
    e.fut.get(&uid).map(|a| a.wallet).unwrap_or_default()
}

fn pos_size(e: &Engine, uid: i64) -> Decimal {
    e.positions.get(&(uid, PERP.to_string())).map(|p| p.size).unwrap_or_default()
}

fn new_trader(e: &mut Engine, name: &str, futures_usd: &str) -> i64 {
    let uid = e.create_user(name, "x", false, T0).unwrap();
    e.fut_transfer(uid, d(futures_usd), true, T0).unwrap();
    uid
}

#[test]
fn transfers_between_spot_and_futures_wallets() {
    let (mut e, uid) = fut_setup();
    assert_eq!(bal(&e, uid, "USD").0, d("90000"));
    assert_eq!(wallet(&e, uid), d("10000"));
    assert!(e.fut_transfer(uid, d("10000.01"), false, T0).is_err());
    assert!(e.fut_transfer(uid, d("90000.01"), true, T0).is_err());
    assert!(e.fut_transfer(uid, d("-5"), true, T0).is_err());
    e.fut_transfer(uid, d("2500"), false, T0).unwrap();
    assert_eq!(bal(&e, uid, "USD").0, d("92500"));
    assert_eq!(wallet(&e, uid), d("7500"));
    audit(&e);
}

#[test]
fn isolated_long_opens_with_margin_and_closes_with_profit() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None));
    // 10 @ 100.1 at 10x: margin 100.1, taker fee 0.05% of 1001
    assert_eq!(pos_size(&e, uid), d("10"));
    let p = e.positions[&(uid, PERP.to_string())].clone();
    assert_eq!((p.entry, p.margin, p.mode), (d("100.1"), d("100.1"), MarginMode::Isolated));
    assert_eq!(wallet(&e, uid), d("9899.3995"));
    assert_eq!(e.fut[&uid].locked, Decimal::ZERO);
    // liquidation price for an isolated long: (q*e - M) / (q * (1 - mm))
    let liq = e.liq_price(&p).unwrap();
    assert_eq!(liq.round_dp(4), d("90.5427"));

    perp_book(&mut e, "110.0", "110.1");
    perp_mark(&mut e, "110");
    assert_eq!(e.fut_equity(uid), d("9899.3995") + d("100.1") + d("99"));
    go(&mut e, uid, OrderReq { reduce_only: true, ..preq(Side::Sell, OType::Market, "10", None) });
    assert!(e.positions.is_empty());
    // +99 profit, -0.55 fee, margin back
    assert_eq!(wallet(&e, uid), d("10097.9495"));
    let h = e.position_history(uid, 10);
    assert_eq!(h[0]["side"], "long");
    assert_eq!(h[0]["realized"], "99");
    assert_eq!(h[0]["fees"], "1.0505");
    assert_eq!(h[0]["reason"], "closed");
    audit(&e);
    e.fut_transfer(uid, d("10097.9495"), false, T0).unwrap();
    audit(&e);
}

#[test]
fn cross_short_flips_to_long_in_one_order() {
    let (mut e, uid) = fut_setup();
    e.set_fut_settings(uid, PERP, MarginMode::Cross, d("5")).unwrap();
    go(&mut e, uid, preq(Side::Sell, OType::Market, "10", None));
    assert_eq!(pos_size(&e, uid), d("-10"));
    assert_eq!(wallet(&e, uid), d("9999.5")); // cross margin stays in the wallet; only the fee left
    assert_eq!(e.fut_available(uid), d("9799.5")); // 1000 notional at 5x ties up 200
    assert!(e.positions[&(uid, PERP.to_string())].margin.is_zero());

    // buying 15 closes the short (-0.1 per contract) and opens a 5 long
    go(&mut e, uid, preq(Side::Buy, OType::Market, "15", None));
    assert_eq!(pos_size(&e, uid), d("5"));
    let p = &e.positions[&(uid, PERP.to_string())];
    assert_eq!((p.entry, p.mode, p.long), (d("100.1"), MarginMode::Cross, true));
    assert_eq!(wallet(&e, uid), d("9999.5") - d("1") - d("0.75075"));
    let h = e.position_history(uid, 10);
    assert_eq!((h[0]["side"].as_str(), h[0]["realized"].as_str()), (Some("short"), Some("-1")));
    audit(&e);
}

#[test]
fn reduce_only_orders_and_locked_settings() {
    let (mut e, uid) = fut_setup();
    let ro = |side, qty| OrderReq { reduce_only: true, ..preq(side, OType::Market, qty, None) };
    assert!(e.place_order(uid, ro(Side::Sell, "1"), T0).is_err()); // nothing to reduce
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None));
    assert!(e.place_order(uid, ro(Side::Buy, "1"), T0).is_err()); // same side would grow it
    assert!(e.place_order(uid, ro(Side::Sell, "20"), T0).is_err()); // larger than the position
    go(&mut e, uid, ro(Side::Sell, "4"));
    assert_eq!(pos_size(&e, uid), d("6"));
    assert!(e.set_fut_settings(uid, PERP, MarginMode::Cross, d("20")).is_err());
    assert!(e.place_order(uid, OrderReq { reduce_only: true, ..req(Side::Sell, OType::Market, "0.001", None) }, T0).is_err());
    audit(&e);
}

#[test]
fn leverage_limits_and_insufficient_margin() {
    let (mut e, uid) = fut_setup();
    for bad in ["101", "0", "2.5"] {
        assert!(e.set_fut_settings(uid, PERP, MarginMode::Isolated, d(bad)).is_err(), "{bad}");
    }
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("100")).unwrap();
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("1")).unwrap();
    // 1x: 200 contracts at ~100 needs ~20,000 of margin; the wallet has 10,000
    let err = e.place_order(uid, preq(Side::Buy, OType::Limit, "200", Some("100.0")), T0).unwrap_err();
    assert!(err.contains("insufficient futures margin"), "{err}");
    // a resting limit order reserves margin + fee until it fills or is cancelled
    let o = e.place_order(uid, preq(Side::Buy, OType::Limit, "50", Some("99.0")), T0).unwrap();
    e.activate(o.id, T0);
    assert_eq!(e.fut[&uid].locked, d("4950") + d("2.475"));
    assert_eq!(e.fut_available(uid), d("10000") - d("4952.475"));
    e.cancel_order(uid, o.id, T0).unwrap();
    assert_eq!(e.fut[&uid].locked, Decimal::ZERO);
    audit(&e);
}

#[test]
fn resting_perp_limit_order_fills_from_real_trades_as_maker() {
    let (mut e, uid) = fut_setup();
    let id = go(&mut e, uid, preq(Side::Buy, OType::Limit, "5", Some("100.0")));
    assert_eq!(order(&e, uid, id)["status"], "open");
    e.on_trade(PERP, d("100.0"), d("95"), Side::Sell, T0, true, T0); // 95 of the 100 ahead of us
    assert_eq!(pos_size(&e, uid), Decimal::ZERO);
    e.on_trade(PERP, d("100.0"), d("8"), Side::Sell, T0, true, T0); // the last 5 ahead, then 3 for us
    assert_eq!(pos_size(&e, uid), d("3"));
    e.on_trade(PERP, d("99.9"), d("10"), Side::Sell, T0, true, T0); // trades through our price
    assert_eq!(pos_size(&e, uid), d("5"));
    let o = order(&e, uid, id);
    assert_eq!(o["status"], "filled");
    assert_eq!(d(o["fee"].as_str().unwrap()), d("0.1")); // maker fee: 0.02% of 500
    assert_eq!(e.fut[&uid].locked, Decimal::ZERO);
    audit(&e);
}

#[test]
fn funding_is_paid_by_longs_to_shorts_once_per_period() {
    let (mut e, alice) = fut_setup();
    let bob = new_trader(&mut e, "bob", "10000");
    e.set_fut_settings(bob, PERP, MarginMode::Cross, d("10")).unwrap();
    go(&mut e, alice, preq(Side::Buy, OType::Market, "10", None));
    go(&mut e, bob, preq(Side::Sell, OType::Market, "10", None));
    let bob_wallet = wallet(&e, bob);
    e.on_perp_ticker(PERP, Some(d("100")), None, Some(d("0.001")), Some(T0 + 1000), None, Ticker::default());
    e.fut_tick(T0 + 500); // not due yet
    assert_eq!(e.positions[&(alice, PERP.to_string())].margin, d("100.1"));
    e.fut_tick(T0 + 1000);
    e.fut_tick(T0 + 2000); // the same period is never paid twice
    let a = &e.positions[&(alice, PERP.to_string())];
    assert_eq!((a.margin, a.funding), (d("99.1"), d("-1"))); // isolated: out of the position's margin
    assert_eq!(wallet(&e, bob), bob_wallet + d("1")); // cross: into the wallet
    assert_eq!(e.funding_history(alice, 10)[0]["amount"], "-1");
    audit(&e);
}

#[test]
fn isolated_liquidation_takes_the_whole_margin() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None));
    let before = wallet(&e, uid); // 9899.3995 after margin and fee
    perp_book(&mut e, "91.0", "91.1");
    perp_mark(&mut e, "91"); // equity 9.1 > maintenance 4.55: safe
    e.fut_tick(T0 + 100);
    assert_eq!(pos_size(&e, uid), d("10"));
    perp_book(&mut e, "90.5", "90.6");
    perp_mark(&mut e, "90.5"); // equity 4.1 <= maintenance 4.525
    e.fut_tick(T0 + 200);
    assert!(e.positions.is_empty());
    // what was left of the margin went on the fee and the liquidation fee: back where it started
    assert_eq!(wallet(&e, uid), before);
    assert_eq!(e.position_history(uid, 1)[0]["reason"], "liquidated");
    let r = e.reconcile();
    assert_eq!(r["insurance_fund"]["USD"], "3.6475");
    audit(&e);
}

#[test]
fn cross_liquidation_and_bankruptcy_hit_the_insurance_fund() {
    let (mut e, _) = fut_setup();
    // carol: 300 in futures, 50 long at 20x cross (initial margin 250.25)
    let carol = new_trader(&mut e, "carol", "300");
    e.set_fut_settings(carol, PERP, MarginMode::Cross, d("20")).unwrap();
    go(&mut e, carol, preq(Side::Buy, OType::Market, "50", None));
    assert_eq!(wallet(&e, carol), d("297.4975"));
    let liq = e.liq_price(&e.positions[&(carol, PERP.to_string())]).unwrap();
    assert_eq!(liq.round_dp(4), d("94.6232"));
    perp_book(&mut e, "94.5", "94.6");
    perp_mark(&mut e, "94.5");
    e.fut_tick(T0 + 100);
    assert_eq!(pos_size(&e, carol), Decimal::ZERO);
    assert_eq!(wallet(&e, carol), Decimal::ZERO); // the liquidation fee took what was left
    audit(&e);

    // dave gaps through bankruptcy: his loss is larger than his wallet
    perp_book(&mut e, "100.0", "100.1");
    perp_mark(&mut e, "100");
    let dave = new_trader(&mut e, "dave", "300");
    e.set_fut_settings(dave, PERP, MarginMode::Cross, d("20")).unwrap();
    go(&mut e, dave, preq(Side::Buy, OType::Market, "50", None));
    perp_book(&mut e, "90.0", "90.1");
    perp_mark(&mut e, "90");
    e.fut_tick(T0 + 200);
    assert_eq!(pos_size(&e, dave), Decimal::ZERO);
    assert_eq!(wallet(&e, dave), Decimal::ZERO); // topped up from the insurance fund, never negative
    audit(&e);
}

#[test]
fn new_season_settles_positions_and_empties_futures_wallets() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None));
    let o = e.place_order(uid, preq(Side::Buy, OType::Limit, "1", Some("95.0")), T0).unwrap();
    e.activate(o.id, T0);
    perp_mark(&mut e, "105");
    e.new_season("S2", T0);
    assert!(e.positions.is_empty());
    assert_eq!(wallet(&e, uid), Decimal::ZERO);
    assert_eq!(e.fut[&uid].locked, Decimal::ZERO);
    assert_eq!(bal(&e, uid, "USD"), (d("100000"), Decimal::ZERO));
    audit(&e);
}

#[test]
fn kraken_futures_messages_drive_the_engine() {
    let (mut e, _) = setup();
    let instruments = r#"{"result":"success","instruments":[{"symbol":"PF_XBTUSD","tickSize":1,"contractValueTradePrecision":4,
      "tradeable":true,"suspended":false,"maxPositionSize":1200.0,
      "marginLevels":[{"numNonContractUnits":0.0,"initialMargin":0.02,"maintenanceMargin":0.01}],
      "retailMarginLevels":[{"numNonContractUnits":0.0,"initialMargin":0.01,"maintenanceMargin":0.005},
                            {"numNonContractUnits":1000000.0,"initialMargin":0.02,"maintenanceMargin":0.01}]}]}"#;
    assert_eq!(apply_instruments(&mut e, &serde_json::from_str(instruments).unwrap()), 1);
    let i = &e.instruments[PERP];
    assert_eq!((i.price_increment, i.qty_increment, i.kind), (d("1"), d("0.0001"), "perp"));
    assert_eq!(e.perps[PERP].max_leverage(), d("100"));

    let mut seqs = std::collections::HashMap::new();
    e.fut_feed_up(T0);
    let snap = r#"{"feed":"book_snapshot","product_id":"PF_XBTUSD","seq":10,"bids":[{"price":81505.0,"qty":0.5}],
      "asks":[{"price":81506.0,"qty":0.3}]}"#;
    handle_fut_message(&mut e, snap, T0, &mut seqs).unwrap();
    let upd = r#"{"feed":"book","product_id":"PF_XBTUSD","side":"buy","seq":11,"price":81505.5,"qty":1.0}"#;
    handle_fut_message(&mut e, upd, T0, &mut seqs).unwrap();
    assert_eq!(e.books[PERP].best(Side::Buy), Some((d("81505.5"), d("1"))));
    let tick = r#"{"feed":"ticker","product_id":"PF_XBTUSD","markPrice":81505.1477,"index":81495.11,
      "relative_funding_rate":2.42436625e-05,"next_funding_rate_time":1792184400000,"last":81507.0,"change":-0.98}"#;
    handle_fut_message(&mut e, tick, T0, &mut seqs).unwrap();
    let p = &e.perps[PERP];
    assert_eq!((p.mark, p.funding_rate, p.next_funding_ms), (Some(d("81505.1477")), Some(d("0.0000242436625")), Some(1792184400000)));
    let trade = r#"{"feed":"trade","product_id":"PF_XBTUSD","side":"sell","type":"fill","time":1792182432209,"qty":0.0001,"price":81505.0}"#;
    handle_fut_message(&mut e, trade, T0, &mut seqs).unwrap();
    assert_eq!(e.tickers[PERP].last, Some(d("81505")));
    assert!(e.tradable(PERP, T0));
    // a skipped sequence number means a lost update: resync
    let gap = r#"{"feed":"book","product_id":"PF_XBTUSD","side":"sell","seq":13,"price":81507.0,"qty":1.0}"#;
    assert!(handle_fut_message(&mut e, gap, T0, &mut seqs).is_err());
}
