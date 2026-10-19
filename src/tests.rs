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
        margin_rollover_secs: 14400,
        options: vec!["BTC".into()],
        opt_spread: d("0.03"),
        opt_fee: d("0.0003"),
        opt_fee_cap: d("0.125"),
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
        leverage: None,
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
    e.set_fut_settings(uid, PERP, MarginMode::Cross, d("5"), T0).unwrap();
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
    assert!(e.set_fut_settings(uid, PERP, MarginMode::Cross, d("20"), T0).is_err());
    assert!(e.place_order(uid, OrderReq { reduce_only: true, ..req(Side::Sell, OType::Market, "0.001", None) }, T0).is_err());
    audit(&e);
}

#[test]
fn leverage_limits_and_insufficient_margin() {
    let (mut e, uid) = fut_setup();
    for bad in ["101", "0", "2.5"] {
        assert!(e.set_fut_settings(uid, PERP, MarginMode::Isolated, d(bad), T0).is_err(), "{bad}");
    }
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("100"), T0).unwrap();
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("1"), T0).unwrap();
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
    e.set_fut_settings(bob, PERP, MarginMode::Cross, d("10"), T0).unwrap();
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
    e.set_fut_settings(carol, PERP, MarginMode::Cross, d("20"), T0).unwrap();
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
    e.set_fut_settings(dave, PERP, MarginMode::Cross, d("20"), T0).unwrap();
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

#[test]
fn funding_is_not_missed_when_the_ticker_rolls_over_first() {
    let (mut e, alice) = fut_setup();
    go(&mut e, alice, preq(Side::Buy, OType::Market, "10", None));
    e.on_perp_ticker(PERP, Some(d("100")), None, Some(d("0.001")), Some(T0 + 1000), None, Ticker::default());
    // just after the hour Kraken already shows the next period (new rate, next time an hour on)
    e.on_perp_ticker(PERP, Some(d("100")), None, Some(d("0.005")), Some(T0 + 1000 + 3_600_000), None, Ticker::default());
    e.fut_tick(T0 + 900); // our clock may lag the exchange's a little
    let a = &e.positions[&(alice, PERP.to_string())];
    assert_eq!(a.funding, d("-1")); // charged at the 0.001 that applied to the period that ended
    e.fut_tick(T0 + 1300);
    assert_eq!(e.positions[&(alice, PERP.to_string())].funding, d("-1"));
    audit(&e);
}

#[test]
fn partial_close_loss_comes_from_the_rest_of_the_margin_first() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None)); // 10 @ 100.1, margin 100.1 at 10x
    // a gap far below the entry, before any liquidation check ran: close half at 85
    perp_book(&mut e, "85.0", "85.1");
    go(&mut e, uid, OrderReq { reduce_only: true, ..preq(Side::Sell, OType::Market, "5", None) });
    // loss 5 * 15.1 = 75.5; 50.05 of margin released, the other 25.45 taken from the remaining margin
    let p = &e.positions[&(uid, PERP.to_string())];
    assert_eq!((p.size, p.margin), (d("5"), d("100.1") - d("50.05") - d("25.45")));
    assert!(e.reconcile()["insurance_fund"].get("USD").is_none()); // the insurance fund was not touched
    // a student's own order with the note "liquidation" is still an ordinary close
    go(&mut e, uid, OrderReq { reduce_only: true, note: Some("liquidation".into()), ..preq(Side::Sell, OType::Market, "5", None) });
    assert_eq!(e.position_history(uid, 1)[0]["reason"], "closed");
    audit(&e);
}

