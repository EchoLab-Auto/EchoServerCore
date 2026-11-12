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

## 网关自己的端点

网关自己不渲染页面（除了登录页）。它提供三样东西：

| 路径 | 内容 | 形态 |
|---|---|---|
| `/__auth/login` | 登录表单 | HTML（单文件内联） |
| `/__auth/logout` | 退出 | 302 |
| `/__auth/` | 我的会话：谁、能去哪些端口 | **JSON** |
| `/__admin/api/*` | 账号 / 端口 / 会话管理 | **JSON**（见下面「管理面」） |

`/__auth/` 从前是一页 HTML 首页（「我是谁、我能去哪儿」）。合并之后它只回 JSON：
那份"我能去哪儿"的清单现在由壳子自己画（导航页 + 端口列表），网关不必再渲染
一遍。清单仍然只列**你这个账号有权访问**的端口——没权限的连名字都不出现，
否则「这台机器开了哪些面板」就成了公开信息。

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

### ④ 想在容器里用管理面板，账号库必须可写

compose 里 `read_only: true` + 配置文件 `:ro`，容器里**没有任何地方**能落盘。
于是 `/__admin/` 会退化成只读视图（列表能看、按钮是灰的），账号只能从
`gateway.toml` 来。要用面板就挂一个可写卷，并注意目录属主是镜像里的 uid：

```bash
install -d -o 65532 -g 65532 /var/lib/esc-gateway
```

```yaml
volumes:
  - /var/lib/esc-gateway:/var/lib/esc-gateway
```

```toml
users_file = "/var/lib/esc-gateway/gateway-users.toml"
```

不是必须的：接受只读面板也完全能用（多账号就在 `gateway.toml` 里手写）。

### 构建时需要网络

workspace 里有 git 依赖（`echo-fabric-arch`），即使 `esc-gateway` 不用它，
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
| `users_file` | 配置文件同目录的 `gateway-users.toml` | 面板管理的账号存在这里。目录不可写时面板自动降级为只读 |
| `routes_file` | 配置文件同目录的 `gateway-routes.toml` | 面板管理的端口（代理目标）存在这里。同样可降级为只读 |
| `admin_listen` | 不设 | 管理面板（含登录页）独占一个端口，如 `127.0.0.1:2226`。不设时挂在每个转发端口的 `/__admin/` 上 |
| `admin_allowed_cidrs` | `[]` | 只允许这些来源访问 `/__admin/`。空 = 不限 |
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
admin = true                  # 允许进管理面板；默认 false
```

`hash` 不是 argon2 格式时**拒绝启动**并提示怎么生成。这是最容易犯、后果最重
的配置错误——写明文等于把口令写进了备份、日志与 git 历史。

### 路由

```toml
[[route]]
name = "tunnel-panel"              # 用户 ACL 引用这个名字
listen = "0.0.0.0:2223"            # 对外
upstream = "127.0.0.1:18787"       # 上游
host = "tunnel.example.com"        # 可选：只在 Host 命中时走这条
allowed_cidrs = []                 # 可选：额外的来源白名单（与认证是「与」）
strip_prefix = "/panel"            # 可选：转发前剥掉路径前缀
max_body_bytes = 1048576           # 可选：默认 10 MB
```

`listen` 是"在哪开门"，`upstream` 是"门后交给谁"。每个 `[[route]]` 都要占一个
`listen` —— 除非用 `host`，见下。

#### 一个监听地址服务多个上游

`host` 填上之后，**多条 route 可以共用同一个 `listen`**：

```toml
[[route]]
name = "panel"; host = "panel.example.com"; listen = "0.0.0.0:8080"
upstream = "127.0.0.1:18788"

[[route]]
name = "cad";   host = "cad.example.com";   listen = "0.0.0.0:8080"
upstream = "127.0.0.1:2228"

