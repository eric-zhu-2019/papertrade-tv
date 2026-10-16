//! Market data: the live Kraken WebSocket v2 feed, plus a synthetic feed for
//! offline development (FEED=sim).

use crate::book::{Level, Side};
use crate::candles::{Candle, INTERVALS};
use crate::config::Config;
use crate::engine::{Engine, Instrument, Shared, Ticker};
use crate::util::*;
use futures_util::{SinkExt, StreamExt};
use rand::{rngs::StdRng, Rng, SeedableRng};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

const KRAKEN_WS: &str = "wss://ws.kraken.com/v2";
const KRAKEN_REST: &str = "https://api.kraken.com";

fn levels(v: &Value) -> Vec<Level> {
    v.as_array()
        .map(|a| a.iter().filter_map(|l| Some((dec(&l["price"])?, dec(&l["qty"])?))).collect())
        .unwrap_or_default()
}

/// Apply one Kraken WS v2 message. Returns Err when the local book has
/// diverged from the exchange (checksum mismatch) and must be resynced.
pub fn handle_message(eng: &mut Engine, text: &str, now: i64, verify: bool) -> Result<(), String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad json: {e}"))?;
    if v.get("method").is_some() {
        if v["success"] == json!(false) {
            tracing::error!("kraken rejected a request: {}", v["error"]);
        }
        return Ok(());
    }
    let snapshot = v["type"] == "snapshot";
    match v["channel"].as_str().unwrap_or("") {
        "instrument" => {
            for p in v["data"]["pairs"].as_array().into_iter().flatten() {
                let Some(symbol) = p["symbol"].as_str() else { continue };
                if !eng.cfg.symbols.iter().any(|s| s == symbol) {
                    continue;
                }
                let g = |k: &str| dec(&p[k]);
                let (Some(pi), Some(qi)) = (g("price_increment"), g("qty_increment")) else { continue };
                eng.set_instrument(Instrument {
                    symbol: symbol.to_string(),
                    base: p["base"].as_str().unwrap_or("").to_string(),
                    quote: p["quote"].as_str().unwrap_or("").to_string(),
                    price_increment: pi,
                    qty_increment: qi,
                    qty_min: g("qty_min").unwrap_or(qi),
                    cost_min: g("cost_min").unwrap_or(Decimal::ZERO),
                    price_precision: p["price_precision"].as_u64().unwrap_or(pi.normalize().scale() as u64) as u32,
                    qty_precision: p["qty_precision"].as_u64().unwrap_or(qi.normalize().scale() as u64) as u32,
                    status: p["status"].as_str().unwrap_or("online").to_string(),
                });
            }
        }
        "book" => {
            for d in v["data"].as_array().into_iter().flatten() {
                let Some(symbol) = d["symbol"].as_str() else { continue };
                let (bids, asks) = (levels(&d["bids"]), levels(&d["asks"]));
                if snapshot {
                    eng.on_book_snapshot(symbol, &bids, &asks, now);
                } else {
                    eng.on_book_update(symbol, &bids, &asks, now);
                }
                if verify {
                    if let (Some(expect), Some(inst), Some(book)) =
                        (d["checksum"].as_u64(), eng.instruments.get(symbol), eng.books.get(symbol))
                    {
                        if book.ready && book.checksum(inst.price_precision, inst.qty_precision) as u64 != expect {
                            return Err(format!("book checksum mismatch for {symbol}"));
                        }
                    }
                }
            }
        }
        "trade" => {
            for t in v["data"].as_array().into_iter().flatten() {
                let (Some(symbol), Some(price), Some(qty)) = (t["symbol"].as_str(), dec(&t["price"]), dec(&t["qty"]))
                else {
                    continue;
                };
                let Some(side) = t["side"].as_str().and_then(Side::parse) else { continue };
                let ts = t["timestamp"].as_str().and_then(parse_rfc3339_ms).unwrap_or(now);
                eng.on_trade(symbol, price, qty, side, ts, !snapshot, now);
            }
        }
        "ticker" => {
            for t in v["data"].as_array().into_iter().flatten() {
                let Some(symbol) = t["symbol"].as_str() else { continue };
                eng.on_ticker(
                    symbol,
                    Ticker {
                        last: dec(&t["last"]),
                        high: dec(&t["high"]),
                        low: dec(&t["low"]),
                        volume: dec(&t["volume"]),
                        change_pct: dec(&t["change_pct"]),
                    },
                );
            }
        }
        _ => {} // heartbeat, status
    }
    Ok(())
}