#[test]
fn isolated_leverage_change_tops_up_or_releases_within_the_margin_floor() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None)); // 10 @ 100.1 at 10x: margin 100.1
    let margin = |e: &Engine| e.positions[&(uid, PERP.to_string())].margin;
    let w0 = wallet(&e, uid);
    // 20x: entry value / 20 would be 50.05, but "remove margin" at 20x keeps initial margin at the
    // mark (50) plus the unrealised loss (1), so 51 stays and 49.1 comes back
    let plan = e.plan_leverage(uid, PERP, d("20"), T0).unwrap();
    assert_eq!((plan.margin_before, plan.margin_after), (d("100.1"), d("51")));
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("20"), T0).unwrap();
    assert_eq!((margin(&e), wallet(&e, uid)), (d("51"), w0 + d("49.1")));
    let p = e.positions[&(uid, PERP.to_string())].clone();
    assert_eq!(p.leverage, d("20"));
    assert_eq!(e.liq_price(&p), plan.liq_after); // the preview is what happens
    // 5x: topped up to entry value / 5
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("5"), T0).unwrap();
    assert_eq!((margin(&e), wallet(&e, uid)), (d("200.2"), w0 - d("100.1")));
    audit(&e);

    // 1x would need 800.8 more than the wallet can spare
    let avail = e.fut_available(uid);
    e.fut_transfer(uid, avail - d("100"), false, T0).unwrap();
    let err = e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("1"), T0).unwrap_err();
    assert!(err.contains("needs 800.80 more"), "{err}");
    assert_eq!(margin(&e), d("200.2"));
    // open orders don't block a change: they are re-reserved at the new leverage
    let o = e.place_order(uid, preq(Side::Buy, OType::Limit, "0.5", Some("95.0")), T0).unwrap();
    e.activate(o.id, T0);
    assert_eq!(d(order(&e, uid, o.id)["locked"].as_str().unwrap()), d("9.52375")); // 0.5 * 95 * (1/5 + 0.0005)
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("10"), T0).unwrap();
    assert_eq!(d(order(&e, uid, o.id)["locked"].as_str().unwrap()), d("4.77375")); // at 10x
    assert_eq!(e.fut[&uid].locked, d("4.77375"));
    assert!(e.set_fut_settings(uid, PERP, MarginMode::Cross, d("10"), T0).is_err()); // no mode switch with a position
    // after a loss nothing is above the floor, so raising leverage releases nothing (and is not refused)
    perp_book(&mut e, "92.0", "92.1");
    perp_mark(&mut e, "92");
    let before = margin(&e);
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("20"), T0).unwrap();
    assert_eq!(margin(&e), before);
    audit(&e);
}

#[test]
fn lowering_leverage_keeps_margin_added_by_hand() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None));
    e.adjust_margin(uid, PERP, d("100"), T0).unwrap(); // 200.1
    let liq = e.liq_price(&e.positions[&(uid, PERP.to_string())]).unwrap();
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("9"), T0).unwrap();
    let p = e.positions[&(uid, PERP.to_string())].clone();
    assert_eq!((p.margin, p.leverage), (d("200.1"), d("9")));
    assert_eq!(e.liq_price(&p).unwrap(), liq);
    audit(&e);
}

#[test]
fn raising_leverage_cannot_bypass_the_remove_margin_floor() {
    let (mut e, uid) = fut_setup();
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("5"), T0).unwrap();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None)); // margin 200.2
    perp_book(&mut e, "92.0", "92.1");
    perp_mark(&mut e, "92"); // unrealised loss 81
    assert_eq!(e.margin_removable(&e.positions[&(uid, PERP.to_string())]), Decimal::ZERO);
    let w = wallet(&e, uid);
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("10"), T0).unwrap();
    // at 10x the floor is 92 + 81 = 173: 27.2 may leave, not 100.1
    assert_eq!(wallet(&e, uid) - w, d("27.2"));
    assert_eq!(e.margin_removable(&e.positions[&(uid, PERP.to_string())]), Decimal::ZERO);
    audit(&e);
}

#[test]
fn margin_changes_wait_for_live_market_data() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None));
    e.adjust_margin(uid, PERP, d("100"), T0).unwrap();
    e.fut_feed_down();
    let later = T0 + 60_000;
    assert!(e.adjust_margin(uid, PERP, d("-50"), later).is_err());
    assert!(e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("50"), later).is_err());
    assert!(e.fut_transfer(uid, d("100"), false, later).is_err()); // positions can't be valued
    assert_eq!(e.positions[&(uid, PERP.to_string())].margin, d("200.1"));
    audit(&e);
}