[[route]]
name = "rest";                              listen = "0.0.0.0:8080"
upstream = "127.0.0.1:2229"
```

这样接一个新服务不用多占一个端口。为什么值得：从前的形态是每个服务一份
`listen`，于是「在这里拦一道认证」也要专门开一个门 —— cloud2 上就堆出了
`18784`（网关门）→ `18788`（面板本尊）两份地址，而 `18784` 的全部作用只是认证。

查表顺序是**精确命中优先，兜底最后**：

1. `Host` 与某条路由的 `host` 全等 → 那条（大小写不敏感，端口被剥掉：
   `Host: a.com:8080` 与 `a.com` 是同一条）；
2. 否则用这个端口上**不带 `host`** 的那条（兜底）；
3. 都没有 → `421 Misdirected Request`，响应体里**列出这个端口上配了哪些 host**。

第 3 步绝不"随便挑一条转发"。那样配错了不报错，只会把请求送给错误的服务 ——
表现是「页面打不开」或者「登录了但没权限」，而日志里一切正常。

约束只有两条：同一端口上最多**一条**不带 `host` 的（一个兜底），以及（端口 +
host）不能重复。`host` 不支持 `*` 通配 —— 精确匹配让"哪个请求去了哪个上游"
一眼可读；真要泛域名，DNS 里加 `*.example.com`，路由这边仍写具体名字。

> 按 Host 分流与按**路径**分流是两回事。路径分流改不了响应里 HTML/JS 的绝对
> 路径（`<script src="/ui/server.js">`），所以只适合 API-only 的服务；面板这类
> 有前端的应用必须用 Host。

面板里保存时还会多拦一条 `gateway.toml` 那种逐条校验拦不住的：**上游指向本网关
自己的监听端口**（含管理端口）。那不是"配置写错"，而是自环——最轻的表现是一次莫名
其妙的 401（流量被转回网关自己、要求二次认证），最坏是打满一个核心。

`esc-gateway check` / 启动校验会拦住这些：

- 多个 route 用同一 `listen` **且会命中的 Host 有交集**（同端口本身不冲突——那正是
  host 分流的意义；冲突的是两条都不写 host，或写了同一个 host）
- `upstream == listen`（转发环路，上线后是 CPU 100% 且难定位）
- 用户引用了不存在的 route 名（错误信息会列出已定义的名字）
- `allowed_cidrs` 解析失败（**不能**"解析不了就当没配"——那会静默开给所有人）
- `strip_prefix` 缺前导 `/`

## 隔离网段（主机内部）

没有网段时，网关的前门只能写在**能连上的地址**上：

```text
公网 ──▶ esc-host（TLS 终结 :443）──▶ 0.0.0.0:18784（网关）──▶ 127.0.0.1:18788
                   └──────────────▶ 公网也能直连 :18784，绕过 TLS
```

第二条箭头就是旁路：证书、SNI 选择、明文策略在那个端口上全都不生效。它之所以
一直"没事"，只是因为没人知道端口——而这不是安全属性。

隔离网段把这条旁路从「没人知道」变成「结构上不存在」：

```text
公网 ──▶ TLS 终结 :443 ──▶ 隔离网段 127.213.0.1:18801 ──▶ 认证 ──▶ 127.0.0.1:18788
```

配置长这样：

```toml
# 声明一个主机内部网段
[[segment]]
name = "edge"
cidr = "127.213.0.0/16"
note = "TLS 终结与网关之间的那一跳"

# 把路由放进去
[[route]]
name = "panel"
listen = "127.213.0.1:18801"      # 网段地址，不是 0.0.0.0
upstream = "127.0.0.1:18788"      # 本机回环
segment = "edge"
```

然后 TLS 终结那一侧的上游指向 `127.213.0.1:18801` 即可（`esc-gateway segment`
会把该填的地址打出来）。

### 为什么要这么写才有用

网段不是"多一个前缀"，而是三条**结构约束**，`esc-gateway check`、启动、面板
热应用共用同一套判据（`check_segment_rules`）：

| 约束 | 违反的后果 |
| --- | --- |
| 归属网段的 `listen` 必须是网段内的**具体地址**，不许 `0.0.0.0` | `0.0.0.0` 会把同一个端口同时铺在公网口上，网段白建 |
| 归属网段的 `upstream` 必须是本机回环（`127.0.0.0/8` / `::1`） | 上游跑到别的机器，这一跳等于没隔离 |
| 网段内的监听**必须**写 `segment` | 否则"这个网段里都有谁在听"没人管得住 |

`listen` 落在网段外的地址照旧可以配（不写 `segment`），
`esc-gateway segment` 会把这些"网段之外的监听"单列出来提醒你：那些端口上，
绕过 TLS 的那条路还在。

### 两种网段

| `kind` | 何时用 | 地址从哪来 |
| --- | --- | --- |
| `loopback`（默认，CIDR ⊂ `127.0.0.0/8`） | 绝大多数情况 | 内核已把整段 127/8 配在 `lo` 上，零权限、无需建设备 |
| `link`（其它私有网段，如 `10.213.0.0/24`） | 要独立设备/MTU/统计时 | 你自己先建 dummy/bridge 并配地址；网关只校验、不创建 |

回环网段的隔离性来自地址族语义，而不是一条可能被改错或 flush 掉的防火墙规则：

* 任何路由器都不转发 `127/8`（RFC 6890）；
* 本机 `route_localnet = 0`（默认）时，从非 `lo` 接口收上来的 `127/8` 目的地址包
  按 martian 丢弃。

`link` 网段故意**只校验不创建**：网关跑在容器里，既没有 `NET_ADMIN` 也没有
`iproute2`，假装能建设备才是危险的。地址不存在就起不来，错误信息里会给出该敲的
命令：

```bash
ip link add escseg0 type dummy
ip addr add 10.213.0.0/24 dev escseg0
ip link set escseg0 up
```

### 工具

```bash
esc-gateway segment -c /etc/esc/gateway.toml   # 网段拓扑：地址可用性、里面的路由、TLS 该指向哪
esc-gateway check   -c /etc/esc/gateway.toml   # 含同上那段（顺带校验整份配置）
```

管理面板的 `GET /__admin/api/segments` 给的是同一份信息（JSON，只读）：网段、
`probe_addr` / `ready`、以及网段里的路由。网段改动**必须重启**——它是拓扑承诺，
不该有一个"点一下生效"的入口。

### 上线顺序

1. 加 `[[segment]]`，把路由的 `listen` 改成网段地址、补 `segment = "…"`；
2. `esc-gateway segment` 确认地址可用、路由都在网段里；
3. 重启网关，看 `segment ready` 那行日志；
4. 改 TLS 终结的上游指向网段地址，重载 TLS；
5. 从公网验证旧端口已经**连不上**（这一步才是这套东西的验收标准）。

第 5 步不做的话，等于只改了个地址，旁路还在——而且更难发现，因为现在没人会
去点那个端口了。

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

## 前面加一层 TLS（自签也能用）

网关自己不做 TLS。生产上的做法是前面放一层 nginx 终结 TLS，网关退到回环：

```text
浏览器 ──https──▶ nginx :2223/:2224/:2226
                    └─http（回环）─▶ 网关 127.0.0.1:18783/18784/18786
                                       └─▶ 面板 127.0.0.1:18787/18788
