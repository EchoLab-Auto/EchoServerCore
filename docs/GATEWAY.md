# esc-gateway —— 面板前面的认证反向代理

在面板（或任何 HTTP 服务）前面加一道认证。默认场景是把 `2223`（隧道面板）
和 `2224`（服务器面板）的认证统一到一处。

```
浏览器 ──▶ esc-gateway :2223 / :2224 / …  ──认证──▶ 127.0.0.1:18787（隧道面板）
                                                   127.0.0.1:18788（服务器面板）
```

## 为什么需要它

面板自带的 Basic 认证有三个不足：

| 问题 | 说明 |
|---|---|
| 口令反复上链路 | Basic 把 `user:pass` 做 base64 放每个请求头里。base64 是编码不是加密 |
| 口令明文存配置 | `/etc/esc/host.toml` 里直接写着 `auth = "admin:pw"` |
| 无法按端口授权 | `2223` 只能看映射，`2224` 能启停/删除 Docker 容器（≈ root），却是同一套凭据 |

网关把这三件事收拢到一处：**服务端会话 + argon2id 哈希 + 按 route 的 ACL**。

## 快速开始

### 方式 A：二进制

```bash
# 1. 生成口令哈希（从标准输入读，不进 shell 历史）
esc-gateway hash-password
#   从标准输入读取口令（输入不回显）：
#   $argon2id$v=19$m=19456,t=2,p=1$...$...

# 2. 写配置
sudo install -d /etc/esc
sudo tee /etc/esc/gateway.toml >/dev/null <<'EOF'
title = "esc 网关"

[[user]]
name = "admin"
hash = "$argon2id$v=19$m=19456,t=2,p=1$...$..."     # 上一步的输出
routes = ["*"]

[[route]]
name = "tunnel-panel"
listen = "0.0.0.0:2223"
upstream = "127.0.0.1:18787"

[[route]]
name = "server-panel"
listen = "0.0.0.0:2224"
upstream = "127.0.0.1:18788"
EOF
sudo chmod 644 /etc/esc/gateway.toml

# 3. 校验配置（不绑定任何端口）
esc-gateway check -c /etc/esc/gateway.toml

# 4. 起服务
esc-gateway -c /etc/esc/gateway.toml
```

### 方式 B：Docker

```bash
# 1. 构建镜像（国内需要镜像源，见下）
docker build \
  --build-arg CARGO_MIRROR="sparse+https://rsproxy.cn/index/" \
  --build-arg GIT_PROXY="https://gh-proxy.com/https://github.com/" \
  -t esc-gateway:latest .

# 2. 生成口令哈希
docker run --rm -it esc-gateway:latest hash-password

# 3. 写配置（同方式 A）+ 校验
docker run --rm -v /etc/esc/gateway.toml:/etc/esc/gateway.toml:ro \
  esc-gateway:latest check -c /etc/esc/gateway.toml

# 4. 起服务
docker compose up -d
```

`docker compose` 不存在的机器（Ubuntu 26.04 的 `docker.io` 包不带 compose
插件）用等价命令：

```bash
docker run -d --name esc-gateway \
  --network host \
  -v /etc/esc/gateway.toml:/etc/esc/gateway.toml:ro \
  -e RUST_LOG=info \
  --read-only --tmpfs /tmp:size=1m \
  --cap-drop ALL --security-opt no-new-privileges:true \
  --restart unless-stopped \
  esc-gateway:latest
```

## Docker 部署的三个硬约束

### ① 必须 host 网络

网关要转发到**宿主机** `127.0.0.1:18787`。用默认 bridge 网络时，容器里的
`127.0.0.1` 指的是容器自己，转发必然连不上（表现为 502 + 日志里
`upstream connect failed: Connection refused`）。

| 方案 | 说明 |
|---|---|
| `--network host` ← 推荐 | 最简单、无 NAT 开销。代价是网关端口直接占宿主端口 |
| `extra_hosts: host.docker.internal:host-gateway` | 然后把 `upstream` 改成 `host.docker.internal:18787`，且面板要绑 `0.0.0.0` |
| docker 自定义网络 + 服务名 | 要求上游也跑在同一个 compose 里 |

用 host 网络时，**面板必须让开 2223/2224**、退到 18787/18788 之类的端口
（见下面「迁移步骤」）。

### ② 配置文件必须让容器用户可读

镜像是 `USER 65532:65532`（非 root）。宿主上 `/etc/esc/gateway.toml` 若是
`600 root`，容器读不到：

```
Error: 读取 /etc/esc/gateway.toml 失败: Permission denied (os error 13)
```

```bash
chmod 644 /etc/esc/gateway.toml
```

（配置里有口令**哈希**而不是明文，644 是可接受的。真要收严就用 `640` 并把
组设成 65532，但容器里没有对应用户名，所以按 uid 设组比较别扭。）

### ③ 健康检查不能用 shell 管道

镜像里没有 `curl`，`CMD-SHELL` 写的管道会失败。用网关自己的子命令：

```yaml
healthcheck:
  test: ["CMD", "/usr/local/bin/esc-gateway", "health", "-c", "/etc/esc/gateway.toml"]
```

