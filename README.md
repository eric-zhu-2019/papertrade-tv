# Paper Exchange — a crypto paper-trading exchange for clubs

English | [中文](README.zh-CN.md)

Real market data (Kraken's public feeds) with pretend money. Students' orders never go to a real exchange: this server simulates their fills against Kraken's live order books and trade stream. No exchange account or API key is needed.

Two kinds of market: 8 spot pairs (BTC/USD and others) and the matching 8 perpetual futures (BTC-PERP and others, priced from Kraken Futures). Perps can be shorted and leveraged, and have funding and liquidation.

The backend is Rust (axum + tokio + SQLite); the frontend is a static page with no build step. The whole thing is one binary plus a `web/` directory.

## Running it

```bash
cp .env.example .env      # set ADMIN_PASSWORD and INVITE_CODE
docker compose up -d --build
```

The server listens on all interfaces, so any device on the LAN can open `http://<server-ip>:8080` (the port is `PT_PORT` in `.env`; set `PT_BIND=127.0.0.1` to allow local access only, e.g. behind nginx). Sign in with `ADMIN_USER` / `ADMIN_PASSWORD`. Students register themselves with an invite code, using just a nickname and a password: no email or real name is collected.

There are two kinds of invite code:

- **One-time codes**: the admin generates them in batches on Admin → Invite codes (1–100 per batch, with an optional note). Each code registers exactly one account and is then used up, and the table shows who used which code. Unused codes can be revoked. A failed registration (e.g. a taken nickname) does not use the code up. Codes look like `CF2V-XCYN`, avoid 0/O/1/I/L, and are case-insensitive with an optional hyphen.
- **Shared code**: `INVITE_CODE` in `.env`, usable any number of times. Leave it empty to allow only one-time codes; with neither, registration is closed. Run `docker compose up -d` after changing it.

To deploy to your own server (reachable over `ssh`, with Docker installed):

```bash
./deploy.sh my-server          # or DEPLOY_HOST=my-server ./deploy.sh
```

The script syncs the code to `~/apps/papertrade`. On the first run it writes a random admin password and invite code to the remote `.env`. It then builds and starts the container and runs the smoke test. Later runs never overwrite `.env` or the database.

Without Docker, run `cargo run --release` with the same environment variables as `.env.example`. For offline development use `FEED=sim cargo run`, which serves random-walk prices.

### nginx reverse proxy

Outside the LAN you need HTTPS, and WebSockets must be forwarded:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_read_timeout 300s;
}
```

If nginx also runs in Docker, put both containers on one Docker network and point `proxy_pass` at `http://papertrade:8080`.

### Backups

All data lives in one SQLite file in the Docker volume `papertrade-data`. Back it up at least before each season reset:

```bash
docker compose exec papertrade sh -c 'cat /data/papertrade.db' > backup.db
```

## Matching model

| Situation | What happens |
|---|---|
| Market orders and marketable limit orders | Walk the real L2 book level by level, so slippage comes from real depth; taker fee |
| Liquidity a student has taken | Cannot be taken again for `LIQUIDITY_TTL_MS` |
| Resting limit orders | Join the back of the real queue at their price (`queue_ahead`). They fill once real trades have used up the quantity ahead of them; cancellations ahead move them up; partial fills happen; maker fee |
| A real trade prints through a resting order's price | Fills at the order's price, up to the size of that real trade |
| One real trade | Is shared between students' resting orders by price, then time, and never used twice |
| The book crosses a resting order (a gap, or after a reconnect) | Fills at the order's price |
| Stop-loss and take-profit | Triggered by the real last trade price, then executed as market or limit orders |
| Latency | An order reaches the book 50–200 ms after it is accepted and fills against the book at that moment |
| Feed down, or silent for `STALE_MS` | New orders and matching pause; the book resyncs on reconnect |
| Trading rules | Minimum size and price/quantity precision come from Kraken's instrument data |

Every book update is checked against Kraken's CRC32 checksum, and the book resyncs on a mismatch.

**Built-in limits of the simulation**: students' orders don't move the real price, and there is no real counterparty queued behind them. With a $100,000 starting balance on major coins the error is small; the bigger the balances and the thinner the market, the more optimistic the results.

## Perpetual futures

Perps are simulated against Kraken Futures' real perpetual books and trades (`PF_XBTUSD` and others, WebSocket v1). Matching works as for spot; a fill opens, grows, reduces or flips a position instead of delivering coins.

