//! REST + WebSocket API. Everything except /api/config, /api/login and
//! /api/register needs `Authorization: Bearer <session token or API key>`.

use crate::accounts::{hash_password, normalize_invite, valid_username, verify_password, User};
use crate::book::Side;
use crate::candles::INTERVALS;
use crate::config::Config;
use crate::engine::{Event, OType, OrderReq, Shared, Status, Tif};
use crate::feed::fetch_ohlc;
use crate::futures::MarginMode;
use crate::kfeed::fetch_perp_ohlc;
use crate::util::*;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::{header, request::Parts, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{async_trait, Json, Router};
use rand::Rng;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeader;

#[derive(Clone)]
pub struct AppState {
    pub engine: Shared,
    pub cfg: Arc<Config>,
    pub tx: broadcast::Sender<Event>,
    pub http: reqwest::Client,
}

pub struct ApiErr(StatusCode, String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

type R = Result<Json<Value>, ApiErr>;
type Q = Query<HashMap<String, String>>;

fn bad(msg: impl Into<String>) -> ApiErr {
    ApiErr(StatusCode::BAD_REQUEST, msg.into())
}

pub struct Auth(pub User, pub String);

#[async_trait]
impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiErr;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> Result<Self, ApiErr> {
        let unauth = || ApiErr(StatusCode::UNAUTHORIZED, "not signed in".into());
        let tok = parts
            .headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or_else(unauth)?
            .trim()
            .to_string();
        let user = st.engine.lock().unwrap().auth(&tok, now_ms()).ok_or_else(unauth)?;
        Ok(Auth(user, tok))
    }
}

fn admin(a: &Auth) -> Result<(), ApiErr> {
    if a.0.is_admin {
        Ok(())
    } else {
        Err(ApiErr(StatusCode::FORBIDDEN, "admin only".into()))
    }
}

pub fn router(state: AppState) -> Router {
    let web = state.cfg.web_dir.clone();
    Router::new()
        .route("/api/config", get(config))
        .route("/api/register", post(register))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/password", post(change_password))
        .route("/api/instruments", get(instruments))
        .route("/api/tickers", get(tickers))
        .route("/api/book", get(book))
        .route("/api/trades", get(trades))
        .route("/api/candles", get(candles))
        .route("/api/portfolio", get(portfolio))
        .route("/api/orders", get(list_orders).post(place_order).delete(cancel_all))
        .route("/api/orders/:id", get(get_order).delete(cancel_order))
        .route("/api/fills", get(fills))
        .route("/api/leaderboard", get(leaderboard))
        .route("/api/equity", get(equity))
        .route("/api/apikeys", get(list_keys).post(create_key))
        .route("/api/apikeys/:prefix", delete(delete_key))
        .route("/api/futures/account", get(fut_account))
        .route("/api/futures/transfer", post(fut_transfer))
        .route("/api/futures/settings", post(fut_settings))
        .route("/api/futures/close", post(fut_close))
        .route("/api/futures/margin", post(fut_margin))
        .route("/api/futures/positions", get(fut_positions))
        .route("/api/futures/funding", get(fut_funding))
        .route("/api/margin", get(margin_account))
        .route("/api/margin/close", post(margin_close))
        .route("/api/margin/positions", get(margin_positions))
        .route("/api/admin/users", get(admin_users))
        .route("/api/admin/users/:id/password", post(admin_password))
        .route("/api/admin/users/:id/disabled", post(admin_disabled))
        .route("/api/admin/season", post(admin_season))
        .route("/api/admin/invites", get(admin_invites).post(admin_create_invites))
        .route("/api/admin/invites/:code", delete(admin_revoke_invite))
        .route("/api/admin/reconcile", get(admin_reconcile))
        .route("/ws", get(ws))
        // no-cache = revalidate on every load, so browsers pick up a redeploy right away
        .fallback_service(SetResponseHeader::if_not_present(
            ServeDir::new(&web).fallback(ServeFile::new(format!("{web}/index.html"))),
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        .with_state(state)
}

// ------------------------------------------------------------------ accounts

async fn config(State(st): State<AppState>) -> Json<Value> {
    let e = st.engine.lock().unwrap();
    Json(json!({
        "club_name": st.cfg.club_name,
        "registration_open": !st.cfg.invite_code.is_empty() || e.unused_invites() > 0,
        "quote": st.cfg.quote,
        "symbols": st.cfg.symbols,
        "perps": st.cfg.perps,
        "fut_maker_fee": st.cfg.fut_maker_fee.to_string(),
        "fut_taker_fee": st.cfg.fut_taker_fee.to_string(),
        "liq_fee": st.cfg.liq_fee.to_string(),
        "maker_fee": st.cfg.maker_fee.to_string(),
        "taker_fee": st.cfg.taker_fee.to_string(),
        "start_balance": st.cfg.start_balance.normalize().to_string(),
        "require_note": st.cfg.require_note,
        "simulated_feed": st.cfg.feed == "sim",
        "season": {"id": e.season_id, "name": e.season_name, "start_ms": e.season_start_ms},
        "intervals": INTERVALS,
    }))
}

fn field<'a>(b: &'a Value, k: &str) -> &'a str {
    b[k].as_str().unwrap_or("").trim()
}

async fn register(State(st): State<AppState>, Json(b): Json<Value>) -> R {
    let (username, password, code) = (field(&b, "username").to_string(), field(&b, "password").to_string(), field(&b, "invite_code"));
    // the shared INVITE_CODE works any number of times; codes made on the admin page work once
    let shared = !st.cfg.invite_code.is_empty() && code == st.cfg.invite_code;
    let invite = normalize_invite(code);
    {
        let e = st.engine.lock().unwrap();
        if st.cfg.invite_code.is_empty() && e.unused_invites() == 0 {
            return Err(bad("registration is closed; ask the club admin for an account"));
        }
        if !shared && !e.invite_usable(&invite) {
            return Err(bad("wrong or already used invite code"));
        }
    }
    if !valid_username(&username) {
        return Err(bad("nickname must be 2-20 letters, digits, _ or -"));
    }
    if password.chars().count() < 8 {
        return Err(bad("password must be at least 8 characters"));
    }
    let hash = tokio::task::spawn_blocking(move || hash_password(&password)).await.unwrap();
    let now = now_ms();
    let mut e = st.engine.lock().unwrap();
    let uid = if shared { e.create_user(&username, &hash, false, now) } else { e.register_with_invite(&invite, &username, &hash, now) }
        .map_err(bad)?;
    let token = e.create_token(uid, "session", "", now);
    Ok(Json(json!({"token": token})))
}

async fn login(State(st): State<AppState>, Json(b): Json<Value>) -> R {
    let (username, password) = (field(&b, "username").to_string(), field(&b, "password").to_string());
    let now = now_ms();
    let found = {
        let e = st.engine.lock().unwrap();
        if e.login_blocked(&username, now) {
            return Err(ApiErr(StatusCode::TOO_MANY_REQUESTS, "too many failed attempts; try again in 10 minutes".into()));
        }
        e.login_lookup(&username)
    };
    let (uid, ok) = match found {
        Some((uid, hash, disabled)) => {
            let ok = tokio::task::spawn_blocking(move || verify_password(&password, &hash)).await.unwrap();
            (uid, ok && !disabled)
        }
        None => (0, false),
    };
    let mut e = st.engine.lock().unwrap();
    e.login_result(&username, ok, now);
    if !ok {
        return Err(ApiErr(StatusCode::UNAUTHORIZED, "wrong nickname or password".into()));
    }
    Ok(Json(json!({"token": e.create_token(uid, "session", "", now)})))
}

async fn logout(State(st): State<AppState>, a: Auth) -> R {
    if a.1.starts_with("s_") {
        st.engine.lock().unwrap().delete_token(&a.1);
    }
    Ok(Json(json!({"ok": true})))
}

async fn me(a: Auth) -> R {
    Ok(Json(json!({"id": a.0.id, "username": a.0.username, "is_admin": a.0.is_admin})))
}

async fn change_password(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    let (old, new) = (field(&b, "old_password").to_string(), field(&b, "new_password").to_string());
    if new.chars().count() < 8 {
        return Err(bad("password must be at least 8 characters"));
    }
    let hash = st.engine.lock().unwrap().login_lookup(&a.0.username).map(|x| x.1).unwrap_or_default();
    let new_hash = tokio::task::spawn_blocking(move || verify_password(&old, &hash).then(|| hash_password(&new)))
        .await
        .unwrap()
        .ok_or_else(|| bad("current password is wrong"))?;
    let now = now_ms();
    let mut e = st.engine.lock().unwrap();
    e.set_password(a.0.id, &new_hash);
    Ok(Json(json!({"token": e.create_token(a.0.id, "session", "", now)})))
}

async fn list_keys(State(st): State<AppState>, a: Auth) -> R {
    Ok(Json(json!({"keys": st.engine.lock().unwrap().list_api_keys(a.0.id)})))
}

async fn create_key(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    let label: String = field(&b, "label").chars().take(40).collect();
    let mut e = st.engine.lock().unwrap();
    if e.list_api_keys(a.0.id).len() >= 5 {
        return Err(bad("at most 5 API keys per account"));
    }
    let key = e.create_token(a.0.id, "api", &label, now_ms());
    Ok(Json(json!({"key": key, "note": "store this key now; it cannot be shown again"})))
}

async fn delete_key(State(st): State<AppState>, a: Auth, Path(prefix): Path<String>) -> R {
    st.engine.lock().unwrap().delete_api_key(a.0.id, &prefix);
    Ok(Json(json!({"ok": true})))
}

// ------------------------------------------------------------------ market data

fn symbol_of(st: &AppState, q: &HashMap<String, String>) -> Result<String, ApiErr> {
    let s = q.get("symbol").map(|s| s.to_uppercase()).unwrap_or_default();
    if st.cfg.is_market(&s) {
        Ok(s)
    } else {
        Err(bad("unknown or missing symbol"))
    }
}

async fn instruments(State(st): State<AppState>, _a: Auth) -> R {
    let e = st.engine.lock().unwrap();
    let list: Vec<Value> = st
        .cfg
        .markets()
        .filter_map(|s| e.instruments.get(s))
        .map(|i| {
            let mut v = serde_json::to_value(i).unwrap();
            if let Some(p) = e.perps.get(&i.symbol) {
                v["kraken_product"] = json!(p.product);
                v["max_leverage"] = json!(p.max_leverage().to_string());
                v["max_position"] = json!(p.max_position.map(|m| m.normalize().to_string()));
                v["margin_tiers"] = json!(p
                    .tiers
                    .iter()
                    .map(|t| json!({"from_value": t.0.normalize().to_string(), "initial": t.1.to_string(), "maintenance": t.2.to_string()}))
                    .collect::<Vec<_>>());
            }
            v
        })
        .collect();
    Ok(Json(json!({"instruments": list})))
}

async fn tickers(State(st): State<AppState>, _a: Auth) -> R {
    Ok(Json(st.engine.lock().unwrap().tickers_json(now_ms())))
}

async fn book(State(st): State<AppState>, _a: Auth, Query(q): Q) -> R {
    let s = symbol_of(&st, &q)?;
    let depth = q.get("depth").and_then(|d| d.parse().ok()).unwrap_or(25usize).clamp(1, 100);
    Ok(Json(st.engine.lock().unwrap().book_json(&s, depth)))
}

async fn trades(State(st): State<AppState>, _a: Auth, Query(q): Q) -> R {
    let s = symbol_of(&st, &q)?;
    let e = st.engine.lock().unwrap();
    let t: Vec<Value> = e.tape.get(&s).map(|t| t.iter().cloned().collect()).unwrap_or_default();
    Ok(Json(json!({"symbol": s, "trades": t})))
}

async fn candles(State(st): State<AppState>, _a: Auth, Query(q): Q) -> R {
    let s = symbol_of(&st, &q)?;
    let iv: u32 = q.get("interval").and_then(|d| d.parse().ok()).unwrap_or(15);
    if !INTERVALS.contains(&iv) {
        return Err(bad("interval must be one of 1, 5, 15, 60, 240, 1440 (minutes)"));
    }
    let now = now_ms();
    let stale = {
        let e = st.engine.lock().unwrap();
        e.candles.seeded_at(&s, iv).map(|t| now - t > 6 * 3_600_000).unwrap_or(true)
    };
    if stale && st.cfg.feed != "sim" {
        let fetched = if st.cfg.is_perp(&s) { fetch_perp_ohlc(&st.http, &s, iv).await } else { fetch_ohlc(&st.http, &s, iv).await };
        match fetched {
            Ok(hist) => st.engine.lock().unwrap().candles.seed(&s, iv, hist, now),
            Err(err) => tracing::warn!("could not load candle history for {s} {iv}m: {err}"),
        }
    }
    let e = st.engine.lock().unwrap();
    let rows: Vec<Value> = e
        .candles
        .get(&s, iv)
        .into_iter()
        .map(|c| json!([c.t, c.o.to_string(), c.h.to_string(), c.l.to_string(), c.c.to_string(), c.v.to_string()]))
        .collect();
    Ok(Json(json!({"symbol": s, "interval": iv, "candles": rows})))
}

// ------------------------------------------------------------------ trading

async fn portfolio(State(st): State<AppState>, a: Auth) -> R {
    Ok(Json(st.engine.lock().unwrap().portfolio(a.0.id)))
}

async fn place_order(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    let need = |k: &str| -> Result<&str, ApiErr> { b[k].as_str().ok_or_else(|| bad(format!("missing field: {k}"))) };
    let price = |k: &str| -> Result<Option<rust_decimal::Decimal>, ApiErr> {
        match &b[k] {
            Value::Null => Ok(None),
            v if v.as_str() == Some("") => Ok(None),
            v => dec(v).map(Some).ok_or_else(|| bad(format!("{k} is not a number"))),
        }
    };
    let qty_field = if b["qty"].is_null() { "order_qty" } else { "qty" };
    let req = OrderReq {
        symbol: need("symbol")?.to_uppercase(),
        side: Side::parse(need("side")?).ok_or_else(|| bad("side must be buy or sell"))?,
        otype: OType::parse(need("type")?).ok_or_else(|| {
            bad("type must be market, limit, stop-loss, take-profit, stop-loss-limit or take-profit-limit")
        })?,
        qty: price(qty_field)?.ok_or_else(|| bad("missing field: qty"))?,
        limit_price: price("limit_price")?,
        trigger_price: price("trigger_price")?,
        tif: match b["time_in_force"].as_str() {
            None => Tif::Gtc,
            Some(s) => Tif::parse(&s.to_lowercase()).ok_or_else(|| bad("time_in_force must be gtc, ioc or fok"))?,
        },
        post_only: b["post_only"].as_bool().unwrap_or(false),
        reduce_only: b["reduce_only"].as_bool().unwrap_or(false),
        // a spot margin order; 1 (or none) is a plain cash order
        leverage: match &b["leverage"] {
            Value::Null => None,
            v => Some(dec(v).ok_or_else(|| bad("leverage is not a number"))?).filter(|l| *l != rust_decimal::Decimal::ONE),
        },
        note: b["note"].as_str().map(|s| s.to_string()),
    };
    submit(&st, a.0.id, req)
}

/// Accept an order and let it reach the book after the simulated latency.
fn submit(st: &AppState, uid: i64, req: OrderReq) -> R {
    let order = st.engine.lock().unwrap().place_order(uid, req, now_ms()).map_err(bad)?;
    if order.status == Status::Pending {
        // simulated network + gateway latency before the order reaches the book
        let (lo, hi) = (st.cfg.latency_min_ms, st.cfg.latency_max_ms.max(st.cfg.latency_min_ms));
        let delay = rand::thread_rng().gen_range(lo..=hi);
        let engine = st.engine.clone();
        let id = order.id;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            engine.lock().unwrap().activate(id, now_ms());
        });
    }
    Ok(Json(json!({"order": order.to_json()})))
}