#[test]
fn closing_needs_no_free_margin_after_a_max_top_up() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None));
    let max = floor_to(e.fut_available(uid), d("0.01"));
    e.adjust_margin(uid, PERP, max, T0).unwrap();
    perp_book(&mut e, "99.0", "99.1");
    perp_mark(&mut e, "99");
    go(&mut e, uid, OrderReq { reduce_only: true, ..preq(Side::Sell, OType::Market, "10", None) });
    assert!(e.positions.is_empty());
    audit(&e);
}

#[test]
fn a_stale_closing_order_cannot_open_an_unbacked_position() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None));
    // a take-profit that only closes, without the reduce-only flag: it reserves nothing
    let tp = e.place_order(uid, preq(Side::Sell, OType::Limit, "10", Some("110.0")), T0).unwrap();
    e.activate(tp.id, T0);
    assert_eq!(e.fut[&uid].locked, Decimal::ZERO);
    // the leverage can change with it resting (its need is re-computed, still zero)
    e.set_fut_settings(uid, PERP, MarginMode::Isolated, d("20"), T0).unwrap();
    // the long is closed another way and every free dollar moved out
    go(&mut e, uid, OrderReq { reduce_only: true, ..preq(Side::Sell, OType::Market, "10", None) });
    let avail = e.fut_available(uid);
    e.fut_transfer(uid, avail, false, T0).unwrap();
    // the market trades up through 110: with no margin behind it the order is cancelled, not filled
    e.on_trade(PERP, d("110.0"), d("30"), Side::Buy, T0, true, T0);
    assert!(e.positions.is_empty());
    assert_eq!(order(&e, uid, tp.id)["status"], "canceled");
    assert!(wallet(&e, uid) >= Decimal::ZERO);
    audit(&e);
}

#[test]
fn cross_leverage_change_changes_free_margin() {
    let (mut e, uid) = fut_setup();
    e.set_fut_settings(uid, PERP, MarginMode::Cross, d("10"), T0).unwrap();
    go(&mut e, uid, preq(Side::Sell, OType::Market, "10", None)); // short 10 @ 100.0
    assert_eq!(e.fut_available(uid), d("9999.5") - d("100"));
    e.set_fut_settings(uid, PERP, MarginMode::Cross, d("50"), T0).unwrap();
    assert_eq!(e.fut_available(uid), d("9999.5") - d("20"));
    e.fut_transfer(uid, d("9900"), false, T0).unwrap(); // 99.5 left in the wallet
    for lev in ["2", "5", "10"] {
        assert!(e.set_fut_settings(uid, PERP, MarginMode::Cross, d(lev), T0).is_err(), "{lev}x");
    }
    assert_eq!(e.positions[&(uid, PERP.to_string())].leverage, d("50")); // a refused change leaves it alone
    // raising is always allowed, even when the account is already short of initial margin
    perp_book(&mut e, "105.0", "105.1");
    perp_mark(&mut e, "105"); // short loses 50: free margin 99.5 - 21 - 50 < 0 at 50x
    e.set_fut_settings(uid, PERP, MarginMode::Cross, d("100"), T0).unwrap();
    perp_book(&mut e, "100.0", "100.1");
    perp_mark(&mut e, "100");
    e.set_fut_settings(uid, PERP, MarginMode::Cross, d("50"), T0).unwrap(); // back down: 20 tied up, affordable
    e.set_fut_settings(uid, PERP, MarginMode::Cross, d("12"), T0).unwrap();
    assert_eq!(e.fut_available(uid).round_dp(2), d("16.17")); // 99.5 - 1000/12
    assert_eq!(e.plan_leverage(uid, PERP, d("12"), T0).unwrap().margin_before, d("83.33333333"));
    audit(&e);
}

