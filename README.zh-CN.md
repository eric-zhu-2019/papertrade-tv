# Paper Exchange — 俱乐部加密货币模拟交易所

[English](README.md) | 中文

真实行情（Kraken 公开数据）加虚拟资金。学生的订单不会发到任何真实交易所，而是在本服务器内对着 Kraken 的实时订单簿和成交流模拟撮合。不需要任何交易所账户或 API 密钥。

两类市场：8 个现货交易对（BTC/USD 等），以及对应的 8 个永续合约（BTC-PERP 等，行情来自 Kraken Futures）。现货也可以像 Kraken 一样用杠杆交易，借币做多或做空。BTC 和 ETH 有期权，由本服务器定价。合约可以做空、加杠杆，有资金费率和强平。

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

部署到自己的服务器（需要能 `ssh` 登录、装有 Docker）：

```bash
./deploy.sh my-server          # 或 DEPLOY_HOST=my-server ./deploy.sh
```

脚本把代码同步到 `~/apps/papertrade`，首次运行时生成随机的管理员密码和邀请码写入远程 `.env`，构建并启动容器，最后跑冒烟测试。之后再运行不会覆盖 `.env` 和数据库。

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

## 现货做空（杠杆现货）

和 Kraken 的杠杆交易一样：现货订单带上 `leverage`（2 倍及以上）就不再交割币，而是在这个交易对上开一个仓位，所以杠杆卖出就是做空，不需要持有这个币。订单和普通现货单对着同一个真实盘口撮合。

| 项目 | 规则 |
|---|---|
| 仓位 | 每个交易对一个净仓位：反向的杠杆单先减仓，多出的部分反手 |
| 杠杆 | 跟 Kraken 一致，按交易对和方向取最高倍数（AssetPairs 的 `leverage_buy` / `leverage_sell`，默认 8 个交易对都是 10 倍）。Kraken 不提供杠杆的交易对不能用杠杆交易 |
| 抵押 | 现货 USD（简化：Kraken 上其他币也算抵押）。仓位会占住初始保证金（开仓价值 ÷ 杠杆）和自己的浮动亏损，这部分 USD 不能用来买现货或划转到合约 |
| 费用 | 现货手续费，加上 Kraken 的开仓费（按开仓价值收）和每 4 小时一次的展期费（`MARGIN_ROLLOVER_SECS`），费率都是所借货币在 Kraken Assets 里的 `margin_rate`：做多借 USD（0.025%），做空借币（BTC 0.01%，其他 0.02%） |
| 保证金水平 | 权益（现货 USD + 杠杆仓位浮动盈亏）÷ 占用保证金。低于 Kraken 的追加保证金线（80%）时下单表单会提示 |
| 强平 | 到 Kraken 的强平线（40%）时，所有杠杆仓位按真实盘口市价平仓（先撤掉占用 USD 的挂单），收 0.5% 清算费进保险基金；亏损超过 USD 的部分由保险基金承担 |
| 平仓 | 「平仓」标签或 `POST /api/margin/close` 发一笔只减仓的市价单 |

## 期权

Kraken 没有期权，所以期权由本服务器自己定价：BTC 和 ETH 的欧式期权，用 USD 现金结算。目前学生只能买入看涨（call）和看跌（put）期权，以及卖出已经持有的期权，不能卖出开仓（写期权）。

| 项目 | 规则 |
|---|---|
| 合约 | 代码沿用 Deribit 的格式，例如 `BTC-30OCT26-80000-C`；1 张 = 1 个币（BTC 最小 0.01 张，ETH 0.1 张） |
| 到期日 | UTC 08:00 到期：未来 3 天每天一个、未来 4 个周五、未来 3 个月每月最后一个周五。行权价 BTC 每 1000 美元一档、ETH 每 50 美元一档，现价上下各 10 档 |
| 定价 | Black-Scholes（利率取 0），标的价格是 Kraken 现货中间价，波动率是 Kraken 过去 30 天小时收盘价的历史波动率（每小时更新） |
| 报价 | 模拟做市商：买价 = 理论价 ×（1 − `OPT_SPREAD`），卖价 = 理论价 ×（1 + `OPT_SPREAD`），默认 3%，单边点差至少是现货价的 0.01%。市价单立即全部成交（每单最多 25 张 BTC / 250 张 ETH）；限价挂单在报价碰到挂单价时按挂单价成交 |
| 手续费 | 跟 Deribit 一样：每张收标的价格的 0.03%，最多不超过权利金的 12.5% |
| 结算 | 到期时按最后 30 分钟的 Kraken 现货均价现金结算；该到期日上未成交的挂单自动撤销 |
| 记账 | 持有的期权记成一种现货资产（名字就是合约代码），所以会出现在「资产」里，带成本均价，按理论价计入净值 |

