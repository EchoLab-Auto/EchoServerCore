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

## CSR 流程（多机部署）

模拟两台机器，验证"私钥不出本机"的路径：

```
$ esc pki keygen --out machine-a --id hubA
key      machine-a/hubA.key  (0600, must stay on this host)
csr      machine-a/hubA.csr  (public; carry it to the CA machine)

$ esc pki sign --ca-dir ca --csr machine-a/hubA.csr --id hubA \
               --kind server --host 127.0.0.1 --host localhost --out machine-a
cert     machine-a/hubA.pem  (CN esc-hub, 3650 days)
         SANs: 127.0.0.1, localhost

$ openssl verify -CAfile ca/ca.pem machine-a/hubA.pem
machine-a/hubA.pem: OK
EKU: TLS Web Server Authentication
SAN: IP Address:127.0.0.1, DNS:localhost
```

关键点已验证：CSR 里**不带** SAN/EKU，证书里的 SAN 与 EKU 完全来自
`esc pki sign` 的参数 —— 请求方无法给自己要到 serverAuth 或额外 SAN。

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

## 生产环境验证（2026-10-25，公网实链路）

链路：公网用户 → `114.132.241.46:2222`（Hub, cloud-server-2）→ mTLS+yamux →
`evence-4090-24`（Spoke，家里/机房 4090）→ `127.0.0.1:8080`（EchoAgent Panel）。

拓扑跨公网、经过至少两个运营商，与环回测试不是一个量级。

| 用例 | 结果 |
|---|---|
| 全球探针 TCP 2222（6 节点） | ✅ 6/6 连通 |
| 全球探针 HTTP（hu/pl/ru/sg/us 5 节点） | ✅ 5/5 `HTTP 200` |
| 静态资源从 id/it/jp 抓取 | ✅ 3/3 `HTTP 200` |
| 262 KB JS 字节一致性（直连 vs 隧道） | ✅ SHA-256 相同 |
| 20 并发 × 262 KB | ✅ 20/20 |
| 浸泡 60 s / 8 并发 | ✅ 1232 次全成功，308 MiB |
| 浸泡 150 s / 16 并发 | ✅ 3840 次全成功，961 MiB |
| Hub 重启恢复 | ✅ 734 ms |
| Spoke 重启恢复 | ✅ 立即重连（`connection closed by hub, reconnecting`） |
| 公网链路延迟 | 首页 0.44–0.53 s（跨洲探针），本机 0.03 s |
| fd 泄漏 | Hub 13 → 13，Spoke 11（无增长） |
| 内存 | Hub 6164 → 6768 kB，Spoke 7.7 MB（见下方说明） |

**内存说明**：Hub 在累计传输约 1.27 GB / 5000+ 请求后 RSS 增长约 600 kB。
这个量级更像是 glibc arena 的正常行为而非泄漏（fd 完全稳定，且增长不随
请求数线性放大），但**只有几分钟的观测，不能断言长时间无泄漏**。

## ⚠️ 生产环境发现的安全问题：Panel 的 API 无认证

映射打通后探测 Panel 的接口，发现 `GET /api/upstreams` **不需要任何认证**
就返回完整的上游清单，包括内网 IP、公网 IP + 端口、各节点在线状态：

```json
[{"name":"ai_station","url":"ws://10.10.2.250:3132","online":true, ...},
 {"name":"A100-40g","url":"ws://223.109.239.30:17176","online":true, ...},
 {"name":"cloud-server-2","url":"ws://114.132.241.46:3132","online":true, ...}]
```

用 check-host.net 从西班牙/伊朗/荷兰三个节点确认，外部访问同样返回 `HTTP 200`。

这意味着**任何知道这个端口的人都能枚举整套基础设施清单**。此外该端口是
**明文 HTTP**，链路上的任何人都能看到内容。

这不是 esc 的缺陷（esc 只负责把流量送过去，它不解析 HTTP），而是
「把只该内网用的管理面板直接暴露到公网」这个决定本身的后果。

建议的处置（按推荐顺序）：

1. **不要把面板直接映射到公网**。面板本来是给 EasyTier overlay 内网用的，
   需要时通过 `10.10.10.100:8080` 访问即可。
2. 若确实需要公网访问，至少在前面加一层认证 + TLS：
   Caddy/nginx 反代 + Basic Auth + 自动 HTTPS，不要让 esc 直接对公网暴露 HTTP。
3. 无论如何都应把 Panel 的 `bind_address` 改成 `127.0.0.1:8080`，
   这样它只能经隧道访问，无法被 overlay 内的其它机器直连。

## 吞吐上限定位：瓶颈是云主机带宽，不是 esc

第一次测 262 KB 资源时耗时在 0.19 ~ 3.35 s 之间抖动，单连接复用 10 次
2.5 MiB 用 5.50 s ≈ **466 KiB/s**。为判断是 esc 还是网络，做了分段测量：

| 测量点 | 结果 |
|---|---|
| 4090 → Cloudflare 上行 | 19.8 MB/s（158 Mbps） |
| 4090 ← Cloudflare 下行 | 23.2 MB/s |
| cloud-server-2 ← Cloudflare 下行 | 1.77 MB/s（14 Mbps） |
| **cloud-server-2 → Cloudflare 上行** | **0.45 MB/s（3.6 Mbps）** |
| esc 隧道端到端 | 0.47 MB/s（466 KiB/s） |