#[test]
fn adding_margin_moves_the_liquidation_price_away() {
    let (mut e, uid) = fut_setup();
    go(&mut e, uid, preq(Side::Buy, OType::Market, "10", None)); // liquidation at about 90.54
    let w0 = wallet(&e, uid);
    e.adjust_margin(uid, PERP, d("100"), T0).unwrap();
    let p = e.positions[&(uid, PERP.to_string())].clone();
    assert_eq!((p.margin, wallet(&e, uid)), (d("200.1"), w0 - d("100")));
    assert_eq!(e.liq_price(&p).unwrap().round_dp(4), d("80.4925"));
    // 90.5 would have liquidated it at 10x; with the extra margin it survives
    perp_book(&mut e, "90.5", "90.6");
    perp_mark(&mut e, "90.5");
    e.fut_tick(T0 + 100);
    assert_eq!(pos_size(&e, uid), d("10"));
    perp_book(&mut e, "100.0", "100.1");
    perp_mark(&mut e, "100");
    // removable: 200.1 margin - 1 unrealised loss - 100 initial margin at 10x
    assert_eq!(e.margin_removable(&e.positions[&(uid, PERP.to_string())]), d("99.1"));
    // a negative margin (funding has no floor) never shows a negative removable amount
    let mut drained = e.positions[&(uid, PERP.to_string())].clone();
    drained.margin = d("-0.5");
    assert_eq!(e.margin_removable(&drained), Decimal::ZERO);
    assert!(e.adjust_margin(uid, PERP, d("-99.2"), T0).is_err());
    e.adjust_margin(uid, PERP, d("-99.1"), T0).unwrap();
    assert_eq!(e.positions[&(uid, PERP.to_string())].margin, d("101"));
    assert!(e.adjust_margin(uid, PERP, d("100000"), T0).is_err()); // more than the wallet holds
    assert!(e.adjust_margin(uid, "ETH-PERP", d("10"), T0).is_err()); // no such position
    let bob = new_trader(&mut e, "bob", "1000");
    e.set_fut_settings(bob, PERP, MarginMode::Cross, d("10"), T0).unwrap();
    go(&mut e, bob, preq(Side::Buy, OType::Market, "1", None));
    assert!(e.adjust_margin(bob, PERP, d("10"), T0).is_err()); // cross has no margin of its own
    audit(&e);
}

// ------------------------------------------------------------------ spot margin

fn mreq(side: Side, otype: OType, qty: &str, limit: Option<&str>, lev: u32) -> OrderReq {
    OrderReq { leverage: Some(Decimal::from(lev)), ..req(side, otype, qty, limit) }
}

fn mpos_size(e: &Engine, uid: i64) -> Decimal {
    e.mpos.get(&(uid, "BTC/USD".to_string())).map(|p| p.size).unwrap_or_default()
}

fn btc_book(e: &mut Engine, bid: &str, ask: &str) {
    e.on_book_snapshot("BTC/USD", &lv(&[(bid, "5")]), &lv(&[(ask, "5")]), T0);
}

#[test]
fn margin_short_opens_without_coins_and_closes_with_profit() {
    let (mut e, u) = setup();
    go(&mut e, u, mreq(Side::Sell, OType::Market, "1", None, 5));
    // sold 1 BTC we never had at the 99.9 bid: fee 0.4%, opening fee 0.01% (BTC's rate), margin 99.9 / 5
    assert_eq!(mpos_size(&e, u), d("-1"));
    let p = e.mpos[&(u, "BTC/USD".to_string())].clone();
    assert_eq!((p.entry, p.margin), (d("99.9"), d("19.98")));
    assert_eq!(bal(&e, u, "BTC").0, Decimal::ZERO);
    assert_eq!(bal(&e, u, "USD"), (d("100000") - d("0.3996") - d("0.00999"), Decimal::ZERO));
    // margin and the unrealised loss at the mark are held back from spot
    let (upnl, used, _) = e.margin_state(u);
    assert_eq!(used, d("19.98"));
    assert_eq!(e.free(u, "USD"), bal(&e, u, "USD").0 - used + upnl);
    audit(&e);

    btc_book(&mut e, "89.9", "90.0");
    go(&mut e, u, OrderReq { reduce_only: true, ..mreq(Side::Buy, OType::Market, "1", None, 5) });
    assert!(e.mpos.is_empty());
    // +9.9 on the short, less the 0.36 fee to buy back
    assert_eq!(bal(&e, u, "USD").0, d("100000") - d("0.3996") - d("0.00999") + d("9.9") - d("0.36"));
    assert_eq!(e.free(u, "USD"), bal(&e, u, "USD").0);
    assert_eq!(e.margin_history(u, 10)[0]["realized"], "9.9");
    audit(&e);
}