async fn kraken_session(engine: &Shared, cfg: &Config, verify: bool) -> Result<(), String> {
    let ws_url = std::env::var("KRAKEN_WS_URL").unwrap_or_else(|_| KRAKEN_WS.to_string());
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(ws_url.as_str()))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;
    let (mut w, mut r) = ws.split();
    let depth = [10usize, 25, 100, 500, 1000].into_iter().find(|d| *d >= cfg.book_depth).unwrap_or(1000);
    let subs = [
        json!({"method": "subscribe", "params": {"channel": "instrument"}}),
        json!({"method": "subscribe", "params": {"channel": "ticker", "symbol": cfg.symbols}}),
        json!({"method": "subscribe", "params": {"channel": "book", "symbol": cfg.symbols, "depth": depth}}),
        json!({"method": "subscribe", "params": {"channel": "trade", "symbol": cfg.symbols, "snapshot": true}}),
    ];
    for s in subs {
        w.send(Message::Text(s.to_string())).await.map_err(|e| format!("send: {e}"))?;
    }
    tracing::info!("kraken feed connected, {} symbols, depth {depth}", cfg.symbols.len());
    loop {
        // Kraken sends a heartbeat every second on a subscribed connection.
        let msg = tokio::time::timeout(Duration::from_secs(10), r.next()).await.map_err(|_| "feed went silent".to_string())?;
        match msg {
            Some(Ok(Message::Text(t))) => {
                let now = now_ms();
                let mut e = engine.lock().unwrap();
                e.feed_up(now);
                handle_message(&mut e, &t, now, verify)?;
            }
            Some(Ok(Message::Ping(p))) => {
                w.send(Message::Pong(p)).await.map_err(|e| format!("pong: {e}"))?;
            }
            Some(Ok(Message::Close(_))) | None => return Err("connection closed by exchange".into()),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("read: {e}")),
        }
    }
}

/// Keep the Kraken feed alive forever. While it is down the engine refuses new
/// orders and does not match resting ones.
pub async fn run_kraken(engine: Shared, cfg: Arc<Config>) {
    let mut verify = cfg.checksum != "off";
    let mut quick_checksum_failures = 0;
    let mut backoff = 1u64;
    loop {
        let started = Instant::now();
        let err = kraken_session(&engine, &cfg, verify).await.unwrap_err();
        {
            let mut e = engine.lock().unwrap();
            e.feed_down();
            e.candles.invalidate();
        }
        tracing::warn!("kraken feed down: {err}; trading paused, reconnecting in {backoff}s");
        if err.contains("checksum") && started.elapsed() < Duration::from_secs(60) {
            quick_checksum_failures += 1;
            if quick_checksum_failures >= 5 && cfg.checksum != "enforce-strict" {
                tracing::error!("repeated book checksum failures; continuing WITHOUT checksum verification");
                verify = false;
            }
        } else {
            quick_checksum_failures = 0;
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

/// Historical candles from Kraken's public REST API.
pub async fn fetch_ohlc(http: &reqwest::Client, symbol: &str, interval: u32) -> Result<Vec<Candle>, String> {
    let v: Value = http
        .get(format!("{KRAKEN_REST}/0/public/OHLC"))
        .query(&[("pair", symbol), ("interval", &interval.to_string())])
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(e) = v["error"].as_array().filter(|a| !a.is_empty()) {
        return Err(format!("kraken: {e:?}"));
    }
    let rows = v["result"]
        .as_object()
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() != "last"))
        .and_then(|(_, a)| a.as_array())
        .ok_or("unexpected OHLC response")?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(Candle {
                t: r[0].as_i64()?,
                o: dec(&r[1])?,
                h: dec(&r[2])?,
                l: dec(&r[3])?,
                c: dec(&r[4])?,
                v: dec(&r[6])?,
            })
        })
        .collect())
}

// ------------------------------------------------------------------ synthetic feed

/// (price precision, minimum quantity, starting price) for the synthetic feed.
fn sim_spec(base: &str) -> (u32, &'static str, &'static str) {
    match base {
        "BTC" => (1, "0.00005", "77000"),
        "ETH" => (2, "0.001", "2400"),
        "SOL" => (2, "0.02", "150"),
        "XRP" => (5, "1", "2.5"),
        "DOGE" => (5, "10", "0.2"),
        "ADA" => (5, "2", "0.6"),
        "LINK" => (3, "0.1", "15"),
        "LTC" => (2, "0.02", "90"),
        _ => (4, "0.1", "10"),
    }
}

struct SimBook {
    symbol: String,
    tick: Decimal,
    mid: i64, // in ticks
    step: i64,
    unit: f64, // typical level size in base units
    bids: BTreeMap<i64, Decimal>,
    asks: BTreeMap<i64, Decimal>,
}