| Topic | Rule |
|---|---|
| Contracts | 1 contract = 1 coin, settled in USD. One net position per market (one-way mode): an opposite order reduces the position first and flips with the rest |
| Wallet | A separate futures wallet; transfer USD in from spot before opening positions. Unrealised profit can only leave once realised |
| Leverage | As on Kraken: up to 100x for BTC, ETH and SOL, 50x for the rest. Larger positions fall into higher Kraken margin tiers, which lowers the usable leverage |
| Margin | Isolated (each position has its own margin and can lose at most that) or cross (the futures wallet backs all cross positions), set per market. The mode cannot change while the market has a position or orders |
| Adding / removing margin | Isolated only. Adding moves the liquidation price away; removing must leave the initial margin for the current leverage (unrealised profit does not count) |
| Fees on closing | Closing (including reduce-only orders) reserves no fee up front; the fee comes out of what the close returns. A resting order that would open a position without enough margin is cancelled instead of opening an unbacked position |
| Changing leverage with a position | Isolated: lowering tops margin up to entry value ÷ new leverage only if it is below that (margin added by hand stays); raising releases margin down to the same floor as removing margin. Cross: only the margin tied up changes; raising is always allowed, lowering is refused if not enough USD is available. Open orders on the market are re-reserved at the new leverage. `POST /api/futures/settings` with `"dry_run": true` returns margin and liquidation price before and after without changing anything |
| Feed down | Markets with a position can't change leverage or margin, and USD can't leave the futures wallet while positions are open |
| Fees | Kraken Futures entry tier: 0.02% maker, 0.05% taker |
| Mark price | Kraken's mark price drives unrealised P&L, margin and liquidation |
| Funding | Paid hourly at Kraken's rate for the period: longs pay shorts when it is positive, the reverse when negative. Isolated positions pay from their margin, cross positions from the wallet |
| Liquidation | When equity falls below maintenance margin, the position is closed at market against the real book and a 0.5% fee on its value (`LIQ_FEE`, our own choice, not checked against Kraken's) goes to the insurance fund. The insurance fund covers any loss beyond the margin, so accounts never go negative |
| Reduce-only | `reduce_only` orders can only shrink or close a position |

While the futures feed is down, perp orders, matching and liquidations pause; spot is unaffected. `FUTURES=0` turns futures off. A new season settles every position at the mark price and empties the futures wallets.

## Ledger

Every balance change goes into a double-entry ledger (the `ledger` table): each fill, fee, starting grant and season reset is a set of entries that sums to zero. Futures add accounts for the futures wallet (`fut:<user>`), isolated margin (`posm:<position>`), funding (`funding`) and the insurance fund (`insurance`). The admin page's ledger audit checks that every asset sums to zero, that each balance equals its ledger history, that reservations equal what open orders hold, and that each isolated position's margin equals its ledger history.

## Leaderboard and seasons

The leaderboard shows return, maximum drawdown, return / drawdown, Sharpe ratio (after 6+ days of history) and orders filled. Equity curves are visible to club members; holdings and orders are not.

The admin can end a season: the standings are archived, all orders cancelled, and everyone reset to the starting balance.

## API (for students writing bots)

Create an API key on the Account page and send `Authorization: Bearer <key>` with every request. Perp orders use the same `POST /api/orders` with `symbol` set to e.g. `BTC-PERP` and optionally `reduce_only`; the futures wallet, transfers, leverage settings, margin top-ups, one-click close, position history and funding history are under `/api/futures/*`. Order fields follow Kraken's names (`limit_price`, `trigger_price`, `time_in_force`, `post_only`); authentication is simplified to a bearer token. A full example is under Account → API quick start.

| Endpoint | Purpose |
|---|---|
| `POST /api/orders` | Place an order; it returns `pending`, and the result arrives over the WebSocket or by polling |
| `GET /api/orders`, `?status=closed`, `/{id}` | Open orders, order history, one order |
| `DELETE /api/orders/{id}`, `DELETE /api/orders` | Cancel one, cancel all |
| `GET /api/portfolio`, `/api/fills` | Balances, fills |
| `GET /api/book`, `/api/trades`, `/api/candles`, `/api/tickers`, `/api/instruments` | Market data |
| `WS /ws` | Send `{"op":"auth","token":…}`, then `{"op":"subscribe","symbol":…}` |

Order types: `market`, `limit`, `stop-loss`, `take-profit`, `stop-loss-limit`, `take-profit-limit`. Limit orders support `gtc`, `ioc`, `fok` and `post_only`.

`bots/trend_bot.py` is a complete example bot (Python standard library only): it buys pullbacks in an uptrend with resting orders and has a take-profit, a stop-loss, a break-even stop, an overall loss limit and an end-of-run exit. It saves its state to disk so it can carry on after a crash, and writes a report at the end. `bots/run_bot.sh` restarts it if it crashes.

## Development

```bash
cargo test          # matching, ledger, futures, Kraken message parsing and checksums
FEED=sim ADMIN_PASSWORD=adminpass123 INVITE_CODE=test cargo run
ADMIN_PASSWORD=adminpass123 python3 smoke_test.py   # end-to-end checks against a running server
```

`smoke_test.py` uses only the Python standard library and runs 36 checks in about 5 seconds: market data, WebSocket, every order type and its reservations, futures (transfers, leverage, isolated long, adding margin, changing leverage with a position, cross short, reduce-only, closing), account isolation, permissions, one-time invite codes, API keys and the ledger audit. It only needs `ADMIN_PASSWORD`. It uses dedicated accounts `smoke-a` / `smoke-b` / `smoke-c` (registered on the first run with one-time codes it creates), sells back what it bought and disables those accounts at the end, so they never appear on the leaderboard. `deploy.sh` runs it on the server after every deploy (`SMOKE=0 ./deploy.sh` skips it).

| File | Contents |
|---|---|
| `src/engine.rs` | Matching simulator and ledger |
| `src/book.rs` | Order book mirror, consumed liquidity, checksum |
| `src/feed.rs` | Kraken spot feed and the simulated feed (including perps) |
| `src/kfeed.rs` | Kraken Futures feed, contract rules, perp candles |
| `src/futures.rs` | Futures: wallet, positions, margin, funding, liquidation |
| `src/api.rs` | REST and WebSocket API |
| `src/accounts.rs` | Users, sessions, API keys, one-time invite codes |
| `src/candles.rs` | Candles |
| `web/` | Frontend (English and Chinese) |

Charts use [TradingView Lightweight Charts™](https://www.tradingview.com/) (Apache-2.0).

## License

Released under the [Apache License 2.0](LICENSE).