#[test]
fn margin_orders_reserve_usd_and_are_validated() {
    let (mut e, u) = setup();
    assert!(e.place_order(u, mreq(Side::Sell, OType::Market, "1", None, 11), T0).unwrap_err().contains("at most 10x"));
    assert!(e.place_order(u, OrderReq { leverage: Some(d("2.5")), ..req(Side::Sell, OType::Market, "1", None) }, T0).is_err());
    assert!(e.place_order(u, OrderReq { reduce_only: true, ..mreq(Side::Buy, OType::Market, "1", None, 2) }, T0).is_err());
    assert!(e.place_order(u, OrderReq { symbol: "BTC-PERP".into(), ..mreq(Side::Buy, OType::Market, "1", None, 2) }, T0).is_err());
    // a resting margin sell reserves USD (margin + fees), not BTC
    let id = go(&mut e, u, mreq(Side::Sell, OType::Limit, "1", Some("105"), 2));
    assert_eq!(bal(&e, u, "USD").1, d("52.9305"));
    assert_eq!(bal(&e, u, "BTC").1, Decimal::ZERO);
    audit(&e);
    e.cancel_order(u, id, T0).unwrap();
    assert_eq!(bal(&e, u, "USD").1, Decimal::ZERO);
    // not enough USD for the margin
    e.fut_transfer(u, d("99990"), true, T0).unwrap();
    assert!(e.place_order(u, mreq(Side::Sell, OType::Market, "1", None, 2), T0).unwrap_err().contains("insufficient margin"));
    audit(&e);
}

#[test]
fn margin_held_back_cannot_be_spent_or_moved() {
    let (mut e, u) = setup();
    e.fut_transfer(u, d("99900"), true, T0).unwrap(); // 100 USD left in spot
    go(&mut e, u, mreq(Side::Sell, OType::Market, "1", None, 2)); // margin 49.95
    let free = e.free(u, "USD");
    assert!(free < d("51") && free > d("49"), "free {free}");
    assert!(e.fut_transfer(u, d("60"), true, T0).is_err());
    assert!(e.place_order(u, req(Side::Buy, OType::Limit, "0.6", Some("99")), T0).unwrap_err().contains("insufficient USD"));
    audit(&e);
}

#[test]
fn resting_margin_short_fills_as_maker_from_real_trades() {
    let (mut e, u) = setup();
    go(&mut e, u, mreq(Side::Sell, OType::Limit, "1", Some("100.5"), 3));
    e.on_trade("BTC/USD", d("100.6"), d("2"), Side::Buy, T0, true, T0);
    let p = e.mpos[&(u, "BTC/USD".to_string())].clone();
    assert_eq!((p.size, p.entry, p.margin), (d("-1"), d("100.5"), d("33.5")));
    assert_eq!(bal(&e, u, "USD").1, Decimal::ZERO);
    audit(&e);
}

#[test]
fn margin_rollover_is_charged_every_period() {
    let (mut e, u) = setup();
    go(&mut e, u, mreq(Side::Buy, OType::Market, "1", None, 4)); // long borrows USD: 0.025%
    let before = bal(&e, u, "USD").0;
    let t = T0 + 4 * 3600 * 1000 + 1;
    e.feed_up(t);
    e.margin_tick(t);
    let mark = e.mark("BTC/USD").unwrap();
    assert_eq!(bal(&e, u, "USD").0, before - round8(mark * d("0.00025")));
    e.margin_tick(t + 1000); // not again until the next period
    assert_eq!(bal(&e, u, "USD").0, before - round8(mark * d("0.00025")));
    audit(&e);
}

