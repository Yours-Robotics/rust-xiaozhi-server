# rust-xiaozhi-server

参考 `xiaozhi-esp32-server` 的接口形态，提供 Rust 原型服务（标准库实现）。

## 已实现接口

- `GET /health`
- `POST /api/v1/device/register`
- `GET /xiaozhi/v1/`（WebSocket）
- `POST /api/ota/`（兼容 `/xiaozhi/ota/`）

## WebSocket 协议（v1）

连接请求头（必需）：

- `Authorization: Bearer <access_token>`
- `Protocol-Version: 1`
- `Device-Id: <MAC>`
- `Client-Id: <UUID>`

握手后支持：

- 客户端 `hello`，服务端返回 `hello`（`sample_rate=24000`）
- `listen start/stop` 最小响应
- `mcp` 消息回显 payload
- 二进制帧（OPUS）回传（二进制 echo，作为 TTS 音频占位）
- ping/pong 控制帧

## OTA 接口

`POST /api/ota/` 读取请求头与 JSON 体，返回：

- `activation`
- `mqtt`
- `websocket`
- `server_time`
- `firmware`

策略：

- `bread-*` 且版本 `0.9.8` 时强制升级到 `1.0.2`
- 其余按语义版本比较，低于 `1.0.2` 返回升级链接

## 运行

```bash
cargo run
```

默认监听 `0.0.0.0:8080`，可通过环境变量覆盖：

```bash
RUST_XIAOZHI_BIND=0.0.0.0:9000 cargo run
```

## 校验

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