数据流向是 `4090 → cloud-server-2 → 客户端`，出 Hub 走的是 cloud-server-2
的**上行**，而它的上行被腾讯云限速在约 3.6 Mbps —— 与隧道实测的 466 KiB/s
（≈ 477 KB/s）几乎完全吻合。

**结论：esc 没有成为瓶颈，它把可用的 3.6 Mbps 跑满了。** 实例型号
`S5.MEDIUM2`，这类按带宽计费的国内云主机通常是 1~5 Mbps 档。

想提高公网访问速度只能换路径：
1. 加钱升 cloud-server-2 的带宽（国内云贵）；
2. 或用 EasyTier overlay 直连 4090（p2p 不经 Hub），但要求客户端也装 EasyTier；
3. 或把 Hub 放在带宽更好的机器上（境外 VPS 通常 100 Mbps 起）。

临时偶发的 3.35 s 单次尖峰出现在与压力测试并行时，串行复测延迟稳定在
0.026–0.036 s，判断为带宽争用而非缺陷。

## 插件化改造后的测试（2026-10-25）

改造引入了「运行时增删映射」这一新能力，以及两个新的进程边界。测试按
**能证明什么** 组织，而不是按 crate 组织：

| 层 | 文件 | 证明了什么 |
|---|---|---|
| 引擎单测 | `crates/esc-engine/src/{store,config}.rs` | 映射表 CRUD / 冲突校验 / 原子落盘；旧 hub.toml、spoke.toml 仍能解析；内联 `[[mapping]]` 优先于 `[[service]]` |
| 引擎集成 | `crates/esc-engine/tests/runtime_mapping.rs` | **spoke 已连接时加映射 → 现有连接被立即授权**（不需要重连）→ 数据穿过隧道 → 停用后端口释放 → 删除后记录消失 |
| 插件 | `crates/esc-plugin-tunnel/src/lib.rs` | mount 后 `Ctx` 里出现 `esc.tunnel.control`；配置非法时挂载 fail-closed；引擎启动失败时调用方拿到明确错误而不是挂住 |
| 面板 HTTP | `crates/esc-plugin-tunnel-panel/tests/http_api.rs` | 每个 REST 端点对应正确的契约方法；URL 里的 `id` 覆盖 body；Basic 认证缺失即 401 且**不会**触达后端；非回环 + 无口令被配置校验拒绝 |
| 端到端 | `crates/esc-plugin-tunnel-panel/tests/e2e_host.rs` | 真子进程面板 + 真宿主 + 真引擎 + 真 spoke：HTTP 建映射 → 端口绑定 → 字节往返；宿主退出后面板进程消失 |
| 隔离性 | `crates/esc-host/tests/isolation.rs` | 面板二进制指向不存在的路径时，宿主**不退出**、隧道引擎仍就绪、进程持续运行（「面板挂了不影响隧道」这条承诺的回归测试） |
| 契约稳定性 | `crates/esc-plugin-tunnel-panel/tests/http_api.rs::error_code_table_lives_in_the_contract` | `ErrorKind::as_code` ↔ `from_code` 往返、未知码归 `Other`（向前兼容）、`http_status` 不退化 |

手工冒烟（release 二进制）：

```bash
# CLI 回归：esc hub / esc spoke 行为不变
esc pki init --out ./certs --host 127.0.0.1 --spoke s1
esc check -c hub.toml --role hub && esc check -c spoke.toml --role spoke
esc hub -c hub.toml & esc spoke -c spoke.toml & curl http://127.0.0.1:<svc>/

# 宿主 + 面板：运行时加映射，curl 直接穿透
esc-host -c host.toml &
curl -su admin:口令 -X POST http://127.0.0.1:8787/api/mappings \
  -H 'content-type: application/json' -d '{"name":"web","listen":"127.0.0.1:PORT","enabled":true}'
curl http://127.0.0.1:PORT/          # → 后端内容
```

实测结果（本机，release）：

| 项 | 结果 |
|---|---|
| `cargo test --workspace` | 107 passed / 0 failed |
| `cargo clippy --workspace --all-targets` | 0 warning |
| release 二进制体积 | `esc` 3.5 MB / `esc-host` 3.6 MB / `esc-plugin-tunnel-panel` 1.7 MB（含 unwind 表，见下） |
| 运行时加映射 → 数据可达 | ✅ 200，`providers` 立即出现现有 spoke |
| 流量计数 | ✅ 单次请求记到 78 B 上行 / 207 B 下行 |
| 停用映射 → 端口释放 | ✅ 后续 connect 被拒 |
| 映射表落盘 | ✅ 0600，`[[mapping]]` 结构，重载可用 |

### panic 策略的实测

`[profile.release]` 从 `panic = "abort"` 改成 `unwind` 后，用同一段代码
（一个任务 panic、一个任务正常干活）在两种 profile 下各跑一次：

```
=== unwind（现在）===
坏任务的 JoinHandle 结果: Err（已被隔离）
健康任务仍在运行: 0 1 2 3 4
进程存活到结束 ✅                    exit=0

=== abort（改造前）===
（没有任何输出）                     exit=134  Aborted (core dumped)
```

`abort` 下连健康任务的输出都没打印出来 —— 一个连接的 bug 会带走整个 hub。
理由与体积代价见 [`ARCHITECTURE.md`](./ARCHITECTURE.md)。