`OPTIONS=0` 可以关掉期权。新赛季时期权按理论价折成现金。

## 永续合约

合约对着 Kraken Futures 的真实永续合约盘口和成交撮合（`PF_XBTUSD` 等，WebSocket v1）。撮合规则和现货相同，只是成交后不交割币，而是开仓、加仓、减仓或反手。

| 项目 | 规则 |
|---|---|
| 合约 | 1 张 = 1 个币，USD 结算；每个市场一个净持仓（单向持仓），反向下单先减仓，多出的部分反手 |
| 资金账户 | 单独的合约钱包，从现货划转 USD 进来才能开仓；未实现盈利要平仓后才能转出 |
| 杠杆 | 跟 Kraken 一致：BTC、ETH、SOL 最高 100 倍，其他 50 倍；仓位越大，Kraken 的保证金档位要求越高，可用杠杆随之降低 |
| 保证金 | 逐仓（每个仓位单独的保证金，最多亏掉这部分）或全仓（合约钱包里的钱共同担保所有全仓仓位），按市场设置；有仓位或挂单时不能切换模式 |
| 追加 / 减少保证金 | 只对逐仓：追加让强平价远离；减少后仓位仍要有当前杠杆下的初始保证金（浮动盈利不算） |
| 平仓手续费 | 平仓（含只减仓单）不需要预先冻结手续费，成交时从平仓返还的钱里扣；挂单成交时若要开仓但保证金不够，会被撤单，不会开出没有保证金的仓位 |
| 持仓调杠杆 | 逐仓：调低时，保证金低于「开仓价值 ÷ 新杠杆」才补足（手动追加的保证金保留）；调高时，最多释放到与「减少保证金」相同的下限。全仓：只改变占用的保证金，调高总是允许，调低时可用余额不够则拒绝。该市场的挂单按新杠杆重新冻结。`POST /api/futures/settings` 加 `"dry_run": true` 只返回调整前后的保证金和强平价，不做修改 |
| 行情断开时 | 有仓位的市场不能调杠杆、不能追加或减少保证金；有仓位时不能把钱从合约账户转出 |
| 手续费 | Kraken Futures 入门费率：挂单 0.02%，吃单 0.05% |
| 标记价格 | 用 Kraken 的 mark price 计算浮动盈亏、保证金和强平 |
| 资金费率 | 每小时按 Kraken 当期费率结算：费率为正时多头付给空头，为负时反过来；逐仓从仓位保证金里扣，全仓从钱包里扣 |
| 强平 | 权益跌到维持保证金以下时，按真实盘口市价平仓，并按仓位价值收 0.5% 清算费（`LIQ_FEE`，我们自己定的，未核实 Kraken 的费率）进保险基金；亏损超过保证金的部分由保险基金承担，账户不会变成负数 |
| 只减仓 | `reduce_only` 订单只能减少或平掉仓位 |

合约行情断开时暂停合约下单、撮合和强平，现货不受影响。`FUTURES=0` 可以关掉合约。新赛季时所有仓位按标记价格结算，合约钱包清零。

## 账本

所有余额变动都写入复式账本（`ledger` 表）。每笔成交、手续费、资金发放和赛季重置都是一组合计为零的分录。杠杆现货不需要额外账户：盈亏用现货 USD 对 `market` 结算，开仓费、展期费和清算费记入 `fees` 和 `insurance`。合约另有合约钱包（`fut:用户`）、逐仓保证金（`posm:仓位`）、资金费（`funding`）和保险基金（`insurance`）账户。管理页的「账本对账」检查：各资产总和为零，每个余额等于其流水之和，冻结金额等于未完成订单的占用，每个逐仓仓位的保证金等于其流水。

## 排行榜和赛季

排行榜显示收益率、最大回撤、收益/回撤比、夏普比率（需 6 天以上数据）和成交订单数。净值曲线对俱乐部成员公开，持仓和订单不公开。

管理员可以结束赛季：归档排名，撤销全部订单，所有人重置为初始资金。

## API（给写机器人的学生）

