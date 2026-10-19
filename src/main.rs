mod accounts;
mod api;
mod book;
mod candles;
mod config;
mod engine;
mod feed;
mod futures;
mod kfeed;
mod margin;
#[cfg(test)]
mod tests;
mod util;

use crate::engine::{Engine, Event};
use crate::util::now_ms;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cfg = Arc::new(config::Config::from_env());
    let (tx, _) = broadcast::channel::<Event>(4096);
    let engine = Arc::new(Mutex::new(Engine::open(cfg.clone(), tx.clone(), &cfg.db_path)));

    // bootstrap the admin account
    if !cfg.admin_password.is_empty() {
        let mut e = engine.lock().unwrap();
        if e.login_lookup(&cfg.admin_user).is_none() {
            let hash = accounts::hash_password(&cfg.admin_password);
            e.create_user(&cfg.admin_user, &hash, true, now_ms()).expect("create admin");
            tracing::info!("created admin account '{}'", cfg.admin_user);
        }
    } else if engine.lock().unwrap().user_count() == 0 {
        tracing::warn!("no users exist and ADMIN_PASSWORD is not set; set it to create the admin account");
    }
    if cfg.invite_code.is_empty() {
        tracing::warn!("INVITE_CODE is not set; self-registration is disabled");
    }

    let http = reqwest::Client::new();
    match cfg.feed.as_str() {
        "sim" => {
            tokio::spawn(feed::run_sim(engine.clone(), cfg.clone()));
        }
        _ => {
            tokio::spawn(feed::run_kraken(engine.clone(), cfg.clone()));
            if !cfg.perps.is_empty() {
                tokio::spawn(kfeed::run_kraken_futures(engine.clone(), cfg.clone(), http.clone()));
            }
        }
    }
    // perps: pay funding when it falls due and liquidate under-margined positions, once a second
    if !cfg.perps.is_empty() {
        let engine = engine.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                engine.lock().unwrap().fut_tick(now_ms());
            }
        });
    }

    // spot margin: charge rollover when due and liquidate accounts at the margin stop, once a second
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                engine.lock().unwrap().margin_tick(now_ms());
            }
        });
    }

    // tickers + exchange status, once a second
    {
        let (engine, tx) = (engine.clone(), tx.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let payload = engine.lock().unwrap().tickers_json(now_ms()).to_string();
                let _ = tx.send(Event::Global { payload: payload.into() });
            }
        });
    }
    // equity snapshots for curves, drawdown and Sharpe
    {
        let (engine, secs) = (engine.clone(), cfg.snapshot_secs.max(10));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                let now = now_ms();
                let mut e = engine.lock().unwrap();
                if e.feed_ok(now) {
                    e.snapshot_equity(now);
                }
            }
        });
    }

    let state = api::AppState { engine, cfg: cfg.clone(), tx, http };
    let app = api::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await.expect("bind");
    tracing::info!("{} paper exchange listening on http://{}", cfg.club_name, cfg.bind);
    axum::serve(listener, app).await.expect("server");
}
