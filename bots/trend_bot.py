#!/usr/bin/env python3
"""示例交易机器人：上升趋势回调时挂单买入，止盈 / 止损 / 保本，结束前全部卖回美元。

    PT_URL=http://127.0.0.1:8080 python3 trend_bot.py      # API 密钥放在同目录的 key 文件里

策略（参数都可以用环境变量调整，见下方常量）：
- 只做多。看 5 分钟 K 线：EMA12 > EMA26 > EMA72，且最近 4 小时涨了 1%–10%，同时 BTC 4 小时没跌超 1.5%。
- 价格回调到 EMA12 附近（但仍在 EMA26 之上）时，在买一挂 post-only 限价买单（挂单费）。3 分钟没成交就撤。
- 成交后挂 +3% 的止盈卖单（挂单费）；跌 1.5% 市价止损；涨到 +1.5% 后止损上移到 +0.7%（扣费后约保本）。
- 每笔 $15,000，最多同时 3 个仓位；机器人累计亏损达到 $1,000 就全部卖出并停止。
- 结束前 2 小时不再开新仓，前 30 分钟把止盈单挪到卖一，前 10 分钟市价卖出剩余仓位。

只买卖自己开的仓位，不动账号里原有的持仓。状态写在 state.json，进程重启后接着跑；
status.txt 是当前概况，结束时生成 report.md。在同目录 touch STOP 会立即卖出全部仓位并结束。
"""

import json
import os
import time
import urllib.error
import urllib.request
from decimal import ROUND_CEILING, ROUND_FLOOR, Decimal

E = os.environ.get
HERE = os.path.dirname(os.path.abspath(__file__))
BASE = E("PT_URL", "http://127.0.0.1:8080").rstrip("/")
KEY = open(E("PT_KEY_FILE", os.path.join(HERE, "key"))).read().strip()
STATE = os.path.join(HERE, "state.json")

DURATION = float(E("DURATION_H", "24")) * 3600
IV = int(E("CANDLE_MIN", "5"))  # candle interval the signals use
LOOKBACK = int(E("LOOKBACK", "48"))  # candles in the trend-return window (48 x 5 min = 4 h)
SIZE = Decimal(E("SIZE_USD", "15000"))
MAX_POS = int(E("MAX_POS", "3"))
MAX_LOSS = Decimal(E("MAX_LOSS_USD", "1000"))
TP, SL = Decimal(E("TP", "0.03")), Decimal(E("SL", "0.015"))
BE_AT, BE_STOP = Decimal(E("BE_AT", "0.015")), Decimal(E("BE_STOP", "0.007"))
ENTRY_TIMEOUT = float(E("ENTRY_TIMEOUT_S", "180"))
NO_NEW = DURATION - float(E("NO_NEW_ENTRIES_S", "7200"))
MAKER_EXIT = DURATION - float(E("MAKER_EXIT_S", "1800"))
FORCE_EXIT = DURATION - float(E("FORCE_EXIT_S", "600"))
COOLDOWN_WIN, COOLDOWN_LOSS = 900, 3600
LOOP_S = float(E("LOOP_S", "5"))
FORCE_ENTRY = E("FORCE_ENTRY", "")  # testing only: buy this symbol right away, ignoring the signal
TAG = "claude-bot"  # order note that marks the bot's own orders


def log(msg):
    print(time.strftime("%m-%d %H:%M:%S"), msg, flush=True)


def D(x):
    return Decimal(str(x))


def fmt(d, places=2):
    return f"{d:,.{places}f}"


def floor_to(x, inc):
    return (x / inc).to_integral_value(ROUND_FLOOR) * inc


def ceil_to(x, inc):
    return (x / inc).to_integral_value(ROUND_CEILING) * inc


def ema(xs, n):
    k, e = 2 / (n + 1), xs[0]
    for x in xs[1:]:
        e = x * k + e * (1 - k)
    return e


# ------------------------------------------------------------------ HTTP


def api(method, path, body=None):
    """(status, json). GETs retry through a server restart; orders are never sent twice."""
    for attempt in range(6):
        req = urllib.request.Request(
            BASE + path,
            method=method,
            # Cloudflare (when the exchange is served through it) blocks urllib's default User-Agent
            headers={"Authorization": "Bearer " + KEY, "Content-Type": "application/json", "User-Agent": "papertrade-bot/1.0"},
        )
        try:
            with urllib.request.urlopen(req, json.dumps(body).encode() if body is not None else None, timeout=15) as r:
                return r.status, json.loads(r.read() or b"null")
        except urllib.error.HTTPError as e:
            try:
                j = json.loads(e.read())
            except ValueError:
                j = {"error": f"HTTP {e.code}"}
            if e.code >= 500 and method == "GET":
                time.sleep(2 * (attempt + 1))
                continue
            return e.code, j
        except (urllib.error.URLError, OSError) as e:
            refused = isinstance(getattr(e, "reason", e), ConnectionRefusedError)
            if method != "GET" and not refused:
                raise  # the request may have arrived; never risk a duplicate order
            time.sleep(3 * (attempt + 1))
    raise RuntimeError(f"{method} {path}: server unreachable")


