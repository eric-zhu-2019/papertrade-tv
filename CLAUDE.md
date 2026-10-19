# Paper Exchange（俱乐部加密货币模拟交易所）

给高中俱乐部用的模拟交易所（现货 + 永续合约）：Kraken 真实行情 + 虚拟资金，订单在本服务内对着真实盘口模拟撮合。用户说中文，请用中文回复。架构和撮合模型见 README.md（英文）和 README.zh-CN.md（中文），两份内容要同步更新。

## 常用命令

- `cargo test`：50 个单元测试（撮合、账本、一次性邀请码、杠杆现货做空/展期费/强平、期权日历/定价/买卖/结算、合约的保证金/反手/资金费/强平/调杠杆/追加保证金、Kraken 现货与合约消息解析）
- `FEED=sim ADMIN_PASSWORD=adminpass123 INVITE_CODE=test cargo run`：本地用随机模拟行情运行，http://127.0.0.1:8080
- `./deploy.sh <ssh主机> [远程目录]`：部署到 `<ssh主机>` 的 `~/apps/papertrade`（也可用环境变量 `DEPLOY_HOST` 指定主机）。不会覆盖远程 `.env` 和数据库卷；部署完自动在服务器上跑冒烟测试（`SMOKE=0` 跳过）
- `python3 smoke_test.py [URL] [--env-file .env]`：对运行中的实例做 38 项端到端检查（含杠杆现货、期权和合约），只需要 `ADMIN_PASSWORD`。改了 API 或撮合后要同步更新它

## 部署

- 部署主机、局域网地址、服务器环境和部署记录在 `CLAUDE.local.md`（已加入 .gitignore，不会提交）。这类信息只写在那里，不要写进 CLAUDE.md、README 或代码。
- 局域网内是明文 HTTP。若要对外网开放，改为 `PT_BIND=127.0.0.1` 并由 nginx 提供 HTTPS（需转发 WebSocket，配置见 README）。
- 静态文件带 `Cache-Control: no-cache`，重新部署后浏览器会立刻拿到新的 JS/CSS。

## 设计约定

- 金额一律用 `rust_decimal`，不用浮点；所有余额变动必须写复式账本（`Engine::transfer`），改动撮合后跑 `cargo test` 并确认 `reconcile()` 无差错。
- 引擎是 `Arc<Mutex<Engine>>`，锁内不能 `await`。
- 第二期永续合约（`src/futures.rs`、`src/kfeed.rs`）：Kraken Futures 真实行情，杠杆跟 Kraken 一致，逐仓和全仓，单独的合约钱包。合约成交走 `fut_apply_fill`，不碰现货余额；改动后跑 `cargo test` 和冒烟测试，并确认对账无差错（合约钱包、逐仓保证金、挂单冻结都在对账里）。
- 杠杆现货（`src/margin.rs`）：现货单带 `leverage`（≥2）就开仓位而不交割币，规则取自 Kraken AssetPairs/Assets。只用现货 USD 作抵押，仓位的保证金和浮亏通过 `Engine::free()` 从可用 USD 里扣出（不单独记账）；杠杆单冻结的是 USD。成交走 `margin_apply_fill`，改动后同样要跑测试、冒烟测试并确认对账。
- 期权（`src/options.rs`）：Kraken 没有期权、Deribit 在加拿大不能用，所以由本服务器用 Black-Scholes 定价（Kraken 现货 + Kraken 30 天历史波动率），点差模拟做市商。只能买入和卖出已持有的期权。持有的期权就是名字为合约代码的现货余额，复用现货账本和对账。
