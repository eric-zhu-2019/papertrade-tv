#!/usr/bin/env python3
"""Paper Exchange 冒烟测试：对一个正在运行的实例跑一遍端到端检查。

    python3 smoke_test.py [URL] [--env-file .env] [--symbol BTC/USD]

URL 默认 http://127.0.0.1:8080。只需要管理员密码 ADMIN_PASSWORD（ADMIN_USER 默认 admin），
从环境变量或 --env-file 读取。只用 Python 标准库。

测试使用专用账号 smoke-a / smoke-b（交易）和 smoke-c（一次性邀请码）：首次运行时用管理员现生成的
一次性邀请码注册，之后由管理员重置密码并启用；结束时撤单、卖回持仓并禁用，所以它们不会出现在
排行榜上。每次运行只花掉几美分手续费。
退出码：0 全部通过，1 有检查失败，2 无法开始（连不上、缺配置）。
"""

import argparse
import base64
import hashlib
import json
import os
import re
import secrets
import socket
import ssl
import struct
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from decimal import ROUND_CEILING, ROUND_FLOOR, Decimal

SMOKE_USERS = ("smoke-a", "smoke-b")
INVITE_USER = "smoke-c"  # registered once with a one-time code; every run re-tries that used code
PROBE_USER = "smoke-x"  # registration attempts that must fail; it should never exist
INVITE_NOTE = "smoke-test"
TTY = sys.stdout.isatty()


def color(code, s):
    return f"\033[{code}m{s}\033[0m" if TTY else s


class Fail(Exception):
    pass


def expect(cond, msg):
    if not cond:
        raise Fail(msg)


def D(x):
    return Decimal(str(x))


def fmt(d):
    return format(d.normalize(), "f")


def floor_to(x, inc):
    return (x / inc).to_integral_value(ROUND_FLOOR) * inc


def ceil_to(x, inc):
    return (x / inc).to_integral_value(ROUND_CEILING) * inc


# ------------------------------------------------------------------ HTTP


class Client:
    def __init__(self, base, token=None):
        self.base, self.token = base.rstrip("/"), token

    def call(self, method, path, body=None, token=None):
        tok = token or self.token
        req = urllib.request.Request(self.base + path, method=method)
        if tok:
            req.add_header("Authorization", f"Bearer {tok}")
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, data, timeout=15) as r:
                return r.status, json.loads(r.read() or b"null")
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                return e.code, json.loads(raw)
            except ValueError:
                return e.code, {"error": raw.decode(errors="replace")[:200]}

    def ok(self, method, path, body=None):
        status, data = self.call(method, path, body)
        if status != 200:
            raise Fail(f"{method} {path} -> {status} {data.get('error') if isinstance(data, dict) else data}")
        return data

    def get(self, path):
        return self.ok("GET", path)

    def post(self, path, body=None):
        return self.ok("POST", path, body if body is not None else {})

    def delete(self, path):
        return self.ok("DELETE", path)


def q(symbol):
    return urllib.parse.quote(symbol, safe="")


# ------------------------------------------------------------------ minimal WebSocket client (RFC 6455)