def get(path):
    s, j = api("GET", path)
    if s != 200:
        raise RuntimeError(f"GET {path}: {s} {j.get('error') if isinstance(j, dict) else j}")
    return j


def order(oid):
    return get(f"/api/orders/{oid}")["order"]


def place(**body):
    body = {k: format(v.normalize(), "f") if isinstance(v, Decimal) else v for k, v in body.items()}
    body["note"] = TAG
    s, j = api("POST", "/api/orders", body)
    if s != 200:
        log(f"  order refused: {body['side']} {body['symbol']} {body['type']}: {j.get('error')}")
        return None
    return j["order"]


def cancel(oid):
    api("DELETE", f"/api/orders/{oid}")
    for _ in range(20):  # a pending order may still be on its way to the book
        o = order(oid)
        if o["status"] not in ("pending", "open", "untriggered"):
            return o
        time.sleep(0.2)
        api("DELETE", f"/api/orders/{oid}")
    return order(oid)


# ------------------------------------------------------------------ the bot


class Bot:
    def __init__(self):
        self.inst = {i["symbol"]: i for i in get("/api/instruments")["instruments"]}
        self.symbols = list(self.inst)
        self.candles, self.candles_at = {}, {}
        self.last_summary = self.last_sweep = 0
        if os.path.exists(STATE):
            with open(STATE) as f:
                self.s = json.load(f)
            log(f"resumed: {len(self.s['positions'])} positions, {len(self.s['trades'])} closed trades")
        else:
            p = get("/api/portfolio")
            t = self.tickers()
            self.s = {
                "start": time.time(), "start_equity": p["equity"], "btc_start": t["BTC/USD"]["last"],
                "start_assets": {a["asset"]: a["total"] for a in p["assets"]},
                "positions": {}, "pending": {}, "cooldown": {}, "trades": [], "killed": None, "done": False,
            }
            self.save()
            log(f"start: equity ${p['equity']}, {len(self.symbols)} symbols, run {DURATION / 3600:g} h")

    def save(self):
        tmp = STATE + ".tmp"
        with open(tmp, "w") as f:
            json.dump(self.s, f, indent=1)
        os.replace(tmp, STATE)

    def tickers(self):
        return {x["symbol"]: x for x in get("/api/tickers")["tickers"]}

    def closes(self, sym):
        now = time.time()
        if now - self.candles_at.get(sym, 0) > 60:
            c = get(f"/api/candles?symbol={sym.replace('/', '%2F')}&interval={IV}")["candles"]
            self.candles[sym], self.candles_at[sym] = [float(x[4]) for x in c], now
        return self.candles[sym]

    # ---- signals

    def trend(self, sym):
        c = self.closes(sym)
        if len(c) < max(200, LOOKBACK + 1):
            return None
        c = c[-300:]
        return {"e12": ema(c, 12), "e26": ema(c, 26), "e72": ema(c, 72), "ret": c[-1] / c[-1 - LOOKBACK] - 1}

    def signal(self, sym, t, btc_ret):
        if FORCE_ENTRY == sym:
            return 1.0, "forced (test)"
        tr = self.trend(sym)
        if not tr or not t["bid"] or not t["ask"]:
            return None
        bid, ask = float(t["bid"]), float(t["ask"])
        if (ask - bid) / bid > 0.001 or btc_ret < -0.015:
            return None
        uptrend = tr["e12"] > tr["e26"] > tr["e72"] and 0.01 <= tr["ret"] <= 0.10
        pullback = tr["e26"] <= bid <= tr["e12"] * 1.0005
        if uptrend and pullback:
            return tr["ret"], f"4h {tr['ret']:+.2%}, bid {bid:g} vs EMA12 {tr['e12']:.6g} / EMA26 {tr['e26']:.6g}"
        return None

    # ---- accounting

    def exits(self, pos):
        """(qty sold, proceeds after fees, gross value sold, an exit order still in flight)"""
        sold, proceeds, gross, busy = Decimal(0), Decimal(0), Decimal(0), False
        for oid in pos["exits"]:
            o = order(oid)
            sold += D(o["filled"])
            gross += D(o["cum_cost"])
            proceeds += D(o["cum_cost"]) - D(o["fee"])
            busy |= o["status"] == "pending"
        return sold, proceeds, gross, busy

    def bot_pnl(self, tk):
        pnl = sum(D(x["pnl"]) for x in self.s["trades"])
        for sym, pos in self.s["positions"].items():
            qty, cost = D(pos["qty"]), D(pos["cost"])
            sold, proceeds, _, _ = self.exits(pos)
            bid = D(tk[sym]["bid"] or pos["price"])
            pnl += proceeds + (qty - sold) * bid - cost
        return pnl

    # ---- order handling

    def check_pending(self, sym, t, now, phase):
        p = self.s["pending"][sym]
        o = order(p["id"])
        if o["status"] == "open":
            stale = now - p["at"] > ENTRY_TIMEOUT or phase != "normal" or self.s["killed"]
            ran = t["bid"] and D(t["bid"]) > D(p["price"]) * Decimal("1.003")
            if stale or ran:
                o = cancel(p["id"])
        if o["status"] in ("filled", "canceled", "rejected"):
            del self.s["pending"][sym]
            if D(o["filled"]) > 0:
                self.open_position(sym, o)
            else:
                log(f"entry {sym} not filled ({o.get('reason') or o['status']})")
            self.save()

    def open_position(self, sym, o):
        qty, cost = D(o["filled"]), D(o["cum_cost"]) + D(o["fee"])
        price = D(o["cum_cost"]) / qty
        pos = {"qty": str(qty), "cost": str(cost), "price": str(price), "stop": str(price * (1 - SL)),
               "tp": str(ceil_to(price * (1 + TP), D(self.inst[sym]["price_increment"]))),
               "tp_id": None, "exits": [], "opened": time.time(), "reason": None}
        self.s["positions"][sym] = pos
        log(f"BOUGHT {sym} {qty} @ {price:.8g} (${fmt(cost)} incl. fee); stop {D(pos['stop']):.8g}, take-profit {pos['tp']}")
        self.place_tp(sym, pos, D(pos["tp"]))

    def place_tp(self, sym, pos, price):
        sold = self.exits(pos)[0]
        qty = floor_to(D(pos["qty"]) - sold, D(self.inst[sym]["qty_increment"]))
        if qty < D(self.inst[sym]["qty_min"]):
            return
        o = place(symbol=sym, side="sell", type="limit", qty=qty, limit_price=price, post_only=True)
        if o:
            pos["tp_id"] = o["id"]
            pos["exits"].append(o["id"])

    def manage(self, sym, t, now, phase):
        pos = self.s["positions"][sym]
        inst = self.inst[sym]
        qty, price = D(pos["qty"]), D(pos["price"])
        if pos["tp_id"]:
            tp = order(pos["tp_id"])
            if tp["status"] not in ("pending", "open"):
                pos["tp_id"] = None
        sold, proceeds, gross, busy = self.exits(pos)
        left = qty - sold
        if left < D(inst["qty_min"]) and not busy and not pos["tp_id"]:
            return self.close(sym, pos, sold, proceeds, gross, left)
        if busy or not t["tradable"] or not t["bid"]:
            return
        bid, ask = D(t["bid"]), D(t["ask"])
        if bid >= price * (1 + BE_AT) and D(pos["stop"]) < price * (1 + BE_STOP):
            pos["stop"] = str(price * (1 + BE_STOP))
            log(f"{sym} up {bid / price - 1:+.2%}: stop raised to {D(pos['stop']):.8g} (break-even after fees)")
        why = None
        if bid <= D(pos["stop"]):
            why = "stop" if D(pos["stop"]) < price else "break-even stop"
        elif phase == "force":
            why = "end of run"
        elif self.s["killed"]:
            why = self.s["killed"]
        elif bid >= D(pos["tp"]) and not pos["tp_id"]:
            why = "take-profit (taker)"
        if why:
            if pos["tp_id"]:
                cancel(pos["tp_id"])
                pos["tp_id"] = None
                sold = self.exits(pos)[0]
            sell = floor_to(qty - sold, D(inst["qty_increment"]))
            if sell >= D(inst["qty_min"]):
                pos["reason"] = why
                log(f"SELL {sym} {sell} at market: {why} (bid {bid:.8g}, entry {price:.8g})")
                o = place(symbol=sym, side="sell", type="market", qty=sell)
                if o:
                    pos["exits"].append(o["id"])
        elif phase == "maker_exit" and (not pos["tp_id"] or D(pos["tp"]) > ask):
            # wind down: offer at the best ask so the exit still pays the maker fee
            if pos["tp_id"]:
                cancel(pos["tp_id"])
                pos["tp_id"] = None
            pos["tp"], pos["reason"] = str(ask), "end of run (maker)"
            self.place_tp(sym, pos, ask)
        elif not pos["tp_id"]:
            self.place_tp(sym, pos, D(pos["tp"]))  # the last one was cancelled (e.g. post-only)
        self.save()

    def close(self, sym, pos, sold, proceeds, gross, left):
        qty, cost = D(pos["qty"]), D(pos["cost"])
        pnl = proceeds - cost * sold / qty
        if pos["reason"] is None:
            pos["reason"] = "take-profit"
        tr = {"symbol": sym, "qty": str(sold), "entry": pos["price"], "exit": str(gross / sold) if sold else None,
              "pnl": str(pnl), "reason": pos["reason"], "opened": pos["opened"], "closed": time.time(),
              "dust": str(left)}
        self.s["trades"].append(tr)
        del self.s["positions"][sym]
        self.s["cooldown"][sym] = time.time() + (COOLDOWN_WIN if pnl > 0 else COOLDOWN_LOSS)
        log(f"CLOSED {sym}: {pos['reason']}, P&L ${fmt(pnl)}" + (f" (unsellable remainder {left})" if left > 0 else ""))
        self.save()

    # ---- main loop

    def step(self):
        now = time.time()
        el = now - self.s["start"]
        phase = "normal" if el < NO_NEW else "no_new" if el < MAKER_EXIT else "maker_exit" if el < FORCE_EXIT else "force"
        if os.path.exists(os.path.join(HERE, "STOP")) and not self.s["killed"]:
            self.s["killed"] = "stopped by hand"
            log("STOP file found: selling everything")
        tk = self.tickers()
        if now - self.last_sweep > 60:
            self.sweep()
        for sym in list(self.s["pending"]):
            self.check_pending(sym, tk[sym], now, phase)
        for sym in list(self.s["positions"]):
            self.manage(sym, tk[sym], now, phase)
        pnl = self.bot_pnl(tk)
        if pnl <= -MAX_LOSS and not self.s["killed"]:
            self.s["killed"] = f"loss limit (${fmt(pnl)})"
            log(f"LOSS LIMIT: bot P&L ${fmt(pnl)}; selling everything and stopping")
            self.save()
        if phase == "normal" and not self.s["killed"]:
            self.enter(tk, now)
        self.status(tk, pnl, el, phase)
        finished = el >= DURATION or self.s["killed"]
        return finished and not self.s["positions"] and not self.s["pending"]

    def sweep(self):
        """Cancel open orders tagged as ours that the bot lost track of (e.g. a timeout while placing)."""
        self.last_sweep = time.time()
        known = {p["id"] for p in self.s["pending"].values()}
        for pos in self.s["positions"].values():
            known.update(pos["exits"])
        for o in get("/api/orders")["orders"]:
            if o.get("note") == TAG and o["id"] not in known:
                log(f"cancelling untracked bot order #{o['id']} {o['side']} {o['symbol']}")
                cancel(o["id"])

    def enter(self, tk, now):
        slots = MAX_POS - len(self.s["positions"]) - len(self.s["pending"])
        if slots <= 0:
            return
        btc = self.trend("BTC/USD")
        btc_ret = btc["ret"] if btc else 0
        cands = []
        for sym in self.symbols:
            if sym in self.s["positions"] or sym in self.s["pending"] or self.s["cooldown"].get(sym, 0) > now:
                continue
            t = tk[sym]
            if not t["tradable"]:
                continue
            sig = self.signal(sym, t, btc_ret)
            if sig:
                cands.append((sig[0], sym, sig[1]))
        usd = next((D(a["available"]) for a in get("/api/portfolio")["assets"] if a["asset"] == "USD"), Decimal(0))
        for _, sym, why in sorted(cands, reverse=True)[:slots]:
            inst, bid = self.inst[sym], D(tk[sym]["bid"])
            qty = floor_to(SIZE / bid, D(inst["qty_increment"]))
            if usd < qty * bid * Decimal("1.005") or qty < D(inst["qty_min"]):
                log(f"skip {sym}: not enough USD")
                continue
            o = place(symbol=sym, side="buy", type="limit", qty=qty, limit_price=bid, post_only=True)
            if o:
                usd -= qty * bid * Decimal("1.005")
                self.s["pending"][sym] = {"id": o["id"], "price": str(bid), "at": now}
                log(f"BUY order {sym} {qty} @ {bid} (post-only): {why}")
                self.save()

    def status(self, tk, pnl, el, phase):
        lines = [
            f"{time.strftime('%m-%d %H:%M:%S')}  elapsed {el / 3600:.2f} h of {DURATION / 3600:g} h  phase {phase}"
            + (f"  STOPPED: {self.s['killed']}" if self.s["killed"] else ""),
            f"bot P&L ${fmt(pnl)}  ({len(self.s['trades'])} closed trades, {sum(1 for x in self.s['trades'] if D(x['pnl']) > 0)} winners)",
        ]
        for sym, pos in self.s["positions"].items():
            bid = D(tk[sym]["bid"] or pos["price"])
            lines.append(f"  holding {sym} {pos['qty']} @ {D(pos['price']):.8g}  now {bid:.8g} ({bid / D(pos['price']) - 1:+.2%})"
                         f"  stop {D(pos['stop']):.8g}  take-profit {pos['tp']}")
        for sym, p in self.s["pending"].items():
            lines.append(f"  buy order {sym} @ {p['price']} (waiting to fill)")
        with open(os.path.join(HERE, "status.txt"), "w") as f:
            f.write("\n".join(lines) + "\n")
        if time.time() - self.last_summary > 1800:  # a market snapshot in the log every 30 min
            self.last_summary = time.time()
            log(lines[1])
            parts = []
            for sym in self.symbols:
                tr = self.trend(sym)
                if tr:
                    state = "up" if tr["e12"] > tr["e26"] > tr["e72"] else "down" if tr["e12"] < tr["e26"] < tr["e72"] else "flat"
                    parts.append(f"{sym.split('/')[0]} {tr['ret']:+.1%} {state}")
            log("  market 4h: " + ", ".join(parts))

    def report(self):
        tk = self.tickers()
        p = get("/api/portfolio")
        trades = self.s["trades"]
        pnl = sum(D(x["pnl"]) for x in trades)
        wins = [x for x in trades if D(x["pnl"]) > 0]
        btc0, btc1 = D(self.s["btc_start"]), D(tk["BTC/USD"]["last"])
        hours = (time.time() - self.s["start"]) / 3600
        out = [
            "# 交易机器人报告", "",
            f"- 运行：{time.strftime('%Y-%m-%d %H:%M', time.localtime(self.s['start']))} 起，共 {hours:.1f} 小时"
            + (f"（提前结束：{self.s['killed']}）" if self.s["killed"] else ""),
            f"- 机器人盈亏（已扣手续费）：**${fmt(pnl)}**，{len(trades)} 笔交易，{len(wins)} 笔盈利",
            f"- 账户总净值：${self.s['start_equity']} → ${p['equity']}（含你原有持仓的涨跌）",
            f"- 对比：什么都不做 = $0；拿 ${fmt(SIZE, 0)} 买入 BTC 持有全程 = "
            f"${fmt(SIZE * (btc1 / btc0) * (1 - Decimal('0.0065')) - SIZE)}（BTC {btc1 / btc0 - 1:+.2%}，已扣来回手续费）",
            "", "| 币种 | 数量 | 买入价 | 卖出均价 | 盈亏 | 原因 | 持有 |", "|---|---|---|---|---|---|---|",
        ]
        for x in trades:
            held = (x["closed"] - x["opened"]) / 60
            out.append(f"| {x['symbol']} | {x['qty']} | {D(x['entry']):.8g} | {D(x['exit']):.8g} | ${fmt(D(x['pnl']))} | {x['reason']} | {held:.0f} 分钟 |"
                       if x["exit"] else f"| {x['symbol']} | {x['qty']} | {D(x['entry']):.8g} | – | ${fmt(D(x['pnl']))} | {x['reason']} | {held:.0f} 分钟 |")
        if not trades:
            out.append("| – | | | | | 期间没有出现符合条件的入场机会 | |")
        with open(os.path.join(HERE, "report.md"), "w") as f:
            f.write("\n".join(out) + "\n")
        log(f"DONE: bot P&L ${fmt(pnl)}, {len(trades)} trades; report.md written")


def main():
    bot = Bot()
    if bot.s["done"]:
        log("this run already finished; delete state.json to start a new one")
        return
    while True:
        try:
            if bot.step():
                break
        except Exception as e:  # noqa: BLE001 - keep trading through transient errors
            log(f"error: {type(e).__name__}: {e}")
            time.sleep(10)
        time.sleep(LOOP_S)
    bot.s["done"] = True
    bot.save()
    bot.report()


if __name__ == "__main__":
    main()
