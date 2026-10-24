# 验证记录

本文档记录 **实际执行过的** 验证，而不是"应该能工作"的推断。
所有测试在 2026-10-25 于一台 Ubuntu 24.04 / 20 核 / 64 GB 的开发机上完成，
被测二进制为 `cargo build --release` 产物（2.9 MB, glibc ≥ 2.34）。

测试拓扑全部在本地环回，端口刻意避开常用端口：

```
curl/openssl ──► Hub :18081(svc notexposed)
                 Hub :18080(svc echo) ──yamux over TLS1.3──► Spoke ──► 127.0.0.1:19000
                 Hub :14433(control)
```

## 单元测试

```
$ cargo test --workspace
esc-common: 14 passed
esc-hub:     4 passed
esc-spoke:   1 passed
```

覆盖：配置校验（重名/端口冲突/控制端口占用）、SAN 分类、证书签发与
PEM 重载、帧编解码（含超大帧拒绝与 EOF 语义）、`host:port` 解析（含 IPv6
字面量）、mux 双向流、注册表（重连不误删、pin 到指定 spoke、失效条目剪枝）、
退避抖动范围。

## 功能

| 用例 | 方法 | 结果 |
|---|---|---|
| HTTP 完整穿越 | `curl -i http://127.0.0.1:18080/` | ✅ `200`，响应体与自定义头 `X-Served-By` 都来自目标 |
| 8 MB 单流完整性 | POST 8 MiB 随机数据，比对目标返回的 SHA-256 | ✅ 一致，0.02 s |
| 64 并发 × 64 KiB | 64 线程并发 POST，逐一比对 SHA-256 | ✅ 64/64 完整，1.01 s |
| 半关闭 | `sendall` 后 `shutdown(SHUT_WR)`，继续读 | ✅ 收到完整 168 字节响应 |
| 空闲 45 s 后再开新流 | 跨 2 个 control-ping（20 s）周期 | ✅ 正常 |
| 空闲 75 s 后再开新流 | 跨 3 个周期 | ✅ 正常 |
| 常驻内存 | `VmRSS` | Hub 5.4 MB / Spoke 5.3 MB |

> 说明：首次做 keep-alive 复用测试时出现 FAIL，排查后确认是目标服务
> `http.server` 的 `protocol_version` 默认 `HTTP/1.0`（每请求关闭连接），
> 与隧道无关。换成"空闲后开新流"才是有意义的隧道层验证。

## 安全边界

| 攻击方式 | 结果 |
|---|---|
| 用**另一个 CA** 签发的客户端证书 | ✅ Hub 拒绝：`TLS handshake failed` |
| **不带客户端证书**的裸 TLS 连接 | ✅ Hub 拒绝：`alert 116 certificate required` |
| ALPN 声明成 `wrong/9` | ✅ 拒绝：`alert 120 no_application_protocol` |
| Spoke 声明 Hub 未配置的服务名（`rogue`） | ✅ 拒绝：`not exposed by this hub`，且 Hub 不为它开任何公网端口 |
| 访问 Hub 上无 Spoke 提供服务的端口（`18081`） | ✅ 立即关闭，日志 `no spoke currently serves this service` |

## 故障恢复

| 场景 | 恢复耗时 |
|---|---|
| Spoke 进程被 `SIGKILL` | Hub 立即注销（`control stream closed by peer` → `spoke disconnected`），探测转为失败 |
| Spoke 重启 | **0.10 s** 后服务恢复 |
| Hub 进程被 `SIGKILL` 后重启 | **0.40 s** 后服务恢复（Spoke 退避重连自动完成） |

Spoke 退避序列实测：`200 ms → 400 ms → 800 ms …` 上限 2000 ms，带 ±20% 抖动，
日志可见 `attempt=3` 失败后约 0.7 s 发起 `attempt=4` 并成功。

## 证书链的独立验证

不依赖本项目代码，用 `openssl` 交叉验证：

```
$ openssl verify -CAfile ca.pem hub.pem
hub.pem: OK
$ openssl verify -CAfile ca.pem nas.pem
nas.pem: OK

hub 证书  SAN: IP Address:127.0.0.1, DNS:localhost
         EKU: TLS Web Server Authentication
spoke 证书 EKU: TLS Web Client Authentication
```

## 未验证 / 已知缺口

以下 **没有** 被测试过，使用前请自行评估：

- **真实跨公网链路**：以上全部在环回完成，未测试高延迟、丢包、
  MTU 不一致、中间设备切断长连接等情形。
- **长时间运行**：最长连续运行约 10 分钟，没有数天级别的浸泡测试，
  因此无法断言无内存泄漏或 fd 泄漏。
- **大规模并发**：最高 64 并发连接，未做数千连接压测，
  也未测量 `max_streams=512` 达到上限时的行为。
- **UDP**：本版本不支持，未测。
- **证书轮换 / 吊销**：只验证了"换 CA 后旧证书失效"（因为根本不同 CA），
  没有实现 CRL/OCSP。
