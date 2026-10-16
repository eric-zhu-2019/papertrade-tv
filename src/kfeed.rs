//! Market data for perpetual futures from Kraken Futures' public REST API and
//! WebSocket v1 feed.
//!
//! The v1 book feed sends the whole book once, then one level per message with
//! a per-product sequence number. A gap means a message was lost, so the
//! session is dropped and the book resynced. Tickers carry the mark price,
//! index and this hour's funding rate.

use crate::book::{Level, Side};
use crate::candles::Candle;
use crate::config::{perp_product, Config};
use crate::engine::{Engine, Instrument, Shared, Ticker};
use crate::util::*;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

const KF_WS: &str = "wss://futures.kraken.com/ws/v1";
const KF_REST: &str = "https://futures.kraken.com";

fn levels(v: &Value) -> Vec<Level> {
    v.as_array()
        .map(|a| a.iter().filter_map(|l| Some((dec(&l["price"])?, dec(&l["qty"])?))).collect())
        .unwrap_or_default()
}

/// Contract specs and margin tiers from Kraken's instruments endpoint. Returns how many perps were set up.
pub fn apply_instruments(eng: &mut Engine, v: &Value) -> usize {
    let mut n = 0;
    let all = v["instruments"].as_array().cloned().unwrap_or_default();
    for perp in eng.cfg.perps.clone() {
        let product = perp_product(&perp);
        let Some(i) = all.iter().find(|i| i["symbol"].as_str() == Some(product.as_str())) else {
            tracing::warn!("Kraken Futures does not list {product} ({perp})");
            continue;
        };
        let Some(tick) = dec(&i["tickSize"]) else { continue };
        // contract size step: precision 4 -> 0.0001, 0 -> 1
        let prec = i["contractValueTradePrecision"].as_i64().unwrap_or(0);
        let step = if prec >= 0 { Decimal::new(1, prec as u32) } else { Decimal::from(10i64.pow(prec.unsigned_abs() as u32)) };
        let tiers_v = if i["retailMarginLevels"].is_array() { &i["retailMarginLevels"] } else { &i["marginLevels"] };
        let tiers: Vec<(Decimal, Decimal, Decimal)> = tiers_v
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| {
                Some((dec(&t["numNonContractUnits"]).unwrap_or(Decimal::ZERO), dec(&t["initialMargin"])?, dec(&t["maintenanceMargin"])?))
            })
            .collect();
        if tiers.is_empty() {
            continue;
        }
        let online = i["tradeable"].as_bool().unwrap_or(false) && !i["suspended"].as_bool().unwrap_or(false);
        let tick = tick.normalize();
        eng.set_instrument(Instrument {
            symbol: perp.clone(),
            kind: "perp",
            base: perp.trim_end_matches("-PERP").to_string(),
            quote: eng.cfg.quote.clone(),
            price_increment: tick,
            qty_increment: step,
            qty_min: step,
            cost_min: Decimal::ZERO,
            price_precision: tick.scale(),
            qty_precision: step.scale(),
            status: if online { "online" } else { "halted" }.into(),
        });
        eng.set_perp_rules(&perp, tiers, dec(&i["maxPositionSize"]));
        n += 1;
    }
    n
}

/// Apply one Kraken Futures v1 message. Returns Err when a book sequence gap
/// means the local book can no longer be trusted.
pub fn handle_fut_message(eng: &mut Engine, text: &str, now: i64, seqs: &mut HashMap<String, i64>) -> Result<(), String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("bad json: {e}"))?;
    if let Some(ev) = v["event"].as_str() {
        if ev == "error" || ev == "alert" {
            tracing::error!("kraken futures: {v}");
        }
        return Ok(());
    }
    let product = v["product_id"].as_str().unwrap_or("");
    let Some(symbol) = eng.cfg.perps.iter().find(|p| perp_product(p) == product).cloned() else {
        return Ok(()); // heartbeat, or a product we do not trade
    };
    match v["feed"].as_str().unwrap_or("") {
        "book_snapshot" => {
            if let Some(seq) = v["seq"].as_i64() {
                seqs.insert(product.to_string(), seq);
            }
            eng.on_book_snapshot(&symbol, &levels(&v["bids"]), &levels(&v["asks"]), now);
        }
        "book" => {
            if let Some(seq) = v["seq"].as_i64() {
                if let Some(last) = seqs.get(product) {
                    if seq != last + 1 {
                        return Err(format!("book sequence gap on {product} ({last} -> {seq})"));
                    }
                }
                seqs.insert(product.to_string(), seq);
            }
            let (Some(price), Some(qty)) = (dec(&v["price"]), dec(&v["qty"])) else { return Ok(()) };
            let lv = [(price, qty)];
            match v["side"].as_str() {
                Some("buy") => eng.on_book_update(&symbol, &lv, &[], now),
                Some("sell") => eng.on_book_update(&symbol, &[], &lv, now),
                _ => {}
            }
        }
        "trade_snapshot" => {
            // newest first; replay oldest first so the tape ends up in order
            for t in v["trades"].as_array().into_iter().flatten().rev() {
                trade(eng, &symbol, t, false, now);
            }
        }
        "trade" => trade(eng, &symbol, &v, true, now),
        "ticker" => {
            eng.on_perp_ticker(
                &symbol,
                dec(&v["markPrice"]),
                dec(&v["index"]),
                dec(&v["relative_funding_rate"]),
                v["next_funding_rate_time"].as_i64(),
                dec(&v["openInterest"]),
                Ticker {
                    last: dec(&v["last"]),
                    high: dec(&v["high"]),
                    low: dec(&v["low"]),
                    volume: dec(&v["volume"]),
                    change_pct: dec(&v["change"]),
                },
            );
        }
        _ => {}
    }
    Ok(())
}