```

这样切分的好处不只是"能加证书"：

- **明文那一层物理上出不了本机**（网关只听 `127.0.0.1`），不是靠配置约束
- 证书轮换、密码套件、TLS 版本这些交给 nginx，网关里少一个"解析 X.509"的攻击面
- 网关的 `secure_cookie = true` 才有意义（否则浏览器在 http 下也会回传 cookie）

### 生成自签证书

没有域名也能用（用 IP SAN）。一条命令：

```bash
install -d -m 0750 /etc/ssl/esc && cd /etc/ssl/esc
cat > san.cnf <<'EOF'
[req]
distinguished_name = dn
x509_extensions = v3
prompt = no
[dn]
CN = 114.132.241.46
[v3]
basicConstraints = critical,CA:TRUE
keyUsage = critical,digitalSignature,keyEncipherment,keyCertSign
extendedKeyUsage = serverAuth
subjectAltName = @alt
[alt]
IP.1 = 114.132.241.46
IP.2 = 127.0.0.1
DNS.1 = localhost
EOF
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout esc.key -out esc.crt -days 3650 -config san.cnf -extensions v3
chmod 0644 esc.crt && chmod 0640 esc.key
```

`basicConstraints CA:TRUE` 是刻意的：把 `esc.crt` 装进**手机/电脑的受信任根**之后，
浏览器就完全没有警告了（自签叶子证书装进根库也不会被信任，自签 CA 才会）。
不装也行，每个浏览器会话点一次"继续访问"。

### nginx 侧两个必须注意的细节

**① `Host` 要用 `$http_host`，不能用 `$host`。**

```nginx
proxy_set_header Host $http_host;   # 保留端口 → 网关能拼出 //host:2223/ 这样的链接
# proxy_set_header Host $host;      # 会把端口吃掉，首页的端口列表全指错
```

网关首页和"管理面板在别处"的提示都是**按 `Host` 拼链接**的，丢掉端口就全错。

**② `X-Forwarded-For` 要覆盖，并同时打开 `trust_forwarded_for`。**

```nginx
proxy_set_header X-Forwarded-For $remote_addr;   # 覆盖，不是 $proxy_add_x_forwarded_for
```

```toml
trust_forwarded_for = true
```

网关取 XFF 的**最后一跳**（最靠近它的那一跳），所以 nginx 用真实对端地址整体
替换时，客户端伪造的值进不来。不开这个开关的后果是：所有请求的来源都变成
`127.0.0.1`，按来源限速把所有人算成同一个人，`allowed_cidrs` 也会失效。

### 有了域名：真证书（无需点「继续访问」）

自签方案能用，但每个浏览器都会拦一次。有域名就换真证书。

**这里踩过一个坑，值得记下来：Let's Encrypt 签发不了。** 报的是

```
SERVFAIL looking up A for cloud.tx.evencewu.asia
During secondary validation: SERVFAIL while processing CAA for ...
```

但本地 `dig @dns25.hichina.com` 连续 20 次全是 NOERROR —— 权威服务器是好的。
原因是 LE 的 Boulder **从全球多个验证点**查询，而阿里云（hichina）的免费 DNS
对部分境外解析源返回 SERVFAIL。LE 的多点校验只要有一个点失败就整体失败，
所以重试也没用。换 **ZeroSSL** 一次就过了（单一验证点 + 走 webroot）：

```bash
# ① 先取 EAB 凭证（ZeroSSL 的 ACME 注册需要外部账户绑定）
curl -s -X POST https://api.zerossl.com/acme/eab-credentials-email \
     --data-urlencode "email=you@example.com"
# → {"success":true,"eab_kid":"...","eab_hmac_key":"..."}

# ② 签发
docker run --rm --network host \
  -v /etc/letsencrypt:/etc/letsencrypt -v /var/lib/letsencrypt:/var/lib/letsencrypt \
  -v /var/www/certbot:/var/www/certbot \
  certbot/certbot certonly --webroot -w /var/www/certbot \
  -d cloud.tx.evencewu.asia \
  --server https://acme.zerossl.com/v2/DV90 \
  --eab-kid <kid> --eab-hmac-key <hmac> \
  --register-unsafely-without-email --agree-tos --non-interactive
