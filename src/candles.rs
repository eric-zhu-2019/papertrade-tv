use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};

pub const INTERVALS: [u32; 6] = [1, 5, 15, 60, 240, 1440];
const MAX_CANDLES: usize = 1500;

#[derive(Clone, Debug)]
pub struct Candle {
    pub t: i64, // bucket start, epoch seconds
    pub o: Decimal,
    pub h: Decimal,
    pub l: Decimal,
    pub c: Decimal,
    pub v: Decimal,
}

#[derive(Default)]
pub struct CandleStore {
    map: HashMap<(String, u32), VecDeque<Candle>>,
    seeded: HashMap<(String, u32), i64>,
}

impl CandleStore {
    pub fn seeded_at(&self, symbol: &str, interval: u32) -> Option<i64> {
        self.seeded.get(&(symbol.to_string(), interval)).copied()
    }

    pub fn invalidate(&mut self) {
        self.seeded.clear();
    }

    /// Replace history with exchange data, keeping any newer candles built from live trades.
    pub fn seed(&mut self, symbol: &str, interval: u32, mut hist: Vec<Candle>, now_ms: i64) {
        let key = (symbol.to_string(), interval);
        hist.sort_by_key(|c| c.t);
        let last_t = hist.last().map(|c| c.t).unwrap_or(0);
        let mut q: VecDeque<Candle> = hist.into();
        if let Some(old) = self.map.get(&key) {
            for c in old.iter().filter(|c| c.t > last_t) {
                q.push_back(c.clone());
            }
        }
        while q.len() > MAX_CANDLES {
            q.pop_front();
        }
        self.map.insert(key.clone(), q);
        self.seeded.insert(key, now_ms);
    }

    pub fn on_trade(&mut self, symbol: &str, price: Decimal, qty: Decimal, ts_ms: i64) {
        for iv in INTERVALS {
            let key = (symbol.to_string(), iv);
            let span = iv as i64 * 60;
            let bucket = ts_ms / 1000 / span * span;
            let q = self.map.entry(key).or_default();
            match q.back_mut() {
                Some(last) if last.t == bucket => {
                    last.h = last.h.max(price);
                    last.l = last.l.min(price);
                    last.c = price;
                    last.v += qty;
                }
                Some(last) if last.t > bucket => {}
                _ => {
                    q.push_back(Candle { t: bucket, o: price, h: price, l: price, c: price, v: qty });
                    if q.len() > MAX_CANDLES {
                        q.pop_front();
                    }
                }
            }
        }
    }

    pub fn get(&self, symbol: &str, interval: u32) -> Vec<Candle> {
        self.map
            .get(&(symbol.to_string(), interval))
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }
}
