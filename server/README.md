# cindy-server

自用的最小 Cindy 后端：邮箱验证码登录加设备互联中继，SQLite 存储。不含模型网关，也不做会话历史云同步。

## 运行

```sh
cargo build --release --manifest-path server/Cargo.toml
CINDY_SERVER_ALLOWED_EMAILS=you@example.com \
CINDY_SERVER_ADDR=127.0.0.1:8787 \
CINDY_SERVER_DB=./cindy-server.db \
  server/target/release/cindy-server
```

| 环境变量 | 说明 |
| --- | --- |
| `CINDY_SERVER_ALLOWED_EMAILS` | **必填**。逗号分隔的允许登录邮箱，不在名单里的邮箱拿不到验证码。 |
| `CINDY_SERVER_ADDR` | 监听地址，默认 `127.0.0.1:8787`。 |
| `CINDY_SERVER_DB` | SQLite 文件，默认 `./cindy-server.db`。 |

## 登录方式

没有邮件通道。客户端请求验证码后，6 位验证码会打印在服务端的 stderr 里（10 分钟有效，同一邮箱 30 秒内只能请求一次，错 5 次作废）。你在服务端日志里看到验证码，填进客户端即可。

## 公网使用

服务端只讲 HTTP/WebSocket，**不做 TLS**。客户端只接受 `https://` 地址，或回环地址的 `http://`。要让手机或另一台电脑访问，放在 Caddy、nginx 等反向代理后面，由它提供 HTTPS，并转发 WebSocket（`/api/device-link/ws`）。

## 客户端配置

任选其一，环境变量优先：

- 环境变量 `CINDY_SERVER_URL=https://your.host`
- 数据目录下 `server.json`：`{"url": "https://your.host"}`（macOS 数据目录是 `~/Library/Application Support/com.cindy.tauri/`）

配置后登录和设备互联都走这个地址；模型网关仍指向官方地址（本服务不提供）。地址不合法时客户端只记日志并回到官方地址。改完需要重启应用。

## 接口

- `GET /api/auth/providers`、`POST /api/auth/discovery`、`POST /api/auth/sso/discovery`
- `POST /api/auth/email/request-code`、`POST /api/auth/email/verify-code`
- `POST /api/auth/refresh`（刷新令牌一次性，轮换后旧的失效）、`POST /api/auth/logout`
- `GET /api/device-link/devices`、`GET /api/device-link/ws`（WebSocket 中继）

## 测试

```sh
cargo test --manifest-path server/Cargo.toml          # 登录、轮换、验证码规则、中继路由、账号隔离
node scripts/server-e2e.mjs                           # 用上游 DeviceLinkClient 走真实进程
# 客户端完整 device-link e2e（建链、可靠传输、撤销）对着本服务跑：
CINDY_SERVER_BIN=$PWD/server/target/debug/cindy-server cargo test --release --offline device_link::e2e   # 在 src-tauri 下
```