```

> 换句话说：**如果这个域名哪天迁到 Cloudflare DNS，LE 就能用了**。不迁也行，
> ZeroSSL 的证书浏览器同样信任，只是 90 天要续一次。

**HTTP-01 的 80 端口从哪来？** 生产上 80 被 esc-host 的 edge_router 占着，
所以加了一条完整的域名路由指向一个只服务 ACME 的 nginx：

```toml
# /var/lib/esc/edge-routes.toml
"cloud.tx.evencewu.asia" = 18789   # ACME webroot
```

那个 nginx（`esc-certweb` 容器）除 `/.well-known/acme-challenge/` 外一律 404：
它不是网站，别让扫描器以为它是。webroot 在 `/var/www/certbot`。

### 续期

ZeroSSL 证书 90 天。`esc-certbot-renew.timer` 每天 04:17（±30 分钟）跑一次，
`certbot renew` 只在到期前 30 天真正申请，平时空跑。

**关键在 `ExecStartPost`：**

```ini
ExecStartPost=/usr/bin/docker exec esc-tls nginx -s reload
```

nginx 启动时就把证书读进内存了，文件换了不 reload 不生效 —— 少了这一行，
续期会"成功"，但线上继续用旧证书直到下次重启，然后突然过期。

### 两套证书按 SNI 并存

IP 访问走自签，域名走真证书。nginx 按 `server_name` 的 SNI 匹配挑证书，
所以同一个端口上可以：

```nginx
server { listen 2224 ssl; server_name _;                        # 自签
         ssl_certificate /etc/ssl/esc/esc.crt; }
server { listen 443 ssl; listen 2224 ssl;
         server_name cloud.tx.evencewu.asia;                    # ZeroSSL
         ssl_certificate /etc/letsencrypt/live/.../fullchain.pem; }
```

域名那一段**同时挂 443 和 2224**：443 要等云安全组放行，2224 是已经开着的，
安全组没放行时 `https://<域名>:2224/` 立刻能用。

### ⚠ HSTS 的陷阱：它是按**主机名**记的，不看端口

这里踩过一个坑，代价是「页面完全打不开」。

当时域名段落同时监听 443 和 2224，并在 2224 上发了：

```nginx
add_header Strict-Transport-Security "max-age=2592000" always;
```

**结果用户访问 `cloud.tx.evencewu.asia` 直接超时。** 因为 HSTS 记录的是
`cloud.tx.evencewu.asia` **这个主机名**，跟端口无关。浏览器收到后就把整个主机名
标成 HTTPS-only，于是下次输入域名（等价于 `http://…/`，端口 80）会被浏览器
**在本地改写成 `https://…/`，也就是标准端口 443** —— 而 443 当时还没在云安全组
放行，于是超时。80 端口上那条 301 跳转根本没机会执行（浏览器压根没发这个请求）。

**规则：HSTS 只能在「它指向的那个端口真的通」时才发。** 具体到这里的顺序：

1. 先放行 443，确认 `https://<域名>/` 能开
2. 再打开 HSTS

自签段永远不加 —— 浏览器本来就不信那个证书，加了反而在换真证书后留一段无法
回退的时间。

已经发出去过 HSTS 的话，光改配置不够，**浏览器那边的记录得手动清**（默认按
`max-age` 记 30 天）：

- **Chrome / Edge**：`chrome://net-internals/#hsts` → *Delete domain security
  policies* → 输入域名 → Delete
- **Firefox**：关闭该站所有标签 → 关闭浏览器 → 在 profile 目录删掉
  `SiteSecurityServiceState.txt`
- **Safari**：删 `~/Library/Cookies/HSTS.plist` 后重启

清之前，显式写端口（`https://<域名>:2224/`）是可以照常访问的 —— HSTS 只改写
默认端口的请求，不阻止你直接指定端口。

### 明文端口会怎样

nginx 在 2223/2224/2226 上只监听 TLS，所以 `http://…:2226` 会得到
`400 Bad Request`（nginx 报 "The plain HTTP request was sent to HTTPS port"）。
这正是想要的：**没有任何一条明文路径能到达登录表单。**

## 管理面：JSON API（2026-11 起没有服务端渲染的 HTML 了）

管理页现在是**服务器面板那个壳子里的一个标签页**（见 `docs/SERVER-PANEL.md`
的「三个面板合并」）。网关这台机器上留下的是接口：

```
GET    /__admin/api/overview          我是谁 + 我能去哪些端口 + 当前从哪个端口进来
GET    /__admin/api/users             账号列表（含操作日志、banner）
GET    /__admin/api/users/{name}      单个账号（编辑前回填用）
POST   /__admin/api/users             新建
POST   /__admin/api/users/{name}      修改
POST   /__admin/api/users/{name}/delete|adopt|revoke
GET    /__admin/api/routes            端口列表
GET    /__admin/api/routes/{name}     单个端口
POST   /__admin/api/routes            新建
POST   /__admin/api/routes/{name}     修改
POST   /__admin/api/routes/{name}/delete|disable|enable
GET    /__admin/api/sessions          在线会话
POST   /__admin/api/sessions/{short}/revoke
GET    /__admin/api/me                我的账号
POST   /__admin/api/me/password       改口令
```

### 三个约定（前端按它们写，别绕开）

**① `/__admin/api/*` 在**任何**端口上都由网关应答。**