`health` 会真的向配置里第一个 route 的端口发一个 `GET /__auth/healthz`
并检查 200。比 `--version` 有意义——后者只证明进程活着。

### 构建时需要网络

workspace 里有 git 依赖（`plugin-framework`），即使 `esc-gateway` 不用它，
`cargo fetch` 也会去 clone。国内构建要传两个 build-arg：

| build-arg | 用什么 | 说明 |
|---|---|---|
| `CARGO_MIRROR` | `sparse+https://rsproxy.cn/index/` | crates.io 镜像 |
| `GIT_PROXY` | `https://gh-proxy.com/https://github.com/` | github 代理（配 `insteadOf`） |

实测（腾讯云 2 vCPU / 2 GB）：冷构建约 **3 分钟**，其中 Rust 编译 1m20s。

**基础镜像选型**：用了 `debian:bookworm-slim` 而不是 `distroless`。原因是
实测 `gcr.io` 在国内直连不通（i/o timeout）、daocloud 的 gcr 代理 403、
ghcr 也 denied，而 `docker.io` 的 debian 走 `registry-mirrors` 能拉。
镜像大 20 MB，换来一个能构建的 Dockerfile。想换 distroless 的话改
`FROM` 一行即可，前提是那台机器拉得到。

## 配置

| 键 | 默认 | 说明 |
|---|---|---|
| `title` | `esc 网关` | 登录页标题 |
| `session_ttl_hours` | `8` | 会话存活时间。进程重启会让全部会话失效 |
| `cookie_name` | `esc_gw` | cookie 名 |
| `max_sessions` | `4096` | 会话表上限，达到后淘汰最久未使用的 |
| `trust_forwarded_for` | `false` | **保持 false**：那个头谁都能伪造 |
| `secure_cookie` | `false` | 有 TLS 终结时打开 |
| `[rate_limit] max_failures` | `8` | 单来源失败上限 |
| `[rate_limit] fail_window_secs` | `60` | 单来源窗口 |
| `[rate_limit] max_failures_global` | `200` | **全局**失败上限（跨来源） |
| `[rate_limit] global_window_secs` | `300` | 全局窗口 |

### 用户

```toml
[[user]]
name = "admin"
hash = "$argon2id$..."        # 必须 argon2id，明文会被拒绝
routes = ["*"]                # 或 ["tunnel-panel", "server-panel"]
```

`hash` 不是 argon2 格式时**拒绝启动**并提示怎么生成。这是最容易犯、后果最重
的配置错误——写明文等于把口令写进了备份、日志与 git 历史。

### 路由

```toml
[[route]]
name = "tunnel-panel"              # 用户 ACL 引用这个名字
listen = "0.0.0.0:2223"            # 对外
upstream = "127.0.0.1:18787"       # 上游
allowed_cidrs = []                 # 可选：额外的来源白名单（与认证是「与」）
strip_prefix = "/panel"            # 可选：转发前剥掉路径前缀
max_body_bytes = 1048576           # 可选：默认 10 MB
```

校验会拦住这些：

- 多个 route 用同一 `listen`（端口冲突应该在启动时暴露，不是访问时）
- `upstream == listen`（转发环路，上线后是 CPU 100% 且难定位）
- 用户引用了不存在的 route 名（错误信息会列出已定义的名字）
- `allowed_cidrs` 解析失败（**不能**"解析不了就当没配"——那会静默开给所有人）
- `strip_prefix` 缺前导 `/`

## 认证行为

### 401 还是 302

这是最容易让人用着难受的地方。判定规则（按可靠度）：

1. `Sec-Fetch-Mode: navigate` → 浏览器导航 → **302 到登录页**
2. `Accept` 含 `text/html` 且不含 `application/json` → **302**
3. 其余 → **401 JSON**

为什么不能只看 `Accept`：面板的 `fetch()` 默认 `Accept: */*`，不是
`application/json`。判错一次，前端 `JSON.parse` 会拿到一坨登录页 HTML，
然后报一个完全无关的解析错误。

`Sec-Fetch-Mode` 是现代浏览器一定会带的，比 Accept 可靠得多。

### 已登录但没权限 = 403

`401` 表示「没登录」，`403` 表示「登录了但你不能访问这个端口」。分开是
为了让使用者知道该做什么：前者去登录，后者去找管理员。

### 会话

| 项 | 做法 | 为什么 |
|---|---|---|
| 存储 | 内存 `HashMap` | 重启即失效（安全默认），不用管密钥轮换 |
| id | 32 字节 CSPRNG，Base64URL | 43 字符，防猜测 |
| cookie | `HttpOnly; SameSite=Strict; Path=/` | 见下 |
| 登录后 | **重建 session id** | 防会话固定 |
| 跨进程 | 不支持 | 单实例部署，YAGNI |

`SameSite=Strict` 意味着浏览器在任何跨站请求里都不带这个 cookie，于是
**CSRF 从根上不成立**——所以没有 CSRF token 却仍然安全。另外加一道
`Origin`/`Host` 一致性校验作为纵深防御（万一将来有人为了嵌 iframe 改成
Lax/None，那层还在）。

