# Paper Exchange（俱乐部加密货币模拟交易所）

给高中俱乐部用的现货模拟交易所：Kraken 真实行情 + 虚拟资金，订单在本服务内对着真实盘口模拟撮合。用户说中文，请用中文回复。架构和撮合模型见 README.md。

## 常用命令

- `cargo test`：17 个单元测试（撮合、账本、一次性邀请码、Kraken 消息解析与校验和）
- `FEED=sim ADMIN_PASSWORD=adminpass123 INVITE_CODE=test cargo run`：本地用随机模拟行情运行，http://127.0.0.1:8080
- `./deploy.sh [ssh主机] [远程目录]`：部署到 `<ssh主机>` 的 `~/apps/papertrade`。不会覆盖远程 `.env` 和数据库卷；部署完自动在服务器上跑冒烟测试（`SMOKE=0` 跳过）
- `python3 smoke_test.py [URL] [--env-file .env]`：对运行中的实例做 27 项端到端检查，只需要 `ADMIN_PASSWORD`。改了 API 或撮合后要同步更新它

## 部署状态

- Kraken 真实行情已实测：WebSocket、订单簿校验和、K 线 REST 都正常。
- 静态文件带 `Cache-Control: no-cache`，重新部署后浏览器会立刻拿到新的 JS/CSS。

## 环境注意事项

- 局域网内是明文 HTTP。以后若要对外网开放，改为 `PT_BIND=127.0.0.1` 并由 nginx 提供 HTTPS（需转发 WebSocket，配置见 README）。

## 设计约定

- 金额一律用 `rust_decimal`，不用浮点；所有余额变动必须写复式账本（`Engine::transfer`），改动撮合后跑 `cargo test` 并确认 `reconcile()` 无差错。
- 引擎是 `Arc<Mutex<Engine>>`，锁内不能 `await`。
- 第一期只做现货；合约（杠杆、资金费率、强平）是计划中的第二期，尚未开始。
