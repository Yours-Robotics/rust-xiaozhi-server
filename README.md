# rust-xiaozhi-server

参考 `xiaozhi-esp32-server` 的接口形态，提供了一个 Rust 原型服务：

- `GET /health`：健康检查与会话数量。
- `POST /api/v1/device/register`：设备注册，返回 `session_id` 与 websocket 地址。
- `GET /ws/:session_id`：真实 WebSocket 握手与最小信令响应。

## 当前 WebSocket 能力

- 支持标准握手（`Sec-WebSocket-Accept`）。
- 接收文本帧：
  - 当消息包含 `"type":"ping"`，返回 `{"type":"pong"}`。
  - 其他文本消息返回 `{"type":"ack"}`。
- 支持 Ping/Pong 控制帧。
- 连接关闭后会回收对应 session。

> 说明：当前实现基于 Rust 标准库，便于在受限环境快速运行与验证。

## 运行

```bash
cargo run
```

默认监听 `0.0.0.0:8080`，可通过环境变量覆盖：

```bash
RUST_XIAOZHI_BIND=0.0.0.0:9000 cargo run
```

## 示例

```bash
curl -X POST http://127.0.0.1:8080/api/v1/device/register \
  -H 'Content-Type: application/json' \
  -d '{"device_id":"esp32-001"}'
```

```bash
curl http://127.0.0.1:8080/health
```

## 校验

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