### 限速

两层，都必须有：

- **按来源**：同一 IP 在窗口内失败 N 次 → 429。挡住单点爆破。
- **全局**：跨所有来源的总失败数达上限 → 429。挡住**分布式**爆破。
  对方有 1000 个 IP，只做按来源限速时每 IP 试 8 次就是 8000 次。

预算用完后**正确口令也会被挡**（60 秒后恢复）。否则限速可以靠"继续试探"绕过。

登录成功后清**源**计数，但**不清全局**计数——否则攻击者只要有一个自己的
账号就能反复解锁全局闸。

### 不泄露账号是否存在

用户名不存在时，仍然跑一次哈希校验（用硬编码的 dummy 哈希），让响应时间与
真实用户相同；错误信息也完全一致（同样的文案、同样的状态码）。

## 迁移步骤（从面板 Basic 切到网关）

停机窗口约 10 秒。**按顺序做**，不要先改面板——那会让 2223/2224 短暂无认证。

```bash
# ① 面板退到回环的高位端口，并去掉自己的 auth
#    /etc/esc/host.toml
#      [panel.config]
#      listen = "127.0.0.1:18787"        ← 原 0.0.0.0:2223
#      auth = ""                          ← 删掉这一行（认证交给网关）
#    面板的 fail-closed 检查仍会拒绝「非回环 + 无认证」，所以必须是回环。

# ② 服务器面板同样处理
#    /etc/esc/server-panel.toml
#      listen = "127.0.0.1:18788"

# ③ 起网关（配置里 listen 用 2223/2224）
esc-gateway check -c /etc/esc/gateway.toml       # 先校验
systemctl restart esc-host esc-server-panel      # 面板退到回环
systemctl enable --now esc-gateway               # 网关接 2223/2224

# ④ 验证
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:2223/    # 期望 302
curl -s -H 'Accept: */*' http://127.0.0.1:2223/api/status          # 期望 401
```

回滚同样 10 秒：停网关 → 面板改回 `0.0.0.0:2223` + `auth` → 重启。

### 端口冲突要提前想清楚

网关和面板**不能同时占 2223**。所以顺序是「面板先让开，网关再上」。
用 Docker 时这一点更容易踩：host 网络下端口是共享的，容器起不来会一直重启。

## 明确不支持

| 不支持 | 原因 |
|---|---|
| WebSocket / Upgrade | 面板用不到，而支持它要处理连接升级、协议切换、双向流复制——占整个代理一半复杂度。遇到 `Upgrade` 头不转发（剥掉），请求会失败 |
| HTTP/2 | 上下游都是 h1。面板不需要多路复用 |
| TLS 终结 | 需要域名 + 证书管理。在前面放 nginx/Caddy 更合适，然后打开 `secure_cookie` |
| gzip 重压/解压 | 原样透传 `Accept-Encoding` 与 `Content-Encoding` |
| 重定向跟随 | 代理不该跟随，那是客户端的事 |
| 动态路由（API 加 route） | 配置必须重启才生效：认证拓扑要能在重启这个动作里被完整替换 |

## 排障

```bash
# 配置对不对（不绑定端口）
esc-gateway check -c /etc/esc/gateway.toml

# 网关活着吗（这个端点不需要认证，供探针）
curl -i http://127.0.0.1:2223/__auth/healthz

# 容器里
docker logs esc-gateway
docker inspect esc-gateway --format '{{.State.Health.Status}}'
```

| 现象 | 多半是 |
|---|---|
| 502 + 日志 `upstream connect failed` | 上游没起 / 地址写错 / Docker 用了 bridge 网络 |
| 502 + `无法连接上游` 但上游在跑 | 容器里 `127.0.0.1` 指容器自己 → 换 host 网络 |
| 一直跳登录页 | `secure_cookie = true` 但在用 http:// 访问（浏览器不存 Secure cookie） |
| 登录成功但仍 401 | cookie 域名/路径不匹配；或中间有反代改写了 `Host` |
| 面板的 fetch 报 JSON 解析错误 | 401/302 判定错了（看该请求的 `Sec-Fetch-Mode`） |
| 容器起不来，`Permission denied` | 配置文件权限（见「硬约束 ②」） |
| 429 一直不恢复 | 全局计数没清（设计如此）。等 `global_window_secs` 过去 |

## 测试

62 个测试（43 单测 + 19 端到端）。端到端用真实 TCP + 真实上游，覆盖：

- 302/401 判定（含 `Sec-Fetch-Mode` 与 Accept 的各种组合）
- 登录 → 转发 → 登出失效的完整链路
- **hop-by-hop 头被剥掉**（含 `Connection` 点名的）
- **session cookie 不转发给上游**
- Host 重写、XFF 覆盖（客户端伪造的被丢弃）
- 来源白名单（403，且连登录页都不给）
- 限速（含"正确口令也被挡"）
- 账号是否存在不泄露（两种失败的响应体逐字节相同）
- 上游不可达 → 502、请求体超限 → 413
- 开放重定向中和、路径前缀重写