fn trade(eng: &mut Engine, symbol: &str, t: &Value, live: bool, now: i64) {
    // "fill" and "liquidation" are real trades; skip anything else (e.g. contract terminations)
    if !matches!(t["type"].as_str(), Some("fill") | Some("liquidation") | None) {
        return;
    }
    let (Some(price), Some(qty)) = (dec(&t["price"]), dec(&t["qty"])) else { return };
    let Some(side) = t["side"].as_str().and_then(Side::parse) else { return };
    let ts = t["time"].as_i64().unwrap_or(now);
    eng.on_trade(symbol, price, qty, side, ts, live, now);
}

async fn fetch_instruments(http: &reqwest::Client) -> Result<Value, String> {
    let v: Value = http
        .get(format!("{KF_REST}/derivatives/api/v3/instruments"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if v["result"] != "success" {
        return Err(format!("instruments: {}", v["error"]));
    }
    Ok(v)
}

async fn session(engine: &Shared, cfg: &Config) -> Result<(), String> {
    let url = std::env::var("KRAKEN_FUTURES_WS_URL").unwrap_or_else(|_| KF_WS.to_string());
    let (ws, _) = tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(url.as_str()))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;
    let (mut w, mut r) = ws.split();
    let products: Vec<String> = cfg.perps.iter().map(|p| perp_product(p)).collect();
    let subs = [
        json!({"event": "subscribe", "feed": "heartbeat"}),
        json!({"event": "subscribe", "feed": "ticker", "product_ids": products}),
        json!({"event": "subscribe", "feed": "book", "product_ids": products}),
        json!({"event": "subscribe", "feed": "trade", "product_ids": products}),
    ];
    for s in subs {
        w.send(Message::Text(s.to_string())).await.map_err(|e| format!("send: {e}"))?;
    }
    tracing::info!("kraken futures feed connected, {} perps", products.len());
    let mut seqs = HashMap::new();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(15), r.next()).await.map_err(|_| "feed went silent".to_string())?;
        match msg {
            Some(Ok(Message::Text(t))) => {
                let now = now_ms();
                let mut e = engine.lock().unwrap();
                e.fut_feed_up(now);
                handle_fut_message(&mut e, &t, now, &mut seqs)?;
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

/// Keep the Kraken Futures feed alive forever. While it is down perp orders are
/// refused, resting perp orders do not match, and nobody is liquidated.
pub async fn run_kraken_futures(engine: Shared, cfg: Arc<Config>, http: reqwest::Client) {
    let mut backoff = 1u64;
    loop {
        let started = Instant::now();
        // contract rules first: without margin tiers no perp order can be accepted
        match fetch_instruments(&http).await {
            Ok(v) => {
                let n = apply_instruments(&mut engine.lock().unwrap(), &v);
                tracing::info!("kraken futures: loaded contract rules for {n} perps");
            }
            Err(e) => tracing::warn!("kraken futures instruments: {e}"),
        }
        let err = session(&engine, &cfg).await.unwrap_err();
        engine.lock().unwrap().fut_feed_down();
        tracing::warn!("kraken futures feed down: {err}; perp trading paused, reconnecting in {backoff}s");
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

/// Historical perp candles from Kraken Futures' charts API.
pub async fn fetch_perp_ohlc(http: &reqwest::Client, perp: &str, interval: u32) -> Result<Vec<Candle>, String> {
    let res = match interval {
        1 => "1m",
        5 => "5m",
        15 => "15m",
        60 => "1h",
        240 => "4h",
        1440 => "1d",
        _ => return Err(format!("no Kraken Futures resolution for {interval} minutes")),
    };
    let to = now_ms() / 1000;
    let from = to - interval as i64 * 60 * 1500;
    let v: Value = http
        .get(format!("{KF_REST}/api/charts/v1/trade/{}/{res}", perp_product(perp)))
        .query(&[("from", from.to_string()), ("to", to.to_string())])
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let rows = v["candles"].as_array().ok_or("unexpected charts response")?;
    Ok(rows
        .iter()
        .filter_map(|c| {
            Some(Candle {
                t: c["time"].as_i64()? / 1000,
                o: dec(&c["open"])?,
                h: dec(&c["high"])?,
                l: dec(&c["low"])?,
                c: dec(&c["close"])?,
                v: dec(&c["volume"])?,
            })
        })
        .collect())
}