这是合并的前提：壳子从业务端口（`2224`）加载，管理接口必须同源可达，否则就要
CORS + 跨端口 cookie，两样都不该为一个内部页面引入。

代价是业务端口上的 `/__admin/*` 不能再被上游占用。`__` 前缀本来就是网关保留字。

**② 未登录是 `401`，不是 `302` 到登录页。**

从前服务端会把你重定向到 `/__auth/login`。合并之后跳走会把整个标签页的状态
（正在编辑的表单、滚动位置、轮询）一起丢掉，所以改成 401，由壳子弹登录框。
登录页本身仍然是 HTML——它是「302 → 填表单 → 回到原处」这条流程的一部分，
那时候前端还没跑起来。

`form` 是回填用的：用户填了十个字段只错一个，不该让他重填一遍。
`message` 和 `error` 都是**给人看的中文**，前端直接显示，不再做二次翻译。

注意区分「**提交失败**」和「**打开表单**」：`GET /__admin/api/users/{name}` 与
`GET /__admin/api/routes/{name}`（编辑前回填）是 **200 + 表单本身**，不是
400 + `{error, form}`。前端的 `api()` 把非 2xx 一律当失败抛出，详情要是用 400，
「编辑」按钮就只会静默失效——弹窗根本不出现。

**④ 请求体两种编码都收：`application/json` 与 `application/x-www-form-urlencoded`。**

面板发 JSON（`content-type: application/json`），命令行与老表单发 urlencoded；
`read_form` 按 `content-type` 分流，两条都摊成同一套 `(键, 文本)`。字段语义对齐：

| JSON | 摊成 |
|---|---|
| 字符串 | 原样 |
| `true` / `false` | `"true"` / `"false"`（`flag()` 认这两个） |
| 数字 | 十进制文本（仍可 `.parse()`） |
| 数组 | 同名多值：`{"routes":["a","b"]}` ≡ `route=a&route=b` |
| `null` / 嵌套对象 | 跳过（"没填"） |

> 2026-11 合并时这里出过一次事故：前端改成了 JSON，服务端还在拆 `a=1&b=2`。
> JSON 体被当成**一个没有 `=` 的键**，每个字段都读成空串——界面上「新建账号」
> 永远回「账号名不合法：只能用字母、数字……」，且回填的名字是空的。
> e2e 全部走 `post_form`，所以一片绿。现在两种编码各有一条测试钉住。

### 只有 `/__auth/login` 和错误页还是 HTML

`web.rs` 里剩下的渲染只有这两样，都是单文件内联、零外部资源。理由没变：
登录页要是依赖 CDN，在网络受限的机器上就白屏——而那正是你需要登录进去改配置
的时候。删掉的那一千多行管理页 HTML，每一个分支都被 `web.rs` 里的
`#[derive(Serialize)]` 结构体承接了。

## 管理面：独立端口还要不要

> 管理和业务页面的合并在 2026-11 完成：账号 / 端口 / 会话现在是服务器面板壳子
> 里的「网络 › 网关」子页（与 Docker 网络、端口映射同一个标签页，理由见
> `docs/SERVER-PANEL.md` 的「为什么这三件事放在同一个标签页」）。
> 本节只回答「`admin_listen` 还需不需要」。

管理接口在**任何**端口都可达（`/__admin/api/*`），所以 `admin_listen` 不再是
「管理面在哪」的开关，它现在只决定**要不要额外开一个只跑接口、不转发业务的端口**。

### 要不要给它一个独立端口

两种部署，`admin_listen` 决定用哪种：

| | 管理面板的位置 | 适合 |
|---|---|---|
| **不配**（默认） | 每个转发端口的 `/__admin/` | 单机、图省事。管理面和业务面共用同一个公网端口，隔着的只有 `admin_allowed_cidrs` |
| **配了**（如 `127.0.0.1:2226`） | 那个端口**独占**，转发端口上只剩一页「在别处」的说明 | 想让管理面从公网消失（只留 SSH 隧道 / VPN / 内网），或不想让改账号的流量和看面板的流量混在一起 |

配了之后有三件具体变化：

1. **暴露面分开了**。可以把 2226 完全不对公网开放，而 2224/2225 照旧。
2. **cookie 仍然共享**。浏览器不按端口隔离 cookie，所以在管理端口登录一次，
   打开被代理的面板不用再登录——反过来也一样。
3. **删业务端口不会把自己关在外面**。面板在独立端口上，业务端口全删光了还能
   进去加回来（这也是为什么「不许删到 0 个」那道闸在配了独立端口后会放开）。
4. **业务端口上看不到管理入口的存在**。`/__admin/` 在那里只是一个普通路径，
   未登录时和别的路径一样拿 401 / 跳登录页。

代价是多一个端口要维护，而且**改 `admin_listen` 本身要重启**（和 `title` 一类：
它不属于"随时会改"的东西，而热改监听目标容易留下"一半新一半旧"的状态）。

`admin_listen` 与任何 `[[route]]` 的端口冲突会在**启动时**报错——不能拖到运行时
靠"谁先绑上谁赢"，因为输的那个可能是管理面板，那就再也进不去了。`0.0.0.0:2224`
和 `127.0.0.1:2224` 也算冲突（bind 时确实会 EADDRINUSE）。