async fn list_orders(State(st): State<AppState>, a: Auth, Query(q): Q) -> R {
    let e = st.engine.lock().unwrap();
    let limit = q.get("limit").and_then(|d| d.parse().ok()).unwrap_or(100i64).clamp(1, 500);
    let orders = match q.get("status").map(|s| s.as_str()) {
        Some("closed") => e.order_history(a.0.id, limit),
        _ => e.open_orders(a.0.id),
    };
    Ok(Json(json!({"orders": orders})))
}

async fn get_order(State(st): State<AppState>, a: Auth, Path(id): Path<i64>) -> R {
    let o = st.engine.lock().unwrap().get_order(a.0.id, id);
    o.map(|o| Json(json!({"order": o}))).ok_or(ApiErr(StatusCode::NOT_FOUND, "order not found".into()))
}

async fn cancel_order(State(st): State<AppState>, a: Auth, Path(id): Path<i64>) -> R {
    st.engine.lock().unwrap().cancel_order(a.0.id, id, now_ms()).map_err(bad)?;
    Ok(Json(json!({"ok": true})))
}

async fn cancel_all(State(st): State<AppState>, a: Auth, Query(q): Q) -> R {
    let sym = q.get("symbol").map(|s| s.to_uppercase());
    let n = st.engine.lock().unwrap().cancel_all(a.0.id, sym.as_deref(), "cancelled by user", now_ms());
    Ok(Json(json!({"cancelled": n})))
}

