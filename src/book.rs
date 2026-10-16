use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "buy" => Some(Side::Buy),
            "sell" => Some(Side::Sell),
            _ => None,
        }
    }
}

pub type Level = (Decimal, Decimal);

/// Mirror of the real exchange's L2 book, plus an overlay of liquidity that
/// simulated takers have already eaten (so the same real quote cannot be hit
/// twice before the market has had time to replenish it).
pub struct Book {
    pub bids: BTreeMap<Decimal, Decimal>,
    pub asks: BTreeMap<Decimal, Decimal>,
    consumed_bids: HashMap<Decimal, (Decimal, i64)>,
    consumed_asks: HashMap<Decimal, (Decimal, i64)>,
    pub depth: usize,
    pub ready: bool,
}

impl Book {
    pub fn new(depth: usize) -> Book {
        Book {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            consumed_bids: HashMap::new(),
            consumed_asks: HashMap::new(),
            depth,
            ready: false,
        }
    }

    pub fn snapshot(&mut self, bids: &[Level], asks: &[Level]) {
        self.bids = bids.iter().filter(|l| !l.1.is_zero()).cloned().collect();
        self.asks = asks.iter().filter(|l| !l.1.is_zero()).cloned().collect();
        self.consumed_bids.clear();
        self.consumed_asks.clear();
        self.truncate();
        self.ready = true;
    }

    pub fn update(&mut self, bids: &[Level], asks: &[Level]) {
        for (p, q) in bids {
            if q.is_zero() {
                self.bids.remove(p);
                self.consumed_bids.remove(p);
            } else {
                self.bids.insert(*p, *q);
            }
        }
        for (p, q) in asks {
            if q.is_zero() {
                self.asks.remove(p);
                self.consumed_asks.remove(p);
            } else {
                self.asks.insert(*p, *q);
            }
        }
        self.truncate();
    }

    /// The exchange does not send deletions for levels that slide out of the
    /// subscribed depth; the client must drop them itself.
    fn truncate(&mut self) {
        while self.bids.len() > self.depth {
            self.bids.pop_first();
        }
        while self.asks.len() > self.depth {
            self.asks.pop_last();
        }
    }

    /// Best level on a side. `Side::Buy` = bids, `Side::Sell` = asks.
    pub fn best(&self, side: Side) -> Option<Level> {
        match side {
            Side::Buy => self.bids.iter().next_back().map(|(p, q)| (*p, *q)),
            Side::Sell => self.asks.iter().next().map(|(p, q)| (*p, *q)),
        }
    }

    pub fn mid(&self) -> Option<Decimal> {
        let (b, _) = self.best(Side::Buy)?;
        let (a, _) = self.best(Side::Sell)?;
        Some((a + b) / Decimal::TWO)
    }

    pub fn qty_at(&self, side: Side, price: Decimal) -> Decimal {
        let m = match side {
            Side::Buy => &self.bids,
            Side::Sell => &self.asks,
        };
        m.get(&price).copied().unwrap_or(Decimal::ZERO)
    }

    pub fn top(&self, side: Side, n: usize) -> Vec<Level> {
        match side {
            Side::Buy => self.bids.iter().rev().take(n).map(|(p, q)| (*p, *q)).collect(),
            Side::Sell => self.asks.iter().take(n).map(|(p, q)| (*p, *q)).collect(),
        }
    }

    /// Levels in price priority with the quantity still available to simulated takers.
    pub fn avail_levels(&self, side: Side, now: i64) -> Vec<Level> {
        // a perp book keeps every level; no order walks further than this
        let n = self.depth.min(1000);
        let (all, consumed) = match side {
            Side::Buy => (self.top(Side::Buy, n), &self.consumed_bids),
            Side::Sell => (self.top(Side::Sell, n), &self.consumed_asks),
        };
        all.into_iter()
            .filter_map(|(p, q)| {
                let used = match consumed.get(&p) {
                    Some((c, exp)) if *exp > now => *c,
                    _ => Decimal::ZERO,
                };
                let a = q - used;
                if a > Decimal::ZERO {
                    Some((p, a))
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn consume(&mut self, side: Side, price: Decimal, qty: Decimal, now: i64, ttl_ms: i64) {
        let m = match side {
            Side::Buy => &mut self.consumed_bids,
            Side::Sell => &mut self.consumed_asks,
        };
        m.retain(|_, v| v.1 > now);
        let e = m.entry(price).or_insert((Decimal::ZERO, 0));
        e.0 += qty;
        e.1 = now + ttl_ms;
    }

    /// Kraken v2 book checksum: CRC32 over the top 10 asks then top 10 bids.
    pub fn checksum(&self, price_prec: u32, qty_prec: u32) -> u32 {
        fn fmt(mut x: Decimal, prec: u32, out: &mut String) {
            x.rescale(prec);
            let s = x.to_string().replace('.', "");
            let t = s.trim_start_matches('0');
            out.push_str(t);
        }
        let mut s = String::with_capacity(512);
        for (p, q) in self.top(Side::Sell, 10) {
            fmt(p, price_prec, &mut s);
            fmt(q, qty_prec, &mut s);
        }
        for (p, q) in self.top(Side::Buy, 10) {
            fmt(p, price_prec, &mut s);
            fmt(q, qty_prec, &mut s);
        }
        crc32fast::hash(s.as_bytes())
    }
}