/// Random-walk market for development and demos. NOT real prices.
pub async fn run_sim(engine: Shared, cfg: Arc<Config>) {
    let mut rng = StdRng::from_entropy();
    let mut books = vec![];
    {
        let mut e = engine.lock().unwrap();
        let now = now_ms();
        for symbol in &cfg.symbols {
            let (base, quote) = symbol.split_once('/').unwrap();
            let (prec, qty_min, start) = sim_spec(base);
            let tick = Decimal::new(1, prec);
            let start = d(start);
            e.set_instrument(Instrument {
                symbol: symbol.clone(),
                base: base.into(),
                quote: quote.into(),
                price_increment: tick,
                qty_increment: d("0.00000001"),
                qty_min: d(qty_min),
                cost_min: d("0.5"),
                price_precision: prec,
                qty_precision: 8,
                status: "online".into(),
            });
            let mid: i64 = (start / tick).try_into().unwrap();
            // synthetic history
            for iv in INTERVALS {
                let span = iv as i64 * 60;
                let end = now / 1000 / span * span;
                let mut p = mid as f64;
                let vol = 0.0006 * (iv as f64).sqrt();
                let mut hist = vec![];
                for i in 0..400 {
                    let c = p;
                    let o = c * (1.0 + rng.gen_range(-vol..vol));
                    let h = c.max(o) * (1.0 + rng.gen_range(0.0..vol / 2.0));
                    let l = c.min(o) * (1.0 - rng.gen_range(0.0..vol / 2.0));
                    let px = |x: f64| Decimal::from(x.round() as i64) * tick;
                    hist.push(Candle { t: end - i * span, o: px(o), h: px(h), l: px(l), c: px(c), v: d("1") });
                    p = o;
                }
                e.candles.seed(symbol, iv, hist, now);
            }
            let f: f64 = start.try_into().unwrap();
            books.push(SimBook {
                symbol: symbol.clone(),
                tick,
                mid,
                step: (mid / 20000).max(1),
                unit: 2000.0 / f,
                bids: BTreeMap::new(),
                asks: BTreeMap::new(),
            });
        }
        e.feed_up(now);
    }
    tracing::warn!("FEED=sim: prices are synthetic random walks, not real market data");
    let mut first = true;
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let now = now_ms();
        let mut e = engine.lock().unwrap();
        e.feed_up(now);
        for b in books.iter_mut() {
            let to_dec = |t: i64| Decimal::from(t) * b.tick;
            b.mid += rng.gen_range(-3..=3) * b.step;
            b.mid = b.mid.max(b.step * 100);
            // rebuild a 40-level ladder around the new mid, keeping most resting sizes
            let (old_b, old_a) = (b.bids.clone(), b.asks.clone());
            let (mut nb, mut na) = (BTreeMap::new(), BTreeMap::new());
            for i in 0..40i64 {
                for (side, price) in [(0, b.mid - b.step * (i + 1)), (1, b.mid + b.step * (i + 1))] {
                    let old = if side == 0 { old_b.get(&price) } else { old_a.get(&price) };
                    let q = match old {
                        Some(q) if rng.gen_bool(0.85) => *q,
                        _ => round8(Decimal::try_from(b.unit * rng.gen_range(0.05..1.5)).unwrap()),
                    };
                    if side == 0 { nb.insert(price, q) } else { na.insert(price, q) };
                }
            }
            let diff = |old: &BTreeMap<i64, Decimal>, new: &BTreeMap<i64, Decimal>| -> Vec<Level> {
                let mut out: Vec<Level> =
                    old.keys().filter(|p| !new.contains_key(p)).map(|p| (to_dec(*p), Decimal::ZERO)).collect();
                out.extend(new.iter().filter(|(p, q)| old.get(p) != Some(q)).map(|(p, q)| (to_dec(*p), *q)));
                out
            };
            if first {
                let all = |m: &BTreeMap<i64, Decimal>| m.iter().map(|(p, q)| (to_dec(*p), *q)).collect::<Vec<_>>();
                e.on_book_snapshot(&b.symbol, &all(&nb), &all(&na), now);
            } else {
                e.on_book_update(&b.symbol, &diff(&old_b, &nb), &diff(&old_a, &na), now);
            }
            b.bids = nb;
            b.asks = na;
            // a random market order hits the best level
            if rng.gen_bool(0.35) {
                let buy = rng.gen_bool(0.5);
                let lvl = if buy {
                    b.asks.iter_mut().next()
                } else {
                    b.bids.iter_mut().next_back()
                };
                if let Some((p, q)) = lvl {
                    let take = round8(*q * Decimal::try_from(rng.gen_range(0.05..0.9)).unwrap());
                    if take > Decimal::ZERO {
                        *q -= take;
                        let (price, left) = (to_dec(*p), *q);
                        let upd = vec![(price, left)];
                        if buy {
                            e.on_book_update(&b.symbol, &[], &upd, now);
                        } else {
                            e.on_book_update(&b.symbol, &upd, &[], now);
                        }
                        e.on_trade(&b.symbol, price, take, if buy { Side::Buy } else { Side::Sell }, now, true, now);
                    }
                }
            }
        }
        first = false;
    }
}