async fn fills(State(st): State<AppState>, a: Auth, Query(q): Q) -> R {
    let limit = q.get("limit").and_then(|d| d.parse().ok()).unwrap_or(100i64).clamp(1, 500);
    Ok(Json(json!({"fills": st.engine.lock().unwrap().fills(a.0.id, limit)})))
}

async fn leaderboard(State(st): State<AppState>, _a: Auth) -> R {
    let e = st.engine.lock().unwrap();
    Ok(Json(json!({
        "season": {"id": e.season_id, "name": e.season_name, "start_ms": e.season_start_ms},
        "rows": e.leaderboard(), "past": e.past_seasons(),
    })))
}

async fn equity(State(st): State<AppState>, a: Auth, Query(q): Q) -> R {
    // equity curves are visible to all club members; holdings and orders are not
    let uid = q.get("user_id").and_then(|d| d.parse().ok()).unwrap_or(a.0.id);
    Ok(Json(json!({"user_id": uid, "points": st.engine.lock().unwrap().equity_curve(uid)})))
}

// ------------------------------------------------------------------ futures

fn perp_of(st: &AppState, b: &Value) -> Result<String, ApiErr> {
    let s = field(b, "symbol").to_uppercase();
    if st.cfg.is_perp(&s) {
        Ok(s)
    } else {
        Err(bad("symbol must be a perpetual, e.g. BTC-PERP"))
    }
}

