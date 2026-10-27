# echo-server-core

内网穿透 / 反向隧道端口映射工具。把内网机器上的服务安全地暴露到公网服务器，
**内网侧不需要公网 IP，也不需要开放任何入站端口**。

```
                         ┌──────────────────────────────────────────┐
公网用户 ───────────────►│  Hub（有公网 IP 的服务器）                │
  :8080                  │                                          │
  :2222                  │  ① 在 :4433 接受 spoke 的 mTLS 连接      │
                         │  ② 每个 [[service]] 开一个公网监听端口    │
                         │  ③ 按配置决定哪些服务允许被暴露           │
                         └───────────────────┬──────────────────────┘
                                             │
                              单条 mTLS 连接，yamux 多路复用
                              （内网主动连出来，反向穿 NAT）
                                             │
                         ┌───────────────────▼──────────────────────┐
                         │  Spoke（内网机器）                        │
                         │  收到流 → 送到 127.0.0.1:3000 等本地服务  │
                         └──────────────────────────────────────────┘
```

也可以加一层 **Web 面板**：`esc-host` 用插件框架把隧道内核（进程内插件）和
面板（子进程插件）接起来，从此可以在浏览器里增删端口映射、看流量、踢掉某个
spoke，**不需要重启**。面板与内核之间只有一份 JSON 契约，没有编译期耦合——
面板挂掉、被换掉都不影响转发。详见 [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
与 [`docs/PANEL.md`](docs/PANEL.md)。

## 设计要点

| 决策 | 原因 |
|---|---|
| **Hub 决定暴露什么，Spoke 无权决定** | Spoke 只能在 `Hello` 里"声明"它能提供哪些服务名；是否真的有公网端口，由 Hub 自己的 `[[service]]` 配置决定。内网机器被攻破也无法自行开公网端口。 |
| **mTLS（双向证书）** | Hub 验 Spoke 的客户端证书，Spoke 验 Hub 的服务端证书，同一私有 CA 签发。比预共享 token 更强，且可单独吊销某台机器。 |
| **单连接 + yamux 多路复用** | 不论多少并发连接、多少个服务，Hub 与 Spoke 之间只有**一条** TCP+TLS 连接。NAT 只需维持一条会话，也避免了每连接三次握手。 |
| **ALPN 固定 `esc/1`** | 协议版本在 TLS 握手阶段就协商，版本不匹配的话连接根本建立不起来，不会出现"连上了但互相看不懂"的情况。 |
| **无 TUN / 无路由改动** | 纯用户态字节转发，不创建虚拟网卡，不需要 root，不影响宿主机路由表。 |

## 快速开始

### 1. 编译

```bash
cargo build --release      # 产物: target/release/esc，单个二进制约 2.9 MB
```

### 2. 生成证书（在能安全保存 CA 私钥的机器上做一次）

```bash
esc pki init \
  --out /etc/esc \
  --host esc.example.com \      # hub 证书的 SAN，填客户端实际连接用的域名或 IP
  --host 203.0.113.10 \
  --spoke home-nas \            # 每台内网机器一个，可重复
  --spoke raspberry-pi
```

产出：

```
ca.pem     ca.key      ← CA 私钥只留在这台机器，绝不外发
hub.pem    hub.key     ← 拷到 Hub
home-nas.pem  home-nas.key   ← 拷到对应 Spoke
```

后续再加一台机器（`pki init` 在同一台机器上生成全部材料时用）：

```bash
esc pki spoke --ca-dir /etc/esc --id new-machine --out ./new-machine-cert
```

### 2b. 多机部署：CSR 流程（推荐）

`pki init` 会把所有私钥生成在同一台机器上，适合"单机组装后分发"。
**当各机器之间无法直接拷贝文件时（比如只能通过人工/跳板转文本），用 CSR 流程：
私钥在各自机器上生成，永不离开本机，只有公开的 CSR 与证书在传输。**

```bash
# --- 在每一台「需要证书的机器」上（hub 和每台 spoke 各一次）---
esc pki keygen --out /etc/esc/req --id hub      # 产出 hub.key(0600) + hub.csr
esc pki keygen --out /etc/esc/req --id home-nas

# --- 把 <id>.csr 拷到持 CA 的机器，在那里签名 ---
esc pki sign --ca-dir /root/esc-ca --csr hub.csr      --id hub \
             --kind server --host esc.example.com --host 203.0.113.10
esc pki sign --ca-dir /root/esc-ca --csr home-nas.csr --id home-nas --kind client

# --- 把签好的 <id>.pem 拷回原机器，与本地私钥放在一起 ---
```

**签名方决定一切**：CSR 里声明的 keyUsage / EKU / SAN 全部被忽略，
证书的用途、SAN、有效期由 `esc pki sign` 的参数决定。这样即使某台机器
自己生成了"看起来像 hub"的 CSR，也拿不到 serverAuth 证书。

### 3. Hub

```bash
esc check -c /etc/esc/hub.toml --role hub     # 预检，不绑定任何端口
esc hub   -c /etc/esc/hub.toml
```

### 4. Spoke

```bash
esc check -c /etc/esc/spoke.toml --role spoke
esc spoke -c /etc/esc/spoke.toml
```

## 配置

完整示例见 `examples/hub.toml` 与 `examples/spoke.toml`。

**Hub**

```toml
[hub]
listen = "0.0.0.0:4433"
reject_when_absent = true      # 无 spoke 提供服务时立即关闭公网连接

[tls]
ca_cert = "/etc/esc/ca.pem"
cert    = "/etc/esc/hub.pem"
key     = "/etc/esc/hub.key"

[[service]]
name   = "grafana"             # 必须与 spoke 侧 name 完全一致
listen = "0.0.0.0:8080"        # 公网监听端口
# spoke = "home-nas"           # 可选：锁定到某台 spoke
```

**Spoke**

```toml
[spoke]
id     = "home-nas"
server = "esc.example.com:4433"

[tls]
ca_cert = "/etc/esc/ca.pem"
cert    = "/etc/esc/home-nas.pem"
key     = "/etc/esc/home-nas.key"

[[service]]
name  = "grafana"
local = "127.0.0.1:3000"
```

## Web 面板（可选）

```bash
cargo build --release \
  --bin esc --bin esc-host --bin esc-plugin-tunnel-panel

# 引擎配置：旧的 hub.toml 直接能用，加两行即可
#   role  = "hub"
#   store = "/etc/esc/mappings.toml"
esc-host -c /etc/esc/host.toml
```

`/etc/esc/host.toml`：

```toml
[engine]
config = "/etc/esc/engine.toml"

[panel]
enabled = true
command = "/usr/local/libexec/esc/esc-plugin-tunnel-panel"

[panel.config]
listen = "127.0.0.1:8787"      # 默认只绑回环
auth   = "admin:换成你的口令"    # 绑非回环地址时必填，否则插件拒绝启动
title  = "隧道面板"
```

打开 `http://127.0.0.1:8787/` 就能：

| 能做 | 说明 |
|---|---|
| 增删改映射 | 公网端口**立即**绑定 / 释放，不重启、不断已有连接 |
| 看运行状态 | 每映射的连接数、活跃连接、上下行字节；spoke 列表与已授权服务 |
| 钉住 provider | 一条映射只能由指定 spoke 提供，防止被别的机器顶替 |
| 踢掉某个 spoke | 断开后它按退避重连并重新握手（用于让新映射生效） |
| 导出静态配置 | 生成可直接喂给 `esc hub` / `esc spoke` 的 TOML |

安全默认：没配 `auth` 时只允许监听回环地址；映射端口才是对公网开放的东西，
面板本身不应该直接暴露（需要外网访问请用 SSH 隧道或前置反代）。

## 部署

`deploy/` 下有加固好的 systemd 单元（`NoNewPrivileges`、`ProtectSystem=strict`、
`MemoryDenyWriteExecute`、syscall 白名单、`MemoryMax=256M` 等）。

```bash
install -m755 target/release/esc /usr/local/bin/esc
useradd --system --no-create-home --shell /usr/sbin/nologin esc
install -d -m750 -o root -g esc /etc/esc
# 放入证书与配置（key 600，pem 644）
install -m644 deploy/esc-hub.service /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now esc-hub
```

## 观测

```bash
journalctl -u esc-hub -f
esc hub -c hub.toml --log debug          # 含每条连接的方向与字节数
esc hub -c hub.toml --log-json           # JSON 结构化日志
```

Hub 侧连接日志示例：

```
INFO  spoke connected  peer=198.51.100.7:51234 spoke=home-nas agent=esc/0.1.0
                       granted=["grafana","nas-ssh"]
DEBUG connection finished service=grafana peer=203.0.113.5:60011 up=78 down=168
```

## 已验证行为

以下均在本地环回做了端到端实测（非仅单元测试）：

| 项目 | 结果 |
|---|---|
| TLS 1.3 + mTLS 握手 | ✅ ALPN 协商为 `esc/1` |
| HTTP 完整穿越（含响应头） | ✅ 200 + 目标服务的自定义头 |
| 8 MB 单流二进制完整性 | ✅ SHA-256 一致 |
| 64 并发连接 × 64 KB | ✅ 64/64 完整 |
| 半关闭（客户端 `shutdown(WR)`） | ✅ 仍能收到完整响应 |
| 空闲 45s / 75s 后再开新流 | ✅ 正常（跨越多个 keepalive 周期） |
| 用**其他 CA** 签发的客户端证书 | ✅ 被拒（TLS handshake failed） |
| **无客户端证书**的裸 TLS | ✅ 被拒（`alert 116 certificate required`） |
| ALPN 不匹配 | ✅ 被拒（`alert 120 no_application_protocol`） |
| Spoke 声明的未授权服务 | ✅ 被拒，且不产生任何公网监听 |
| Spoke 进程被杀 → Hub 注销 | ✅ 探测立即失败 |
| Spoke 重启自动重连 | ✅ 0.10s 恢复 |
| Hub 重启 → Spoke 自动重连 | ✅ 0.40s 恢复 |
| 常驻内存 | Hub 5.4 MB / Spoke 5.3 MB |

测试共 245 个，`cargo test --workspace` 全绿（`cargo clippy` 0 warning）。除了原有的协议 / 配置 / PKI 单测，
新增部分覆盖了这次改造的关键承诺：

| 用例 | 断言的事 |
|---|---|
| `esc-engine` 集成测试 | spoke 已连接时**动态**加映射能被立即授权、数据真的穿过去、停用后端口立即释放 |
| `esc-plugin-tunnel` 测试 | 插件 mount 后 Ctx 里出现隧道服务；配置非法时 fail-closed |
| `esc-plugin-tunnel-panel` HTTP 测试 | 每个 REST 端点映射到正确的契约方法；URL 里的 id 覆盖 body；口令缺失即 401 |
| `esc-plugin-tunnel-panel` 端到端测试 | 浏览器 API → 子进程面板 → HostCall → 宿主 → 进程内插件 → 引擎 → spoke → 回显服务，全链路真实流量 |

## 已知边界（当前版本故意不做）

- **仅 TCP**。UDP 映射（DNS / QUIC / 游戏）未实现。
- **无 HTTP 层能力**。不做按 `Host` 头分流、不做 TLS 终止、不管路径。
  做这些请在前面放 nginx/caddy。
- **单 Hub**。没有 Hub 之间的高可用与状态同步；Hub 是单点。
- **无权流量统计 / 限速**。目前只有日志里的字节数。
- **一个服务一个端口**。同一个 public 端口不能按域名分给多个 spoke。

## 开发

```
crates/
  esc-common/        配置、TLS、线协议、yamux 驱动、PKI
  esc-engine/        可编程内核：动态端口绑定 / 注册表 / 映射表 / 计数管道
  esc-control-api/   控制面契约（DTO + 方法名 + 信封 + TunnelControl trait）
  esc-plugin-tunnel/ 进程内插件：把引擎注册进宿主的 Ctx
  esc-plugin-tunnel-panel/  子进程插件：axum + 单文件前端，只认契约
  esc-host/          宿主：Ctx + 插件挂载 + HostCall 服务 + 面板管理
  esc-hub/           公网侧 CLI 薄封装（行为与改造前一致）
  esc-spoke/         内网侧 CLI 薄封装（行为与改造前一致）
  esc-cli/           单个二进制 esc，子命令 hub / spoke / check / pki
  esc-host(二进制)   esc-host，宿主入口
```

分层原则只有一个：**依赖方向永远指向 `esc-control-api` / `esc-common`**。
面板不 import 引擎，引擎不 import 面板，宿主两边都不 import 实现细节。

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo build --release
```

### 关于 yamux 0.14

`yamux` 0.14 移除了旧版的 `Control` 句柄，也改用了 **`futures::io`** 而不是 tokio
的 I/O trait。因此 `esc-common::mux` 自己做了一个 driver：一个后台任务独占
`Connection`，把「open 请求」「入站流」「socket 就绪」三件事用 `poll_*` 手工多路
复用起来。hub/spoke 两侧通过 `tokio_util::compat` 在两种 I/O trait 之间转换。

## License

MIT

- [`docs/SERVER-PANEL.md`](docs/SERVER-PANEL.md) —— 服务器面板：主机指标 + Docker 容器管理

- [`docs/GATEWAY.md`](docs/GATEWAY.md) —— 认证网关：面板前面的反向代理（含 Docker 部署）