### 转发端口上的 `/__admin/` 变成什么

**什么都不变——它就是一个普通路径。** 配了 `admin_listen` 之后，网关不再占用
这个路径：请求照常认证，然后转发给上游（上游通常没有它，于是 404）。

一开始这里放了一页「管理面板在另一个端口」的指路说明 + 链接，后来去掉了：
那等于在**每个**公网端口上公开管理入口的位置。指路的价值远不如「别告诉任何人
管理面在哪」。忘了管理端口在哪，去看 `esc-gateway check` 的输出或配置文件。

（首页 `/__auth/` 上的「管理账号」按钮会指到管理端口去 —— 那是登录后才有的一页，
不算泄露，而且不该让它在业务端口上点出 404。）

### 谁能进

`admin = true` 的账号。普通账号只能进 `/__admin/me`（看自己的授权 + 改自己的
口令），其余路径在分发前就 403——**在分发之前**是有意的：将来加了新端点，
不会因为「忘了加权限判断」而漏掉。

### 账号存在哪：账号库叠加在配置文件之上

| | 位置 | 面板能改吗 |
|---|---|---|
| `[[user]]` | `gateway.toml` | 不能（只读） |
| 账号库 | `users_file`，默认 `gateway-users.toml` | 能 |

**同名账号以账号库为准。** 这样配置文件永远是一条救援通道：账号库误删、
解析失败、卷没挂上，`gateway.toml` 里的账号照样能登进来修。反过来说，管理面板
**永远无法把配置文件里的账号删掉**——「把自己锁在门外」有硬下限。

代价是「改了 `gateway.toml` 里的口令却还是旧口令能登」这种困惑。所以：

- 启动时对每个被覆盖的同名账号打一条 WARN，日志里还有一句「账号库：<路径>」；
- 面板列表里把「配置文件（只读）」和「账号库（覆盖了配置文件）」标出来，后者
  还挂一条黄色提示建议把配置文件里那条删掉；
- `esc-gateway check` 会把两个来源都列出来。

### 配置文件里的端口也能直接改

**不需要先点「接管」。** 面板里每一行都有「编辑」和「下架」，包括来自
`gateway.toml` 的端口。保存时会自动在路由库生成一条同名条目接管它——一步到位。

状态还是两个来源、同名的以路由库为准，只是这个细节不用用户操心：

| 面板上看到的来源 | 含义 |
|---|---|
| 配置文件 | 只在 `gateway.toml` 里，还没被面板动过 |
| 路由库（覆盖了配置文件） | 面板改过它，现在库里的定义生效 |
| 配置文件（已下架） | 面板把它下架了，配置文件里那条被墓碑挡住 |
| 路由库 | 面板新建的 |

### 下架 vs 删除

两个按钮，区别是「以后还要不要它」：

| | 监听 | 定义 | 能恢复 |
|---|---|---|---|
| **下架** | 停 | 留着（路由库里的 `[[disabled]]`） | 能，点「恢复」 |
| **删除** | 停 | **抹掉** | 不能，重启也不会回来 |

删除只对**路由库自己拥有**的条目可用。下面两种情况按钮根本不出现，因为它们的
结果和"删除"的意图正相反：

* **`gateway.toml` 里也有同名条目** → 删掉库里那条，配置那条立刻复活，端口照样
  在听。用户以为删了，其实什么都没变（甚至更糟：定义换成了配置文件里那份）。
* **条目只在 `gateway.toml` 里** → 那是只读文件，面板无从删起。

这两种情况面板上会标一个「外部定义」的徽标，点「下架」可以让它停止监听；想彻底
删掉需要编辑 `gateway.toml`。

> 为什么不让面板改写 `gateway.toml`：那会把你手写的注释（为什么这么配、踩过什么
> 坑）用 `toml::to_string` 一次性吃掉，而且运维手改和面板改会互相覆盖。分开两个
> 文件，冲突点只剩「同名」一处，而它能被明确地表达出来。

### 「下架」为什么需要墓碑

配置文件里的端口面板删不掉，没有墓碑的话"删除"会变成"删掉库里那条覆盖项，然后
配置里那条马上又冒出来"——用户会觉得删除按钮坏了。墓碑让"下架"成为一个**可表达、
且重启后仍然成立**的状态：写进文件，重载后 `disabled_count` 仍是 1。

### 改完立刻生效（这一点和 route 不一样）

账号变更**不需要重启**，但会话里的权限是登录那一刻的快照，所以每次变更都会
显式作废该账号的会话：

| 操作 | 会话处理 |
|---|---|
| 改口令 / 改授权 / 降权限 | 作废该账号所有会话（当前这个除外——把自己踢掉太蠢） |
| 删账号 | 作废该账号所有会话 |
| 踢下线（按账号或按单条会话） | 立即失效 |
| 新增账号 | 无需处理 |

不这么做的话「删掉一个被盗账号」要等到 `session_ttl_hours` 之后才真正生效。

### 锁死防护

三重，从软到硬：

1. 最后一个管理员不能被删除、也不能被取消管理权限；
2. 不能用**正在使用的**账号删除自己（否则当前会话立刻失效，你只会看到一个
   莫名其妙的登录页）；