async fn fut_account(State(st): State<AppState>, a: Auth) -> R {
    Ok(Json(st.engine.lock().unwrap().fut_account_json(a.0.id)))
}

/// {"amount": "1000", "to": "futures" | "spot"}
async fn fut_transfer(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    let amount = dec(&b["amount"]).ok_or_else(|| bad("amount is required"))?;
    let to_futures = match field(&b, "to") {
        "futures" => true,
        "spot" => false,
        _ => return Err(bad("to must be futures or spot")),
    };
    let mut e = st.engine.lock().unwrap();
    e.fut_transfer(a.0.id, amount, to_futures, now_ms()).map_err(bad)?;
    Ok(Json(e.fut_account_json(a.0.id)))
}

/// {"symbol": "BTC-PERP", "margin_mode": "isolated" | "cross", "leverage": 10}; with "dry_run": true
/// it only returns what a leverage change would do (margin and liquidation price before/after).
async fn fut_settings(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    let symbol = perp_of(&st, &b)?;
    let mode = MarginMode::parse(field(&b, "margin_mode")).ok_or_else(|| bad("margin_mode must be isolated or cross"))?;
    let leverage = dec(&b["leverage"]).ok_or_else(|| bad("leverage is required"))?;
    let mut e = st.engine.lock().unwrap();
    if b["dry_run"].as_bool().unwrap_or(false) {
        // what this leverage would do to the position and open orders, without doing it
        let max = e.perps.get(&symbol).map(|p| p.max_leverage()).unwrap_or(rust_decimal::Decimal::ONE);
        if leverage < rust_decimal::Decimal::ONE || leverage > max || !leverage.fract().is_zero() {
            return Err(bad(format!("leverage must be a whole number from 1 to {max}")));
        }
        return Ok(Json(e.plan_leverage(a.0.id, &symbol, leverage, now_ms()).map_err(bad)?.json()));
    }
    e.set_fut_settings(a.0.id, &symbol, mode, leverage, now_ms()).map_err(bad)?;
    Ok(Json(e.fut_account_json(a.0.id)))
}