在「账户」页创建 API 密钥，所有请求带 `Authorization: Bearer <key>`。期权下单也用 `POST /api/orders`，`symbol` 填期权代码（市价或限价；买入，或卖出已持有的）。杠杆现货下单是普通现货单加 `"leverage": 2`（或更高），可加 `reduce_only`；保证金账户、一键平仓和仓位历史在 `/api/margin*`。合约下单用同一个 `POST /api/orders`，`symbol` 填 `BTC-PERP`，可加 `reduce_only`；合约账户、划转、杠杆设置、追加保证金、一键平仓、仓位历史和资金费记录在 `/api/futures/*`。订单字段命名参照 Kraken（`limit_price`、`trigger_price`、`time_in_force`、`post_only`），鉴权方式简化为 Bearer 令牌。完整示例在「账户 → API 快速上手」。

| 接口 | 用途 |
|---|---|
| `POST /api/orders` | 下单，返回 `pending`，结果通过 WebSocket 推送或查询得到 |
| `GET /api/orders`、`?status=closed`、`/{id}` | 当前委托、历史委托、单个订单 |
| `DELETE /api/orders/{id}`、`DELETE /api/orders` | 撤单、全部撤单 |
| `GET /api/portfolio`、`/api/fills` | 资产、成交 |
| `GET /api/margin`、`POST /api/margin/close`、`GET /api/margin/positions` | 杠杆现货：仓位、保证金水平和 Kraken 的规则；平仓；历史 |
| `GET /api/options/chain?underlying=BTC&expiry=`、`/api/options/quote?symbol=` | 期权：某个到期日的行权价和买卖报价；单个合约 |
| `GET /api/book`、`/api/trades`、`/api/candles`、`/api/tickers`、`/api/instruments` | 行情 |
| `WS /ws` | 先发 `{"op":"auth","token":…}`，再发 `{"op":"subscribe","symbol":…}` |

订单类型：`market`、`limit`、`stop-loss`、`take-profit`、`stop-loss-limit`、`take-profit-limit`。限价单支持 `gtc`、`ioc`、`fok` 和 `post_only`。

`bots/trend_bot.py` 是一个完整的示例机器人（只用 Python 标准库）：趋势回调时挂单买入，带止盈、止损、保本止损、总亏损上限、到点清仓，状态写盘、进程崩溃后能接着跑，结束时生成报告。`bots/run_bot.sh` 负责在崩溃后自动重启它。

## 开发

```bash
cargo test          # 撮合、账本、杠杆现货、期权、合约、Kraken 消息解析与校验和
FEED=sim ADMIN_PASSWORD=adminpass123 INVITE_CODE=test cargo run
ADMIN_PASSWORD=adminpass123 python3 smoke_test.py   # 对运行中的实例做端到端检查
```

`smoke_test.py` 只用 Python 标准库，约 5 秒跑完 38 项检查：行情、WebSocket、各类订单与冻结资金、2 倍杠杆做空、期权买卖、合约（划转、杠杆、逐仓开多、追加保证金、持仓调杠杆、全仓做空、只减仓、平仓）、账号隔离、权限、一次性邀请码、API 密钥、账本对账。只需要 `ADMIN_PASSWORD`。它使用专用账号 `smoke-a` / `smoke-b` / `smoke-c`（首次运行时用自己生成的一次性邀请码注册），结束时卖回持仓并禁用这些账号，所以不会出现在排行榜上。`deploy.sh` 部署完会在服务器上自动运行它（`SMOKE=0 ./deploy.sh` 跳过）。

| 文件 | 内容 |
|---|---|
| `src/engine.rs` | 撮合模拟与账本 |
| `src/book.rs` | 订单簿镜像、已消耗流动性、校验和 |
| `src/feed.rs` | Kraken 现货行情接入、模拟行情（含合约） |
| `src/kfeed.rs` | Kraken Futures 行情接入、合约规则、合约 K 线 |
| `src/margin.rs` | 杠杆现货：仓位、占用的 USD、展期费、强平 |
| `src/options.rs` | 期权：到期日、Black-Scholes 报价、下单、结算 |
| `src/futures.rs` | 合约：钱包、仓位、保证金、资金费、强平 |
| `src/api.rs` | REST 和 WebSocket 接口 |
| `src/accounts.rs` | 用户、会话、API 密钥、一次性邀请码 |
| `src/candles.rs` | K 线 |
| `web/` | 前端（中英双语） |

K 线图使用 [TradingView Lightweight Charts™](https://www.tradingview.com/)（Apache-2.0）。

## 许可证

本项目以 [Apache License 2.0](LICENSE) 发布。