3. `gateway.toml` 里的账号面板动不了 —— 就算账号库被删光，配置文件仍能登进来。

### 自助改口令要输当前口令

光有 cookie 就能改口令的话，一次会话劫持就变成了永久占有。所以 `/__admin/me`
要输当前口令，而且**同样走登录失败限速**——不然这里会变成绕过登录页限速的
无限试错口。

### 安全细节

| 项 | 做法 | 为什么 |
|---|---|---|
| CSRF | 变更一律 POST + `same_origin` 校验 | `SameSite=Strict` 是主防线，这是纵深防御 |
| 提示横幅 | 存在**会话**里，不放 URL | `/__admin/?err=…` 是别人能构造的，能在我们自己的面板里显示一条看似权威的红条。转义挡得住 XSS，挡不住这种钓鱼 |
| 页面缓存 | `Cache-Control: no-store` | 账号列表被中间层缓存下来会是什么后果，不用细想 |
| 点击劫持 | `X-Frame-Options: DENY` | 「删除账号」按钮不该能被套在别人的页面里 |
| 会话 id | 页面只显示前 12 位 | 一次 XSS 不该等于交出所有在线会话 |
| 口令哈希 | 面板从不显示、不返回 | 只能整体重置 |
| 换口令后 | 重建 session id 由登录路径负责；改密的会话处理见上表 | |

### 只读模式

`users_file` 所在目录不可写（只读容器、`/etc` 只读、`users_file` 未设）时，
面板变成一个只读视图：列表照常显示、页面顶部说明为什么改不了、提交按钮禁用。
不会出现「填完表单才被拒」。

### 手工编辑账号库

格式和 `gateway.toml` 一样，只是没有 `[[route]]`：

```toml
[[user]]
name = "alice"
hash = "$argon2id$v=19$m=19456,t=2,p=1$..."
routes = ["tunnel-panel"]
admin = false
```

改完**要重启网关**（运行中的进程用的是内存里的副本）。文件权限是 `0600`——
口令哈希不是明文，但也不该让同机其他用户随便读：离线爆破一个 8 字符口令只要
几小时。

忘了管理员口令的恢复路径：`esc-gateway hash-password` 造一个哈希填进去，
重启；或者直接从 `gateway.toml` 里的账号登录。

## 管理面板：代理端口

`/__admin/routes` 增删「端口 → 上游」，**保存即生效**，不用重启。

### 它改的是哪个文件

和账号一样的叠放关系：

| | 位置 | 面板能改吗 |
|---|---|---|
| `[[route]]` | `gateway.toml` | 不能（只读，要改先「接管」） |
| 路由库 | `routes_file`，默认 `gateway-routes.toml` | 能 |

同名以路由库为准。这条设计在这里比账号那边更要紧：路由库坏了如果连带端口不
监听，那就连"进管理面板去修"的入口都没有了。所以配置文件里的端口永远有效。

### 热应用：要么全成，要么全不动

改一个端口涉及三件事：绑套接字、写磁盘、换内存里的路由表。顺序是固定的：

```text
① 算合并结果并校验（纯计算）        ← 端口冲突 / 自环 / 名字重复在这里暴露
② 现绑新端口                        ← 任何一个绑不上就整体放弃，旧监听一个不动
③ 写盘（原子替换）                  ← 写失败就丢掉刚绑的套接字，内存不动
④ 换注册表 + 起新监听 + 停旧监听
```

这样做的原因很直白：**不能出现"文件里改了、端口没绑上"或者"端口绑上了、文件没写"**。
前者会让下次重启静默回退，后者会让重启后端口消失。中间任何一步出错，用户看到的
都还是原来那套。

### 上游改了，下一个请求就生效

不是"下次连接"，是**下一个请求**：每条连接在处理每个请求时，按 `local_addr`
从路由表重读一次定义。

```text
accept 时拿到 local_addr（这个端口） → 每个请求 gw.routes.at(local_addr) → 转发
```

按地址查而不是按路由名：改名之后老连接也能立刻用上新定义。副作用是改上游不会
踢掉现有连接——这正是想要的（改一个字符不该让正在看日志的人页面断掉）。

### 端口删错了会怎样

两道闸，因为这类操作不可撤销：

- **不能删掉你自己正在用的那个端口**。删了会立刻掐断当前连接，用户只会看到
  「页面加载失败」，然后不知道该怪谁。请从别的端口进来删。
- **不能删到只剩 0 个**。一个端口都没有的网关什么都不转发，包括管理面板。

真删错了还有兜底：配置文件里的同名端口会重新生效。

### 未监听的端口会标出来

列表里如果某条标着「未监听」，意思是它在配置/路由库里存在、但注册表里找不到
对应的活端口。正常情况下不该出现；出现就说明热应用中途出过问题，去看网关日志。

### 只读模式

`routes_file` 不可写时（只读容器、`/etc` 只读、没配 `routes_file`），`/__admin/routes`
变成只读视图：列表照常显示、来源标「配置文件（只读）」、只有「接管」按钮。
端口还是只能在 `gateway.toml` 里改，改完重启。

### 和账号授权的联动

账号的授权是按**端口名**引用的。所以：