/// {"symbol": "BTC-PERP", "amount": "50"}: add margin to an isolated position; a negative amount removes it.
async fn fut_margin(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    let symbol = perp_of(&st, &b)?;
    let amount = dec(&b["amount"]).ok_or_else(|| bad("amount is required"))?;
    let mut e = st.engine.lock().unwrap();
    e.adjust_margin(a.0.id, &symbol, amount, now_ms()).map_err(bad)?;
    Ok(Json(e.fut_account_json(a.0.id)))
}

/// Close the whole position on a perp with a reduce-only market order.
async fn fut_close(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    let symbol = perp_of(&st, &b)?;
    let size = {
        let e = st.engine.lock().unwrap();
        e.positions.get(&(a.0.id, symbol.clone())).map(|p| p.size).ok_or_else(|| bad("no open position on this market"))?
    };
    let req = OrderReq {
        symbol,
        side: if size > rust_decimal::Decimal::ZERO { Side::Sell } else { Side::Buy },
        otype: OType::Market,
        qty: size.abs(),
        limit_price: None,
        trigger_price: None,
        tif: Tif::Gtc,
        post_only: false,
        reduce_only: true,
        leverage: None,
        note: b["note"].as_str().map(|s| s.to_string()).or(Some("close position".into())),
    };
    submit(&st, a.0.id, req)
}