#[test]
fn margin_stop_liquidates_and_insurance_covers_the_shortfall() {
    let (mut e, u) = setup();
    e.fut_transfer(u, d("99970"), true, T0).unwrap(); // 30 USD in spot
    go(&mut e, u, mreq(Side::Sell, OType::Market, "1", None, 5));
    let liq = e.margin_liq_price(&e.mpos[&(u, "BTC/USD".to_string())].clone()).unwrap();
    assert!(liq > d("121") && liq < d("122"), "liq {liq}");
    btc_book(&mut e, "115.0", "115.1");
    e.margin_tick(T0);
    assert_eq!(mpos_size(&e, u), d("-1")); // above the stop: still open
    btc_book(&mut e, "129.9", "130.0");
    e.margin_tick(T0);
    assert!(e.mpos.is_empty());
    assert_eq!(bal(&e, u, "USD"), (Decimal::ZERO, Decimal::ZERO));
    assert_eq!(e.margin_history(u, 10)[0]["close_reason"], "liquidated");
    let r = e.reconcile();
    assert!(r["insurance_fund"]["USD"].as_str().unwrap().starts_with('-'));
    audit(&e);
}

#[test]
fn new_season_settles_margin_positions() {
    let (mut e, u) = setup();
    go(&mut e, u, mreq(Side::Sell, OType::Market, "1", None, 2));
    e.new_season("S2", T0);
    assert!(e.mpos.is_empty());
    assert_eq!(bal(&e, u, "USD").0, d("100000"));
    audit(&e);
}