class WS:
    def __init__(self, base):
        u = urllib.parse.urlparse(base)
        port = u.port or (443 if u.scheme == "https" else 80)
        sock = socket.create_connection((u.hostname, port), timeout=10)
        if u.scheme == "https":
            sock = ssl.create_default_context().wrap_socket(sock, server_hostname=u.hostname)
        key = base64.b64encode(os.urandom(16)).decode()
        sock.sendall(
            (
                f"GET /ws HTTP/1.1\r\nHost: {u.netloc}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
            ).encode()
        )
        buf = b""
        while b"\r\n\r\n" not in buf:
            chunk = sock.recv(4096)
            if not chunk:
                raise Fail("WebSocket 握手时连接被关闭")
            buf += chunk
        head, self.buf = buf.split(b"\r\n\r\n", 1)
        lines = head.decode(errors="replace").split("\r\n")
        expect(" 101 " in lines[0] + " ", f"WebSocket 握手失败：{lines[0]}")
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        headers = {k.strip().lower(): v.strip() for k, _, v in (l.partition(":") for l in lines[1:])}
        expect(headers.get("sec-websocket-accept") == accept, "WebSocket 握手校验值不对")
        self.sock = sock

    def _read(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise Fail("WebSocket 连接被服务器关闭")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def _frame(self, opcode, payload):
        mask = os.urandom(4)
        n = len(payload)
        if n < 126:
            head = struct.pack("!BB", 0x80 | opcode, 0x80 | n)
        elif n < 65536:
            head = struct.pack("!BBH", 0x80 | opcode, 0x80 | 126, n)
        else:
            head = struct.pack("!BBQ", 0x80 | opcode, 0x80 | 127, n)
        self.sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def send(self, obj):
        self._frame(0x1, json.dumps(obj).encode())

    def recv(self, timeout):
        """Next JSON text message, or None on timeout."""
        self.sock.settimeout(timeout)
        try:
            data = b""
            while True:
                b0, b1 = self._read(2)
                n = b1 & 0x7F
                if n == 126:
                    n = struct.unpack("!H", self._read(2))[0]
                elif n == 127:
                    n = struct.unpack("!Q", self._read(8))[0]
                mask = self._read(4) if b1 & 0x80 else None
                payload = self._read(n)
                if mask:
                    payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
                op = b0 & 0x0F
                if op == 0x8:
                    raise Fail("WebSocket 被服务器关闭")
                if op == 0x9:
                    self._frame(0xA, payload)
                    continue
                if op in (0x1, 0x0):
                    data += payload
                    if b0 & 0x80:
                        return json.loads(data)
        except socket.timeout:
            return None

    def close(self):
        try:
            self._frame(0x8, b"")
            self.sock.close()
        except OSError:
            pass


# ------------------------------------------------------------------ the test run


class Run:
    def __init__(self, base, admin_user, admin_pw, symbol):
        self.base, self.admin_user, self.admin_pw, self.symbol = base, admin_user, admin_pw, symbol
        self.passed = self.failed = 0
        self.failures = []
        self.admin = self.a = self.b = None

    def check(self, name, fn):
        t = time.time()
        try:
            detail = fn()
            self.passed += 1
            extra = f"  {color('2', detail)}" if detail else ""
            print(f"  {color('32', '✓')} {name}{extra}  {color('2', f'{(time.time() - t) * 1000:.0f}ms')}")
            return True
        except Exception as e:  # noqa: BLE001 - every failure is reported, the run continues
            self.failed += 1
            msg = str(e) if isinstance(e, Fail) else f"{type(e).__name__}: {e}"
            self.failures.append(f"{name}：{msg}")
            print(f"  {color('31', '✗')} {name}  {color('31', msg)}")
            return False

    # ---- helpers

    def quote(self):
        t = next(x for x in self.a.get("/api/tickers")["tickers"] if x["symbol"] == self.symbol)
        expect(t["bid"] and t["ask"], f"{self.symbol} 盘口为空")
        return D(t["bid"]), D(t["ask"])

    def balance(self, client, asset):
        for x in client.get("/api/portfolio")["assets"]:
            if x["asset"] == asset:
                return D(x["total"]), D(x["locked"]), D(x["available"])
        return Decimal(0), Decimal(0), Decimal(0)

    def place(self, client, **order):
        order.setdefault("symbol", self.symbol)
        for k, v in list(order.items()):
            if isinstance(v, Decimal):
                order[k] = fmt(v)
        return client.post("/api/orders", order)["order"]

    def settle(self, client, oid, until=("filled", "canceled", "rejected", "open", "untriggered"), timeout=5):
        deadline = time.time() + timeout
        while True:
            o = client.get(f"/api/orders/{oid}")["order"]
            if o["status"] in until or time.time() > deadline:
                return o
            time.sleep(0.1)

    def small_qty(self, price):
        i = self._inst
        target = max(D(i["cost_min"]) * 3, Decimal(20)) / price
        return max(ceil_to(target, D(i["qty_increment"])), D(i["qty_min"]))

    # ---- setup

    def setup(self):
        print(color("1", "准备"))
        self.public = Client(self.base)

        def config():
            self.cfg = self.public.get("/api/config")
            expect(self.cfg["symbols"], "没有配置任何交易对")
            if self.symbol not in self.cfg["symbols"]:
                self.symbol = self.cfg["symbols"][0]
            self.taker, self.maker = D(self.cfg["taker_fee"]), D(self.cfg["maker_fee"])
            feed = "模拟行情 FEED=sim" if self.cfg["simulated_feed"] else "Kraken 实时行情"
            return f"{self.cfg['club_name']}，{feed}，{self.cfg['season']['name']}"

        if not self.check("服务可访问（/api/config）", config):
            return False

        def unauth():
            status, _ = self.public.call("GET", "/api/portfolio")
            expect(status == 401, f"未登录访问返回 {status}，应为 401")
            status, _ = self.public.call("GET", "/api/portfolio", token="s_not-a-real-token")
            expect(status == 401, f"伪造令牌返回 {status}，应为 401")

        self.check("未登录和伪造令牌被拒绝（401）", unauth)

        def admin_login():
            status, d = self.public.call("POST", "/api/login", {"username": self.admin_user, "password": self.admin_pw})
            expect(status == 200, f"管理员登录失败：{status} {d.get('error')}")
            self.admin = Client(self.base, d["token"])
            expect(self.admin.get("/api/me")["is_admin"], f"{self.admin_user} 不是管理员")

        if not self.check("管理员登录", admin_login):
            return False

        self.check("账本对账（测试前）", self.reconcile)

        def accounts():
            pw = secrets.token_urlsafe(18)
            users = {u["username"]: u for u in self.admin.get("/api/admin/users")["users"]}
            made = [name for name in SMOKE_USERS if name not in users]
            codes = self.admin.post("/api/admin/invites", {"count": len(made), "note": INVITE_NOTE})["codes"] if made else []
            for name, code in zip(made, codes):
                status, d = self.public.call("POST", "/api/register", {"username": name, "password": pw, "invite_code": code})
                expect(status == 200, f"用一次性邀请码注册 {name} 失败：{d.get('error')}")
            for name in SMOKE_USERS:
                if name in users:
                    uid = users[name]["id"]
                    self.admin.post(f"/api/admin/users/{uid}/disabled", {"disabled": False})
                    self.admin.post(f"/api/admin/users/{uid}/password", {"new_password": pw})
            tokens = []
            for name in SMOKE_USERS:
                status, d = self.public.call("POST", "/api/login", {"username": name, "password": pw})
                expect(status == 200, f"{name} 登录失败：{d.get('error')}")
                tokens.append(d["token"])
            self.a, self.b = (Client(self.base, t) for t in tokens)
            self.a.delete("/api/orders")
            self.b.delete("/api/orders")
            return f"新注册 {', '.join(made)}" if made else "复用已有账号"

        if not self.check(f"准备测试账号 {' / '.join(SMOKE_USERS)}", accounts):
            return False
        return True

    def reconcile(self):
        r = self.admin.get("/api/admin/reconcile")
        expect(r["ok"], "；".join(r["problems"][:3]))
        return f"{r['ledger_lines']} 条分录"

    # ---- market data

    def market_data(self):
        print(color("1", "行情"))
        sym = self.symbol

        def tickers():
            deadline = time.time() + 20  # right after a deploy the feed may still be syncing
            while True:
                t = self.a.get("/api/tickers")
                bad = [x["symbol"] for x in t["tickers"] if not x["tradable"]]
                if t["feed_ok"] and not bad:
                    break
                expect(time.time() < deadline, "行情中断，交易已暂停" if not t["feed_ok"] else f"不可交易：{', '.join(bad)}")
                time.sleep(1)
            for x in t["tickers"]:
                expect(x["bid"] and x["ask"] and x["last"], f"{x['symbol']} 缺少报价")
                expect(D(x["bid"]) < D(x["ask"]), f"{x['symbol']} 买一 {x['bid']} 不低于卖一 {x['ask']}")
            return f"{len(t['tickers'])} 个市场"

        if not self.check("行情在线，所有市场可交易、买一 < 卖一", tickers):
            return False

        def instruments():
            got = {i["symbol"]: i for i in self.a.get("/api/instruments")["instruments"]}
            missing = set(self.cfg["symbols"]) - set(got)
            expect(not missing, f"缺少交易规则：{', '.join(sorted(missing))}")
            self._inst = got[sym]
            i = self._inst
            return f"{sym} 最小 {fmt(D(i['qty_min']))}，数量步长 {fmt(D(i['qty_increment']))}，价格步长 {fmt(D(i['price_increment']))}"

        if not self.check("交易规则（/api/instruments）", instruments):
            return False

        def book():
            b = self.a.get(f"/api/book?symbol={q(sym)}&depth=20")
            bids, asks = [D(p) for p, _ in b["bids"]], [D(p) for p, _ in b["asks"]]
            expect(bids and asks, "盘口为空")
            expect(bids == sorted(bids, reverse=True), "买盘没有从高到低排序")
            expect(asks == sorted(asks), "卖盘没有从低到高排序")
            expect(bids[0] < asks[0], f"盘口交叉：买一 {bids[0]} ≥ 卖一 {asks[0]}")
            return f"{len(bids)}+{len(asks)} 档，价差 {fmt(asks[0] - bids[0])}"

        self.check(f"{sym} 盘口", book)

        def trades():
            t = self.a.get(f"/api/trades?symbol={q(sym)}")["trades"]
            expect(t, "没有成交记录")
            age = time.time() - max(x["ts"] for x in t) / 1000
            expect(age < 300, f"最近一笔成交在 {age:.0f} 秒前，行情可能卡住了")
            return f"{len(t)} 笔，最新一笔在 {age:.0f} 秒前"

        self.check(f"{sym} 最近成交", trades)

        def candles():
            out = []
            for iv in (1, 60):
                c = self.a.get(f"/api/candles?symbol={q(sym)}&interval={iv}")["candles"]
                expect(c, f"{iv} 分钟 K 线为空（Kraken REST 请求可能失败了，查服务器日志）")
                age = time.time() - c[-1][0]
                expect(age < iv * 60 * 2 + 60, f"{iv} 分钟 K 线最后一根在 {age / 60:.0f} 分钟前")
                out.append(f"{iv}m×{len(c)}")
            return "，".join(out)

        self.check(f"{sym} K 线", candles)

        def websocket():
            ws = WS(self.base)
            try:
                ws.send({"op": "auth", "token": self.a.token})
                seen, books, live, deadline = set(), 0, 0, time.time() + 15
                subscribed = False
                while time.time() < deadline and not (live and {"book", "trades", "tickers"} <= seen):
                    m = ws.recv(timeout=max(0.1, deadline - time.time()))
                    if m is None:
                        break
                    kind = m.get("type")
                    if kind == "auth":
                        expect(m.get("ok"), "WebSocket 认证失败")
                        ws.send({"op": "subscribe", "symbol": sym})
                        subscribed = True
                    elif kind == "error":
                        raise Fail(f"WebSocket 返回错误：{m.get('error')}")
                    elif kind == "book":
                        books += 1
                        live += books > 1  # the first book message is the subscribe snapshot
                    elif kind == "trade":
                        live += 1
                    seen.add(kind)
                expect(subscribed, "没有收到认证回复")
                expect({"book", "trades", "tickers"} <= seen, f"只收到 {sorted(seen)}，缺少快照")
                expect(live >= 1, "15 秒内没有收到任何实时更新")
                return f"认证、订阅、快照和实时推送都正常（{live} 条更新）"
            finally:
                ws.close()

        self.check("WebSocket 实时推送", websocket)
        return True

    # ---- trading

    def trading(self):
        print(color("1", f"交易（{self.symbol}，账号 {SMOKE_USERS[0]}）"))
        i = self._inst
        base, quote = i["base"], i["quote"]
        pinc = D(i["price_increment"])
        state = {}

        def market_buy():
            bid, ask = self.quote()
            qty = self.small_qty(ask)
            usd0, _, _ = self.balance(self.a, quote)
            base0, _, _ = self.balance(self.a, base)
            o = self.place(self.a, side="buy", type="market", qty=qty)
            expect(o["status"] == "pending", f"新订单状态是 {o['status']}，应为 pending（模拟延迟）")
            o = self.settle(self.a, o["id"], until=("filled", "canceled", "rejected"))
            expect(o["status"] == "filled", f"市价单状态 {o['status']}：{o.get('reason')}")
            filled, cost, fee, avg = D(o["filled"]), D(o["cum_cost"]), D(o["fee"]), D(o["avg_price"])
            expect(filled == qty, f"成交 {filled}，下单 {qty}")
            slip = abs(avg - ask) / ask
            expect(slip < Decimal("0.005"), f"成交均价 {avg} 偏离下单前卖一 {ask} 达 {slip:.2%}")
            expect(abs(fee - cost * self.taker) <= Decimal("0.000001"), f"手续费 {fee}，按吃单费率应为 {cost * self.taker}")
            usd1, _, _ = self.balance(self.a, quote)
            base1, _, _ = self.balance(self.a, base)
            expect(abs((usd0 - usd1) - (cost + fee)) <= Decimal("0.000001"), f"{quote} 减少 {usd0 - usd1}，应为 {cost + fee}")
            expect(base1 - base0 == filled, f"{base} 增加 {base1 - base0}，应为 {filled}")
            state.update(bought=filled, order=o["id"])
            return f"买入 {fmt(filled)} {base} @ {fmt(avg)}，手续费 {fmt(fee)} {quote}"

        if not self.check("市价买入：成交价贴近卖一、收吃单费、余额正确", market_buy):
            return

        def fills():
            f = [x for x in self.a.get("/api/fills?limit=20")["fills"] if x["order_id"] == state["order"]]
            expect(f, "成交记录里找不到这笔订单")
            expect(all(x["liquidity"] == "taker" for x in f), "市价单成交应标记为 taker")
            expect(sum(D(x["qty"]) for x in f) == state["bought"], "成交记录数量合计不对")
            return f"{len(f)} 条成交记录"

        self.check("成交记录（/api/fills）", fills)

        def resting_limit():
            bid, _ = self.quote()
            px = floor_to(bid * Decimal("0.9"), pinc)
            qty = self.small_qty(px)
            _, locked0, _ = self.balance(self.a, quote)
            o = self.place(self.a, side="buy", type="limit", limit_price=px, qty=qty)
            o = self.settle(self.a, o["id"])
            expect(o["status"] == "open", f"远低于市价的限价单状态 {o['status']}：{o.get('reason')}")
            expect(o["queue_ahead"] is not None, "挂单没有排队信息 queue_ahead")
            _, locked1, _ = self.balance(self.a, quote)
            expect(locked1 - locked0 == D(o["locked"]) > 0, f"冻结 {locked1 - locked0}，订单冻结 {o['locked']}")
            state.update(limit=o["id"], locked0=locked0)
            return f"买 {fmt(qty)} @ {fmt(px)}，冻结 {fmt(D(o['locked']))} {quote}"

        if self.check("限价挂单：进入 open 状态并冻结资金", resting_limit):

            def isolation():
                oid = state["limit"]
                status, _ = self.b.call("GET", f"/api/orders/{oid}")
                expect(status == 404, f"别人的订单返回 {status}，应为 404")
                status, _ = self.b.call("DELETE", f"/api/orders/{oid}")
                expect(status != 200, "其他账号居然能撤掉这笔订单")
                expect(self.a.get(f"/api/orders/{oid}")["order"]["status"] == "open", "订单被其他账号影响了")
                expect(all(o["id"] != oid for o in self.b.get("/api/orders")["orders"]), "其他账号能在委托列表里看到这笔订单")

            self.check(f"账号隔离：{SMOKE_USERS[1]} 看不到也撤不掉别人的订单", isolation)

            def cancel():
                self.a.delete(f"/api/orders/{state['limit']}")
                o = self.a.get(f"/api/orders/{state['limit']}")["order"]
                expect(o["status"] == "canceled", f"撤单后状态 {o['status']}")
                _, locked, _ = self.balance(self.a, quote)
                expect(locked == state["locked0"], f"撤单后仍冻结 {locked - state['locked0']} {quote}")

            self.check("撤单：状态 canceled，冻结资金释放", cancel)

        def immediate():
            bid, ask = self.quote()
            low = floor_to(bid * Decimal("0.9"), pinc)
            out = []
            cases = [
                ("post-only", dict(type="limit", limit_price=ceil_to(ask * Decimal("1.01"), pinc), post_only=True), "post-only"),
                ("IOC", dict(type="limit", limit_price=low, time_in_force="ioc"), "immediate"),
                ("FOK", dict(type="limit", limit_price=low, time_in_force="fok"), "fill-or-kill"),
            ]
            for label, extra, why in cases:
                o = self.place(self.a, side="buy", qty=self.small_qty(D(extra["limit_price"])), **extra)
                o = self.settle(self.a, o["id"], until=("filled", "canceled", "rejected"))
                expect(o["status"] == "canceled" and D(o["filled"]) == 0, f"{label} 单状态 {o['status']}，成交 {o['filled']}")
                expect(why in (o.get("reason") or ""), f"{label} 单取消原因不对：{o.get('reason')}")
                out.append(label)
            return "、".join(out) + " 都按规则取消且没有成交"

        self.check("post-only / IOC / FOK", immediate)

        def validation():
            bid, _ = self.quote()
            qinc, qmin = D(i["qty_increment"]), D(i["qty_min"])
            qty, px = self.small_qty(bid), floor_to(bid * Decimal("0.9"), pinc)
            bad = {
                "数量低于最小值": dict(side="buy", type="limit", limit_price=px, qty=qmin - qinc),
                "数量不是步长整数倍": dict(side="buy", type="limit", limit_price=px, qty=qty + qinc / 10),
                "价格不是步长整数倍": dict(side="buy", type="limit", limit_price=px + pinc / 10, qty=qty),
                "未知交易对": dict(symbol="NOPE/USD", side="buy", type="market", qty=qty),
                "未知订单类型": dict(side="buy", type="moon", qty=qty),
                "post-only 搭配 IOC": dict(side="buy", type="limit", limit_price=px, qty=qty, post_only=True, time_in_force="ioc"),
                "限价单缺少价格": dict(side="buy", type="limit", qty=qty),
            }
            for label, order in bad.items():
                order.setdefault("symbol", self.symbol)
                body = {k: fmt(v) if isinstance(v, Decimal) else v for k, v in order.items()}
                status, d = self.a.call("POST", "/api/orders", body)
                expect(status == 400, f"{label}：返回 {status}，应为 400")
                time.sleep(0.05)  # stay well inside the per-user order rate limit
            return f"{len(bad)} 种非法订单都返回 400"

        self.check("参数校验", validation)

        def insufficient():
            _, ask = self.quote()
            _, _, avail = self.balance(self.a, quote)
            qty = ceil_to(avail * 2 / ask, D(i["qty_increment"]))
            status, d = self.a.call("POST", "/api/orders", {"symbol": self.symbol, "side": "buy", "type": "market", "qty": fmt(qty)})
            expect(status == 400 and "insufficient" in (d.get("error") or ""), f"超额买入返回 {status} {d.get('error')}")
            status, d = self.a.call(
                "POST", "/api/orders", {"symbol": self.symbol, "side": "sell", "type": "market", "qty": fmt(state["bought"] * 1000)}
            )
            expect(status == 400 and "insufficient" in (d.get("error") or ""), f"超额卖出返回 {status} {d.get('error')}")
            return "超额买入和超额卖出都被拒绝"

        self.check("余额不足被拒绝", insufficient)

        def stop_loss():
            bid, _ = self.quote()
            trig = floor_to(bid * Decimal("0.8"), pinc)
            _, locked0, _ = self.balance(self.a, base)
            o = self.place(self.a, side="sell", type="stop-loss", trigger_price=trig, qty=state["bought"])
            o = self.settle(self.a, o["id"])
            expect(o["status"] == "untriggered", f"止损单状态 {o['status']}，应为 untriggered")
            _, locked1, _ = self.balance(self.a, base)
            expect(locked1 - locked0 == state["bought"], f"冻结 {locked1 - locked0} {base}，应为 {state['bought']}")
            self.a.delete(f"/api/orders/{o['id']}")
            _, locked2, _ = self.balance(self.a, base)
            expect(locked2 == locked0, f"撤销止损单后仍冻结 {locked2 - locked0} {base}")
            return f"触发价 {fmt(trig)}，等待触发、冻结持仓、撤销后释放"

        self.check("止损单", stop_loss)

        def market_sell():
            bid, _ = self.quote()
            base0, _, _ = self.balance(self.a, base)
            o = self.place(self.a, side="sell", type="market", qty=state["bought"])
            o = self.settle(self.a, o["id"], until=("filled", "canceled", "rejected"))
            expect(o["status"] == "filled", f"市价卖单状态 {o['status']}：{o.get('reason')}")
            avg = D(o["avg_price"])
            slip = abs(avg - bid) / bid
            expect(slip < Decimal("0.005"), f"成交均价 {avg} 偏离下单前买一 {bid} 达 {slip:.2%}")
            base1, _, _ = self.balance(self.a, base)
            expect(base0 - base1 == state["bought"], f"{base} 减少 {base0 - base1}，应为 {state['bought']}")
            return f"卖出 {fmt(state['bought'])} {base} @ {fmt(avg)}"

        self.check("市价卖出：持仓卖回", market_sell)

    # ---- perpetual futures

    def futures(self):
        perps = self.cfg.get("perps") or []
        if not perps:
            return
        coin = self.symbol.split("/")[0]
        sym = next((p for p in perps if p == f"{coin}-PERP"), perps[0])
        sym2 = next((p for p in perps if p != sym), sym)
        a = self.a
        print(color("1", f"合约（{sym} / {sym2}，账号 {SMOKE_USERS[0]}）"))
        inst = {i["symbol"]: i for i in a.get("/api/instruments")["instruments"]}

        def ticker(s):
            return next(x for x in a.get("/api/tickers")["tickers"] if x["symbol"] == s)

        def pos(s):
            return next((p for p in a.get("/api/futures/account")["positions"] if p["symbol"] == s), None)

        def qty_for(s, value=200):
            i = inst[s]
            return max(ceil_to(Decimal(value) / D(ticker(s)["ask"]), D(i["qty_increment"])), D(i["qty_min"]))

        def wait_flat(s):
            deadline = time.time() + 5
            while pos(s) and time.time() < deadline:
                time.sleep(0.2)
            return pos(s) is None

        def market():
            t = {x["symbol"]: x for x in a.get("/api/tickers")["tickers"]}
            for s in perps:
                x = t.get(s)
                expect(x and x["tradable"], f"{s} 不可交易")
                expect(x["mark"] and x["index"] and x["funding_rate"] is not None and x["next_funding_ms"], f"{s} 缺少标记价、指数或资金费率")
                dev = abs(D(x["mark"]) / D(x["index"]) - 1)
                expect(dev < Decimal("0.02"), f"{s} 标记价偏离指数 {dev:.2%}")
            i = inst.get(sym) or {}
            expect(i.get("max_leverage") and i.get("margin_tiers"), f"{sym} 没有保证金档位")
            rate = D(t[sym]["funding_rate"]) * 100
            return f"{len(perps)} 个永续合约；{sym} 最高 {i['max_leverage']}x，资金费率 {rate:+.4f}%/h"

        if not self.check("合约行情：标记价、指数、资金费率、保证金档位", market):
            return

        def transfer():
            w0 = D(a.get("/api/futures/account")["wallet"])
            usd0 = self.balance(a, "USD")[0]
            a.post("/api/futures/transfer", {"amount": "2000", "to": "futures"})
            acct = a.get("/api/futures/account")
            expect(D(acct["wallet"]) - w0 == 2000, f"合约钱包增加 {D(acct['wallet']) - w0}，应为 2000")
            expect(usd0 - self.balance(a, "USD")[0] == 2000, "现货 USD 没有减少 2000")
            status, _ = a.call("POST", "/api/futures/transfer", {"amount": fmt(D(acct["available"]) + 1), "to": "spot"})
            expect(status == 400, "转出超过可用余额居然成功了")
            return "现货 → 合约 2000 USD；超额转回被拒绝"

        if not self.check("资金划转", transfer):
            return

        def settings():
            status, _ = a.call("POST", "/api/futures/settings", {"symbol": sym, "margin_mode": "isolated", "leverage": 100000})
            expect(status == 400, "超过上限的杠杆没有被拒绝")
            a.post("/api/futures/settings", {"symbol": sym, "margin_mode": "isolated", "leverage": 10})
            a.post("/api/futures/settings", {"symbol": sym2, "margin_mode": "cross", "leverage": 5})
            st = a.get("/api/futures/account")["settings"]
            expect((st[sym]["margin_mode"], st[sym]["leverage"]) == ("isolated", "10"), f"{sym} 设置是 {st[sym]}")
            expect((st[sym2]["margin_mode"], st[sym2]["leverage"]) == ("cross", "5"), f"{sym2} 设置是 {st[sym2]}")
            return f"{sym} 逐仓 10x，{sym2} 全仓 5x；非法杠杆被拒绝"

        self.check("杠杆和保证金模式", settings)

        def long_isolated():
            qty = qty_for(sym)
            o = self.place(a, symbol=sym, side="buy", type="market", qty=qty)
            o = self.settle(a, o["id"], until=("filled", "canceled", "rejected"))
            expect(o["status"] == "filled", f"开多单状态 {o['status']}：{o.get('reason')}")
            p = pos(sym)
            expect(p and p["side"] == "long" and D(p["size"]) == qty, f"持仓不对：{p}")
            entry, margin, liq = D(p["entry"]), D(p["margin"]), D(p["liq_price"])
            expect(abs(margin - qty * entry / 10) <= Decimal("0.01"), f"保证金 {margin}，按 10x 应为 {qty * entry / 10}")
            expect(entry * Decimal("0.88") < liq < entry * Decimal("0.92"), f"10x 多单强平价 {liq} 应在开仓价下方约 9.5%")
            return f"多 {fmt(qty)} @ {fmt(entry)}，保证金 {margin:.2f}，强平价 {liq:.2f}"

        if self.check("逐仓开多：保证金和强平价", long_isolated):

            def reduce_only():
                status, d = a.call(
                    "POST", "/api/orders",
                    {"symbol": sym, "side": "buy", "type": "market", "qty": fmt(D(inst[sym]["qty_min"])), "reduce_only": True},
                )
                expect(status == 400 and "reduce-only" in (d.get("error") or ""), f"同方向只减仓单返回 {status} {d.get('error')}")
                return "同方向的只减仓单被拒绝"

            self.check("只减仓", reduce_only)

            def margin_and_leverage():
                p0 = pos(sym)
                a.post("/api/futures/margin", {"symbol": sym, "amount": "10"})
                p1 = pos(sym)
                expect(D(p1["margin"]) - D(p0["margin"]) == 10, f"追加 10 后保证金 {p1['margin']}，原来 {p0['margin']}")
                expect(D(p1["liq_price"]) < D(p0["liq_price"]), "追加保证金后多单强平价应该下移")
                status, _ = a.call("POST", "/api/futures/margin", {"symbol": sym, "amount": fmt(-(D(p1["margin_removable"]) + 1))})
                expect(status == 400, "减少的保证金超过上限居然成功了")
                # 20x: the dry run says what will happen, the change must match it
                plan = a.post("/api/futures/settings", {"symbol": sym, "margin_mode": "isolated", "leverage": 20, "dry_run": True})
                expect(pos(sym)["leverage"] == "10", "预演居然改了杠杆")
                a.post("/api/futures/settings", {"symbol": sym, "margin_mode": "isolated", "leverage": 20})
                p2 = pos(sym)
                expect(p2["leverage"] == "20" and D(p2["margin"]) == D(plan["margin_after"]), f"20x 后保证金 {p2['margin']}，预演为 {plan['margin_after']}")
                expect(D(p2["margin"]) <= D(p1["margin"]), "调高杠杆不应增加保证金")
                expect(D(p2["liq_price"]) >= D(p1["liq_price"]), "杠杆调高后多单强平价不应下移")
                status, _ = a.call("POST", "/api/futures/settings", {"symbol": sym, "margin_mode": "cross", "leverage": 20})
                expect(status == 400, "持仓时切换全仓居然成功了")
                return f"追加 10：强平价 {D(p0['liq_price']):.0f} → {D(p1['liq_price']):.0f}；调到 20x：保证金 {D(p2['margin']):.2f}，强平价 {D(p2['liq_price']):.0f}"

            self.check("追加保证金、持仓调杠杆", margin_and_leverage)

            def close_long():
                a.post("/api/futures/close", {"symbol": sym})
                expect(wait_flat(sym), "平仓后仓位还在")
                h = a.get("/api/futures/positions?limit=5")["positions"]
                expect(h and (h[0]["symbol"], h[0]["side"], h[0]["reason"]) == (sym, "long", "closed"), f"仓位历史不对：{h[:1]}")
                return f"净盈亏 {D(h[0]['net']):+.2f}（含手续费）"

            self.check("一键平仓，进入仓位历史", close_long)

        def short_cross():
            qty = qty_for(sym2)
            o = self.place(a, symbol=sym2, side="sell", type="market", qty=qty)
            o = self.settle(a, o["id"], until=("filled", "canceled", "rejected"))
            expect(o["status"] == "filled", f"开空单状态 {o['status']}：{o.get('reason')}")
            p = pos(sym2)
            expect(p and (p["side"], p["margin_mode"]) == ("short", "cross"), f"持仓不对：{p}")
            expect(D(p["liq_price"]) > D(p["entry"]), f"空单强平价 {p['liq_price']} 应高于开仓价 {p['entry']}")
            a.post("/api/futures/close", {"symbol": sym2})
            expect(wait_flat(sym2), "平仓后仓位还在")
            return f"空 {fmt(qty)} @ {fmt(D(p['entry']))}，强平价 {D(p['liq_price']):.2f}，已平仓"

        self.check("全仓做空并平仓", short_cross)

        def transfer_back():
            acct = a.get("/api/futures/account")
            expect(not acct["positions"], "还有没平掉的合约仓位")
            amt = D(acct["wallet"])
            if amt > 0:
                a.post("/api/futures/transfer", {"amount": fmt(amt), "to": "spot"})
            expect(D(a.get("/api/futures/account")["wallet"]) == 0, "合约钱包没有清空")
            return f"转回现货 {amt:.2f} USD"

        self.check("合约账户清空，资金转回现货", transfer_back)

    # ---- accounts and permissions

    def accounts(self):
        print(color("1", "账号与权限"))

        def forbidden():
            # only harmless admin calls: if the permission check were broken, a season reset
            # here would wipe everyone's balances
            b_id = self.b.get("/api/me")["id"]
            for method, path, body in (
                ("GET", "/api/admin/users", None),
                ("GET", "/api/admin/reconcile", None),
                ("POST", f"/api/admin/users/{b_id}/disabled", {"disabled": False}),
                ("GET", "/api/admin/invites", None),
                ("DELETE", "/api/admin/invites/NOPE-NOPE", None),
            ):
                status, _ = self.a.call(method, path, body)
                expect(status == 403, f"学生访问 {method} {path} 返回 {status}，应为 403")

        self.check("学生不能访问管理接口（403）", forbidden)

        def invites():
            adm, pub = self.admin, self.public

            def register(name, code):
                return pub.call("POST", "/api/register", {"username": name, "password": secrets.token_urlsafe(18), "invite_code": code})

            done = []
            if INVITE_USER not in {u["username"] for u in adm.get("/api/admin/users")["users"]}:
                code = adm.post("/api/admin/invites", {"count": 1, "note": INVITE_NOTE})["codes"][0]
                # typed in lower case without the hyphen, the way a student might
                status, d = register(INVITE_USER, code.replace("-", "").lower())
                expect(status == 200, f"用一次性邀请码注册 {INVITE_USER} 失败：{d.get('error')}")
                done.append(f"注册了 {INVITE_USER}")
            listed = adm.get("/api/admin/invites")["invites"]
            used = [i for i in listed if i["used_by_name"] == INVITE_USER]
            expect(used, f"邀请码列表里没有记录 {INVITE_USER} 用过的码")
            status, _ = register(PROBE_USER, used[0]["code"])
            expect(status == 400, f"已用过的码再次注册返回 {status}，应为 400")

            code = adm.post("/api/admin/invites", {"count": 1, "note": INVITE_NOTE})["codes"][0]
            expect(re.fullmatch(r"[A-HJKMNP-Z2-9]{4}-[A-HJKMNP-Z2-9]{4}", code), f"邀请码格式不对：{code}")
            expect(pub.get("/api/config")["registration_open"], "有未使用的邀请码时，注册应为开放状态")
            status, _ = register(SMOKE_USERS[0], code)
            expect(status == 400, f"用已存在的昵称 {SMOKE_USERS[0]} 注册返回 {status}，应为 400")
            row = next((i for i in adm.get("/api/admin/invites")["invites"] if i["code"] == code), None)
            expect(row and row["used_by"] is None, "注册失败却把邀请码用掉了")
            adm.delete(f"/api/admin/invites/{code}")
            status, _ = register(PROBE_USER, code)
            expect(status == 400, f"作废的码仍能注册（{status}）")
            status, _ = register(PROBE_USER, "NOPE-NOPE")
            expect(status == 400, f"不存在的码返回 {status}，应为 400")
            return "；".join(done + ["用过、作废、不存在的码都被拒绝，注册失败不消耗码"])

        self.check("一次性邀请码", invites)

        def api_key():
            key = self.a.post("/api/apikeys", {"label": "smoke-test"})["key"]
            status, d = self.a.call("GET", "/api/portfolio", token=key)
            expect(status == 200, f"用 API 密钥访问返回 {status}")
            keys = [k for k in self.a.get("/api/apikeys")["keys"] if k["label"] == "smoke-test"]
            expect(keys, "密钥列表里找不到新建的密钥")
            for k in keys:
                self.a.delete(f"/api/apikeys/{urllib.parse.quote(k['prefix'], safe='')}")
            status, _ = self.a.call("GET", "/api/portfolio", token=key)
            expect(status == 401, f"删除后的密钥仍可使用（{status}）")
            return "创建、使用、删除后失效"

        self.check("API 密钥", api_key)

        def board():
            rows = self.a.get("/api/leaderboard")["rows"]
            expect(any(r["username"] == SMOKE_USERS[0] for r in rows), "排行榜里没有测试账号")
            self.a.get("/api/equity")
            return f"{len(rows)} 名"

        self.check("排行榜和净值曲线", board)

    # ---- cleanup

    def cleanup(self):
        if not self.admin:
            return
        print(color("1", "收尾"))

        def tidy():
            done = []
            if self.a and self.b:
                for c in (self.a, self.b):
                    c.delete("/api/orders")
                    # futures left over from an interrupted run: close positions, move the USD back
                    if self.cfg.get("perps"):
                        acct = c.get("/api/futures/account")
                        for p in acct["positions"]:
                            c.call("POST", "/api/futures/close", {"symbol": p["symbol"]})
                            done.append(f"平掉剩余合约仓位 {p['symbol']}")
                        deadline = time.time() + 5
                        while acct["positions"] and time.time() < deadline:
                            time.sleep(0.2)
                            acct = c.get("/api/futures/account")
                        if D(acct["available"]) > 0:
                            c.call("POST", "/api/futures/transfer", {"amount": fmt(D(acct["available"])), "to": "spot"})
                # sell any leftover position from an interrupted run
                i = getattr(self, "_inst", None)
                if i:
                    _, _, avail = self.balance(self.a, i["base"])
                    qty = floor_to(avail, D(i["qty_increment"]))
                    if qty >= D(i["qty_min"]):
                        o = self.place(self.a, side="sell", type="market", qty=qty)
                        self.settle(self.a, o["id"], until=("filled", "canceled", "rejected"))
                        done.append(f"卖出剩余 {fmt(qty)} {i['base']}")
            users = {u["username"]: u for u in self.admin.get("/api/admin/users")["users"]}
            for name in (*SMOKE_USERS, INVITE_USER, PROBE_USER):
                if name in users:
                    self.admin.post(f"/api/admin/users/{users[name]['id']}/disabled", {"disabled": True})
            expect(PROBE_USER not in users, f"本不该注册成功的 {PROBE_USER} 被创建了（已禁用）")
            done.append("测试账号已禁用（不上排行榜）")
            # unused codes left behind by an interrupted run
            for i in self.admin.get("/api/admin/invites")["invites"]:
                if i["note"] == INVITE_NOTE and i["used_by"] is None:
                    self.admin.delete(f"/api/admin/invites/{i['code']}")
            return "，".join(done)

        self.check("撤单并禁用测试账号", tidy)
        self.check("账本对账（测试后）", self.reconcile)
        self.admin.call("POST", "/api/logout")


def read_env_file(path):
    env = {}
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, _, v = line.partition("=")
                env[k.strip()] = v.strip().strip("'\"")
    return env


def main():
    ap = argparse.ArgumentParser(description="Paper Exchange 冒烟测试")
    ap.add_argument("url", nargs="?", default="http://127.0.0.1:8080")
    ap.add_argument("--env-file", help="从这个文件读取 ADMIN_USER / ADMIN_PASSWORD（例如服务器上的 .env）")
    ap.add_argument("--symbol", default="BTC/USD", help="用来下单测试的交易对（默认 BTC/USD）")
    args = ap.parse_args()

    env = dict(os.environ)
    if args.env_file:
        env = {**read_env_file(args.env_file), **{k: v for k, v in os.environ.items() if k in ("ADMIN_USER", "ADMIN_PASSWORD")}}
    if not env.get("ADMIN_PASSWORD"):
        print("缺少 ADMIN_PASSWORD：用环境变量传入，或加 --env-file .env", file=sys.stderr)
        return 2

    print(color("1", f"Paper Exchange 冒烟测试 → {args.url}"))
    t0 = time.time()
    run = Run(args.url, env.get("ADMIN_USER", "admin"), env["ADMIN_PASSWORD"], args.symbol.upper())
    try:
        if not run.setup():
            print(color("31", "\n无法开始测试，后面的检查已跳过。"))
            return 2
        if run.market_data():
            run.trading()
            run.futures()
        run.accounts()
    finally:
        run.cleanup()

    took = time.time() - t0
    if run.failed:
        print(color("31", f"\n{run.passed} 项通过，{run.failed} 项失败（{took:.1f} 秒）"))
        for f in run.failures:
            print(color("31", f"  - {f}"))
        return 1
    print(color("32", f"\n全部 {run.passed} 项通过（{took:.1f} 秒）"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