async fn margin_account(State(st): State<AppState>, a: Auth) -> R {
    Ok(Json(st.engine.lock().unwrap().margin_account_json(a.0.id)))
}

/// Close the whole spot margin position on a pair with a reduce-only market order.
async fn margin_close(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    let symbol = b["symbol"].as_str().unwrap_or("").to_uppercase();
    let (size, lev) = {
        let e = st.engine.lock().unwrap();
        e.mpos.get(&(a.0.id, symbol.clone())).map(|p| (p.size, p.leverage)).ok_or_else(|| bad("no open margin position on this pair"))?
    };
    let req = OrderReq {
        symbol,
        side: if size > rust_decimal::Decimal::ZERO { Side::Sell } else { Side::Buy },
        otype: OType::Market,
        qty: size.abs(),
        limit_price: None,
        trigger_price: None,
        tif: Tif::Gtc,
        post_only: false,
        reduce_only: true,
        leverage: Some(lev),
        note: b["note"].as_str().map(|s| s.to_string()).or(Some("close position".into())),
    };
    submit(&st, a.0.id, req)
}

async fn margin_positions(State(st): State<AppState>, a: Auth, Query(q): Q) -> R {
    let limit = q.get("limit").and_then(|d| d.parse().ok()).unwrap_or(100i64).clamp(1, 500);
    Ok(Json(json!({"positions": st.engine.lock().unwrap().margin_history(a.0.id, limit)})))
}

async fn fut_positions(State(st): State<AppState>, a: Auth, Query(q): Q) -> R {
    let limit = q.get("limit").and_then(|d| d.parse().ok()).unwrap_or(100i64).clamp(1, 500);
    Ok(Json(json!({"positions": st.engine.lock().unwrap().position_history(a.0.id, limit)})))
}

async fn fut_funding(State(st): State<AppState>, a: Auth, Query(q): Q) -> R {
    let limit = q.get("limit").and_then(|d| d.parse().ok()).unwrap_or(100i64).clamp(1, 500);
    Ok(Json(json!({"funding": st.engine.lock().unwrap().funding_history(a.0.id, limit)})))
}

// ------------------------------------------------------------------ admin

async fn admin_users(State(st): State<AppState>, a: Auth) -> R {
    admin(&a)?;
    Ok(Json(json!({"users": st.engine.lock().unwrap().list_users()})))
}

async fn admin_password(State(st): State<AppState>, a: Auth, Path(id): Path<i64>, Json(b): Json<Value>) -> R {
    admin(&a)?;
    let new = field(&b, "new_password").to_string();
    if new.chars().count() < 8 {
        return Err(bad("password must be at least 8 characters"));
    }
    let hash = tokio::task::spawn_blocking(move || hash_password(&new)).await.unwrap();
    st.engine.lock().unwrap().set_password(id, &hash);
    Ok(Json(json!({"ok": true})))
}

async fn admin_disabled(State(st): State<AppState>, a: Auth, Path(id): Path<i64>, Json(b): Json<Value>) -> R {
    admin(&a)?;
    if id == a.0.id {
        return Err(bad("you cannot disable your own account"));
    }
    let disabled = b["disabled"].as_bool().unwrap_or(true);
    let mut e = st.engine.lock().unwrap();
    if disabled {
        e.cancel_all(id, None, "account disabled", now_ms());
    }
    e.set_disabled(id, disabled);
    Ok(Json(json!({"ok": true})))
}