- 端口改名 = 换一个端口，原来授权过旧名字的账号会失去访问权（而不是悄悄跟上）。
- 端口下架后，授权给它的账号会看到一条「被授权访问已不存在的端口」的提示。
- 删掉端口后，如果还有账号的 `routes` 里留着这个名字，代理页顶部会挂一条黄条
  列出「谁还引用着已不存在的端口」——悬空引用不会报错，只会让人以为自己有权限。

### 手工编辑路由库

```toml
[[route]]
name = "server-panel"
listen = "0.0.0.0:2224"
upstream = "127.0.0.1:18788"
allowed_cidrs = []
# strip_prefix = "/panel"
# max_body_bytes = 1048576
```

改完要重启网关（运行中的进程用内存副本）。文件权限 `0600`。

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
| TLS 终结 | 需要域名 + 证书管理。在前面放 nginx/Caddy 更合适，然后打开 `secure_cookie`（仓库里有现成的自签方案：见下） |
| gzip 重压/解压 | 原样透传 `Accept-Encoding` 与 `Content-Encoding` |
| 重定向跟随 | 代理不该跟随，那是客户端的事 |
| 运行时改 `title` / `admin_allowed_cidrs` / 限速参数 | 这些仍在配置文件里、要重启。端口与账号都能热改（见下），剩下的属于"改了也不急"的一类 |

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
| 浏览器一直跳登录页，但口令没错 | `secure_cookie = true` 却在用 `http://` 访问——浏览器不会把 Secure cookie 回传。改用 `https://` |
| 加了 nginx 后所有请求来源都是 127.0.0.1 | 忘了开 `trust_forwarded_for`，或 nginx 那边没设 `X-Forwarded-For $remote_addr` |
| `http://…` 访问得到 400 Bad Request | 正常：那个端口只监听 TLS（nginx: "plain HTTP request was sent to HTTPS port"） |
| 首页的端口链接少了端口号 | nginx 用了 `Host $host`，要改成 `$http_host` |
| Let's Encrypt 报 SERVFAIL，但 `dig @权威NS` 正常 | 权威 DNS 对 LE 的境外验证点返回 SERVFAIL（阿里云免费 DNS 常见）。换 ZeroSSL，或把 DNS 迁到 Cloudflare |
| 证书续期"成功"了但浏览器还是旧证书 | 忘了 `nginx -s reload`——nginx 启动时就把证书读进内存了 |
| 加了域名后页面**完全打不开**（超时），但带端口能开 | 在非 443 端口发过 HSTS。浏览器把主机名锁到 443 了，而 443 没放行。清浏览器 HSTS 记录，或先放行 443 |
| `/__admin/` 一直 403 | 账号没有 `admin = true`；或来源不在 `admin_allowed_cidrs` 里（页面会写明是哪一个） |
| 面板是灰的、按钮点不动 | 账号库不可写。页面顶部有原因，`esc-gateway check` 也会列 |
| 改了 `gateway.toml` 里的口令但不生效 | 该账号在账号库里也有同名条目，后者优先。看启动日志的 WARN 或面板里的「覆盖」标记 |
| 改了 `gateway.toml` 里的端口但不生效 | 同上：面板改过它，路由库里那条生效。面板上会标「路由库（覆盖了配置文件）」 |
| 端口在面板里标着「已下架」 | 面板把它下架了（配置文件里那条被墓碑挡住）。点「恢复」或删掉路由库里的 `[[disabled]]` 块 |

## 测试

139 个测试（87 单测 + 52 端到端）。端到端用真实 TCP + 真实上游，覆盖：

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

独立管理端口另外覆盖：管理端口提供面板与登录页、不转发任何东西、业务端口上的
`/__admin/` 变成普通路径（转发给上游，未登录时也不泄露管理端口号）、首页的管理
链接指向管理端口、在管理端口登录的会话能直接打开转发端口、非管理员能进来改自己
的口令但进不了管理功能、`admin_listen` 与 route 端口撞车时启动即报错。

代理端口另外覆盖：新增端口后**真的开始在听**、落盘并能在重启后读回、改上游
下一个请求就生效（同一连接、同一 URL，状态码从 200 变 502 即证明）、端口被占用
时整体不生效且表单回填、下架端口后连接被拒、下架状态重启后仍然成立、可以恢复、
**真删除会连定义和墓碑一起抹掉**、配置里也有同名条目时拒绝删除并说明原因、
配置文件来的端口不给删除按钮、
**编辑配置文件里的端口不需要先接管**、不许删自己脚下的端口、不许删到 0 个、
上游指向本网关自己的端口（含管理端口）会被拒绝。

网关首页另外覆盖：未登录跳登录页、只列有权限的端口（无权的不出现名字）、
管理员才有管理入口、链接是协议相对且带端口。

管理面板另外覆盖：未登录跳登录页/拿 401、普通用户只能进 `/me`、建账号→能登录
→只能进被授权的端口、账号库落盘且不含明文口令、最后一个管理员删不掉也降不了、
删自己被拦、改口令作废其他会话（保留当前）、跨站 POST 被拒、按账号/按前缀踢
下线、只读账号库、`admin_allowed_cidrs` fail-closed、页面不可缓存不可套 iframe。