#[test]
fn kraken_margin_rules_are_parsed() {
    let (mut e, _) = setup();
    let pairs: serde_json::Value = serde_json::from_str(
        r#"{"XXBTZUSD":{"wsname":"XBT/USD","leverage_buy":[2,3,4,5],"leverage_sell":[2,3],"margin_call":80,"margin_stop":40}}"#,
    )
    .unwrap();
    let assets: serde_json::Value =
        serde_json::from_str(r#"{"XXBT":{"altname":"XBT","margin_rate":"0.01"},"ZUSD":{"altname":"USD","margin_rate":"0.025"}}"#).unwrap();
    assert_eq!(crate::feed::apply_margin_rules(&mut e, &pairs, &assets), 1);
    let r = e.margin_rule("BTC/USD");
    assert_eq!((r.max_long, r.max_short), (d("5"), d("3")));
    assert_eq!((r.long_rate, r.short_rate, r.stop_level), (d("0.00025"), d("0.0001"), d("0.4")));
    let (mut e2, u) = setup();
    crate::feed::apply_margin_rules(&mut e2, &pairs, &assets);
    assert!(e2.place_order(u, mreq(Side::Sell, OType::Market, "1", None, 4), T0).unwrap_err().contains("at most 3x"));
}

// ------------------------------------------------------------------ options

use crate::options::{black_scholes, date_code, listed_expiries, OptSpec};

#[test]
fn option_calendar_and_symbols() {
    // T0 is Friday 2027-01-15 08:00 UTC, an expiry itself, so it is no longer listed
    let codes: Vec<String> = listed_expiries(T0).into_iter().map(date_code).collect();
    assert_eq!(codes, ["16JAN27", "17JAN27", "18JAN27", "22JAN27", "29JAN27", "5FEB27", "12FEB27", "26FEB27", "26MAR27"]);
    let s = OptSpec::parse("BTC-29JAN27-80000-C").unwrap();
    assert_eq!((s.strike, s.call, s.expiry_ms), (d("80000"), true, T0 + 14 * 86_400_000));
    assert_eq!(s.symbol(), "BTC-29JAN27-80000-C");
    assert!(OptSpec::parse("BTC-30FEB27-80000-C").is_none());
    assert!(OptSpec::parse("BTC-29JAN27-80000-X").is_none());
}

#[test]
fn black_scholes_values() {
    let (c, delta) = black_scholes(true, 100.0, 100.0, 1.0, 0.5);
    assert!((c - 19.7413).abs() < 1e-3, "{c}");
    assert!((delta - 0.5987).abs() < 1e-3, "{delta}");
    let (p, _) = black_scholes(false, 100.0, 100.0, 1.0, 0.5);
    assert!((c - p).abs() < 1e-9); // put-call parity at the money with no rates
    assert_eq!(black_scholes(false, 90.0, 100.0, 0.0, 0.5).0, 10.0);
}

#[test]
fn options_buy_sell_rest_and_settle_in_cash() {
    let (mut e, u) = setup();
    e.opt_vols.insert("BTC".into(), 0.5);
    let expiry = listed_expiries(T0)[0];
    let put = OptSpec { underlying: "BTC".into(), expiry_ms: expiry, strike: d("1000"), call: false }.symbol();
    let oreq = |side, otype, qty: &str, limit: Option<&str>| OrderReq { symbol: put.clone(), ..req(side, otype, qty, limit) };

    // buy 1 deep in-the-money put at our ask: theo (~900.05) + 3%, plus the fee (0.03% of spot)
    let usd0 = bal(&e, u, "USD").0;
    let id = go(&mut e, u, oreq(Side::Buy, OType::Market, "1", None));
    let o = order(&e, u, id);
    assert_eq!(o["status"], "filled");
    let paid = d(o["avg_price"].as_str().unwrap());
    assert!(paid > d("925") && paid < d("929"), "ask {paid}");
    assert_eq!(bal(&e, u, &put).0, d("1"));
    assert_eq!(bal(&e, u, "USD").0, usd0 - paid - round8(d("99.95") * d("0.0003")));
    audit(&e);

    // can't write options: only what is held can be sold
    assert!(e.place_order(u, oreq(Side::Sell, OType::Market, "2", None), T0).unwrap_err().contains("only sell options you hold"));
    go(&mut e, u, oreq(Side::Sell, OType::Market, "0.5", None));
    assert_eq!(bal(&e, u, &put).0, d("0.5"));
    // a buy below the ask rests and reserves USD until cancelled
    let id = go(&mut e, u, oreq(Side::Buy, OType::Limit, "1", Some("800")));
    assert_eq!(order(&e, u, id)["status"], "open");
    assert_eq!(bal(&e, u, "USD").1, d("900"));
    e.opt_tick(T0);
    assert_eq!(order(&e, u, id)["status"], "open");
    e.cancel_order(u, id, T0).unwrap();
    assert_eq!(bal(&e, u, "USD").1, Decimal::ZERO);
    audit(&e);

    // expiry: cash at the average spot of the last 30 minutes
    let usd1 = bal(&e, u, "USD").0;
    e.feed_up(expiry - 600_000);
    e.opt_tick(expiry - 600_000);
    e.feed_up(expiry + 1000);
    e.opt_tick(expiry + 1000);
    assert_eq!(bal(&e, u, &put).0, Decimal::ZERO);
    assert_eq!(bal(&e, u, "USD").0, usd1 + d("0.5") * (d("1000") - d("99.95")));
    assert!(e.place_order(u, oreq(Side::Buy, OType::Market, "1", None), expiry + 2000).unwrap_err().contains("expired"));
    audit(&e);
}

#[test]
fn options_are_valued_in_equity_and_cashed_in_at_a_new_season() {
    let (mut e, u) = setup();
    e.opt_vols.insert("BTC".into(), 0.5);
    let expiry = listed_expiries(T0)[3];
    let put = OptSpec { underlying: "BTC".into(), expiry_ms: expiry, strike: d("1000"), call: false }.symbol();
    go(&mut e, u, OrderReq { symbol: put.clone(), ..req(Side::Buy, OType::Market, "2", None) });
    let theo = e.opt_value(&put).unwrap();
    let eq = e.equity(u);
    assert_eq!(eq, bal(&e, u, "USD").0 + d("2") * theo);
    e.new_season("S2", T0);
    assert_eq!(bal(&e, u, &put).0, Decimal::ZERO);
    assert_eq!(bal(&e, u, "USD").0, d("100000"));
    audit(&e);
}