async fn admin_invites(State(st): State<AppState>, a: Auth) -> R {
    admin(&a)?;
    Ok(Json(json!({"invites": st.engine.lock().unwrap().list_invites(), "shared_code": st.cfg.invite_code})))
}

async fn admin_create_invites(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    admin(&a)?;
    let n = b["count"].as_u64().unwrap_or(1);
    if !(1..=100).contains(&n) {
        return Err(bad("count must be between 1 and 100"));
    }
    let note: String = field(&b, "note").chars().take(40).collect();
    Ok(Json(json!({"codes": st.engine.lock().unwrap().create_invites(n as usize, &note, now_ms())})))
}

async fn admin_revoke_invite(State(st): State<AppState>, a: Auth, Path(code): Path<String>) -> R {
    admin(&a)?;
    st.engine.lock().unwrap().revoke_invite(&normalize_invite(&code)).map_err(bad)?;
    Ok(Json(json!({"ok": true})))
}

async fn admin_season(State(st): State<AppState>, a: Auth, Json(b): Json<Value>) -> R {
    admin(&a)?;
    let name: String = field(&b, "name").chars().take(40).collect();
    if name.is_empty() {
        return Err(bad("season name is required"));
    }
    let mut e = st.engine.lock().unwrap();
    e.new_season(&name, now_ms());
    Ok(Json(json!({"ok": true, "season_id": e.season_id})))
}

async fn admin_reconcile(State(st): State<AppState>, a: Auth) -> R {
    admin(&a)?;
    Ok(Json(st.engine.lock().unwrap().reconcile()))
}

// ------------------------------------------------------------------ websocket

async fn ws(State(st): State<AppState>, up: WebSocketUpgrade) -> Response {
    up.on_upgrade(move |sock| ws_conn(st, sock))
}

/// Client -> server: {"op":"auth","token":"..."} then {"op":"subscribe","symbol":"BTC/USD"}.
/// Server -> client: book, trade, trades, tickers, order and fill messages.
async fn ws_conn(st: AppState, mut sock: WebSocket) {
    let mut rx = st.tx.subscribe();
    let mut user: Option<i64> = None;
    let mut symbol: Option<String> = None;
    loop {
        let out: Vec<String> = tokio::select! {
            m = sock.recv() => match m {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    let e = st.engine.lock().unwrap();
                    match v["op"].as_str() {
                        Some("auth") => match e.auth(v["token"].as_str().unwrap_or(""), now_ms()) {
                            Some(u) => {
                                user = Some(u.id);
                                vec![json!({"type": "auth", "ok": true}).to_string(), e.tickers_json(now_ms()).to_string()]
                            }
                            None => vec![json!({"type": "auth", "ok": false}).to_string()],
                        },
                        Some("subscribe") if user.is_some() => {
                            let s = v["symbol"].as_str().unwrap_or("").to_uppercase();
                            if st.cfg.is_market(&s) {
                                let tape: Vec<Value> = e.tape.get(&s).map(|t| t.iter().cloned().collect()).unwrap_or_default();
                                let msgs = vec![
                                    e.book_json(&s, 25).to_string(),
                                    json!({"type": "trades", "symbol": s, "trades": tape}).to_string(),
                                ];
                                symbol = Some(s);
                                msgs
                            } else {
                                vec![json!({"type": "error", "error": "unknown symbol"}).to_string()]
                            }
                        }
                        Some("ping") => vec![json!({"type": "pong"}).to_string()],
                        _ => vec![json!({"type": "error", "error": "send auth first, then subscribe"}).to_string()],
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => vec![],
            },
            ev = rx.recv() => match ev {
                Ok(Event::Public { symbol: s, payload }) if user.is_some() && symbol.as_deref() == Some(s.as_str()) => {
                    vec![payload.to_string()]
                }
                Ok(Event::Global { payload }) if user.is_some() => vec![payload.to_string()],
                Ok(Event::User { user_id, payload }) if Some(user_id) == user => vec![payload.to_string()],
                Ok(_) => vec![],
                Err(broadcast::error::RecvError::Lagged(_)) => vec![],
                Err(broadcast::error::RecvError::Closed) => break,
            },
        };
        for m in out {
            if sock.send(Message::Text(m)).await.is_err() {
                return;
            }
        }
    }
}
