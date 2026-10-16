# Paper Exchange — 俱乐部加密货币模拟交易所

真实行情（Kraken 公开数据）加虚拟资金。学生的订单不会发到任何真实交易所，而是在本服务器内对着 Kraken 的实时订单簿和成交流模拟撮合。不需要任何交易所账户或 API 密钥。

后端是 Rust（axum + tokio + SQLite），前端是无构建步骤的静态页面。整体是一个二进制文件加一个 `web/` 目录。

## 启动

```bash
cp .env.example .env      # 修改 ADMIN_PASSWORD 和 INVITE_CODE
docker compose up -d --build
```

服务默认监听所有网卡，局域网内的设备可以直接访问 `http://<服务器IP>:8080`（端口由 `.env` 里的 `PT_PORT` 决定；设置 `PT_BIND=127.0.0.1` 则只允许本机访问，适合放在 nginx 后面）。打开该地址，用 `ADMIN_USER` / `ADMIN_PASSWORD` 登录。学生用邀请码自行注册，只需昵称和密码，不收集邮箱或真实姓名。

邀请码有两种：

- **一次性邀请码**：管理员在「管理 → 邀请码」批量生成（每批 1–100 个，可加备注），每个码只能注册一个账号，用过即失效，表里能看到谁用了哪个码。未用的码可以作废。注册失败（比如昵称被占用）不会消耗码。格式如 `CF2V-XCYN`，不含 0/O/1/I/L，输入时不区分大小写、可省略横线。
- **公用邀请码**：`.env` 里的 `INVITE_CODE`，不限次数。留空则只能用一次性邀请码注册；两者都没有时注册关闭。修改后运行 `docker compose up -d` 生效。

不用 Docker 时运行 `cargo run --release`，环境变量同 `.env.example`。离线开发用 `FEED=sim cargo run`，价格为随机游走。

### nginx 反向代理

对外需要 HTTPS，并且要转发 WebSocket：

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

如果 nginx 也跑在 Docker 里，把两者放进同一个 Docker 网络，`proxy_pass` 指向 `http://papertrade:8080`。

### 备份

所有数据在 Docker 卷 `papertrade-data` 里的一个 SQLite 文件中。建议在赛季重置前备份一次：

```bash
docker compose exec papertrade sh -c 'cat /data/papertrade.db' > backup.db
```

## 撮合模型

| 情形 | 处理方式 |
|---|---|
| 市价单、可立即成交的限价单 | 按真实 L2 盘口逐档成交，滑点来自真实深度；收吃单费 |
| 被学生吃掉的流动性 | 在 `LIQUIDITY_TTL_MS` 内不能被再次吃到 |
| 挂出的限价单 | 排在该价位真实挂单量之后（`queue_ahead`）。真实成交消耗完前方队列后才轮到它；前方撤单会让队列前移；可部分成交；收挂单费 |
| 真实成交价穿过挂单价 | 按挂单价成交，数量不超过那笔真实成交的量 |
| 一笔真实成交 | 在多个学生的挂单之间按价格、时间优先分配，不会重复使用 |
| 盘口越过挂单（跳空或断线重连后） | 按挂单价成交 |
| 止损、止盈 | 由真实最新成交价触发，触发后按市价或限价执行 |
| 延迟 | 订单被接受后 50–200 ms 才到达盘口，按那一刻的盘口成交 |
| 行情中断或超过 `STALE_MS` 无数据 | 暂停接单和撮合，恢复后自动重新同步盘口 |
| 交易规则 | 最小下单量、价格精度、数量精度取自 Kraken 的 instrument 数据 |

订单簿每次更新都用 Kraken 的 CRC32 校验和核对，不一致就重新同步。

**模拟的固有局限**：学生的单子不会真的推动市场价格，也没有真实的对手方排在他们后面。初始资金 10 万美元、交易主流币种时，这个误差很小；资金量设得越大、币种越冷门，结果越偏乐观。

## 账本

所有余额变动都写入复式账本（`ledger` 表）。每笔成交、手续费、资金发放和赛季重置都是一组合计为零的分录。管理页的「账本对账」检查三件事：各资产总和为零，每个余额等于其流水之和，冻结金额等于未完成订单的占用。

## 排行榜和赛季

排行榜显示收益率、最大回撤、收益/回撤比、夏普比率（需 6 天以上数据）和成交订单数。净值曲线对俱乐部成员公开，持仓和订单不公开。

管理员可以结束赛季：归档排名，撤销全部订单，所有人重置为初始资金。

## API（给写机器人的学生）

在「账户」页创建 API 密钥，所有请求带 `Authorization: Bearer <key>`。订单字段命名参照 Kraken（`limit_price`、`trigger_price`、`time_in_force`、`post_only`），鉴权方式简化为 Bearer 令牌。完整示例在「账户 → API 快速上手」。

| 接口 | 用途 |
|---|---|
| `POST /api/orders` | 下单，返回 `pending`，结果通过 WebSocket 推送或查询得到 |
| `GET /api/orders`、`?status=closed`、`/{id}` | 当前委托、历史委托、单个订单 |
| `DELETE /api/orders/{id}`、`DELETE /api/orders` | 撤单、全部撤单 |
| `GET /api/portfolio`、`/api/fills` | 资产、成交 |
| `GET /api/book`、`/api/trades`、`/api/candles`、`/api/tickers`、`/api/instruments` | 行情 |
| `WS /ws` | 先发 `{"op":"auth","token":…}`，再发 `{"op":"subscribe","symbol":…}` |

订单类型：`market`、`limit`、`stop-loss`、`take-profit`、`stop-loss-limit`、`take-profit-limit`。限价单支持 `gtc`、`ioc`、`fok` 和 `post_only`。

`bots/trend_bot.py` 是一个完整的示例机器人（只用 Python 标准库）：趋势回调时挂单买入，带止盈、止损、保本止损、总亏损上限、到点清仓，状态写盘、进程崩溃后能接着跑，结束时生成报告。`bots/run_bot.sh` 负责在崩溃后自动重启它。

## 开发

```bash
cargo test          # 撮合、账本、Kraken 消息解析与校验和
FEED=sim ADMIN_PASSWORD=adminpass123 INVITE_CODE=test cargo run
ADMIN_PASSWORD=adminpass123 python3 smoke_test.py   # 对运行中的实例做端到端检查
```

`smoke_test.py` 只用 Python 标准库，约 3 秒跑完 27 项检查：行情、WebSocket、各类订单与冻结资金、账号隔离、权限、一次性邀请码、API 密钥、账本对账。只需要 `ADMIN_PASSWORD`。它使用专用账号 `smoke-a` / `smoke-b` / `smoke-c`（首次运行时用自己生成的一次性邀请码注册），结束时卖回持仓并禁用这些账号，所以不会出现在排行榜上。`deploy.sh` 部署完会在服务器上自动运行它（`SMOKE=0 ./deploy.sh` 跳过）。

| 文件 | 内容 |
|---|---|
| `src/engine.rs` | 撮合模拟与账本 |
| `src/book.rs` | 订单簿镜像、已消耗流动性、校验和 |
| `src/feed.rs` | Kraken 行情接入、模拟行情 |
| `src/api.rs` | REST 和 WebSocket 接口 |
| `src/accounts.rs` | 用户、会话、API 密钥、一次性邀请码 |
| `src/candles.rs` | K 线 |
| `web/` | 前端（中英双语） |

K 线图使用 [TradingView Lightweight Charts™](https://www.tradingview.com/)（Apache-2.0）。
