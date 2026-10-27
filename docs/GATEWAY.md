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

## 网关自己的页面

登录之后除了被代理的面板，网关自己还有三个页面：

| 路径 | 内容 |
|---|---|
| `/__auth/` | **网关首页**：我是谁、我能访问哪些端口（可点进去）、账号设置入口 |
| `/__admin/` | 账号管理（见下一节）。普通账号只能进它的 `/me` |
| `/__auth/logout` | 退出 |

`/__auth/` 存在的理由很实际：登录成功后你会**落在被代理的面板里**，而那个页面
完全不知道「网关」的存在 —— 于是 `/__admin/` 成了一个只能靠记 URL 才到得了的
地方。首页补上了这个出口，登录页页脚和管理面板顶栏也各有链接指回来。

首页的入口列表只列**你这个账号有权访问**的端口。没权限的连名字都不出现：
否则「这台机器开了哪些面板」就成了公开信息。链接用协议相对写法
（`//host:port/`），以后前面挂 TLS 也不用改。

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
| `users_file` | 配置文件同目录的 `gateway-users.toml` | 面板管理的账号存在这里。目录不可写时面板自动降级为只读 |
| `routes_file` | 配置文件同目录的 `gateway-routes.toml` | 面板管理的端口（代理目标）存在这里。同样可降级为只读 |
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

## 管理面板

登录后在**任意**端口访问 `/__admin/`：增删账号、改口令、改授权、踢会话。

### 它为什么不另开端口

网关已经是公网入口、已经有 TLS 与防火墙策略，多开一个端口就多一处要同步维护
的东西。想「只从内网改账号」用 `admin_allowed_cidrs`，不必为此多占端口。

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

### 「接管」

配置文件里的账号在面板里是只读的，要改得先点**接管**：把它的 argon2 哈希原样
复制进账号库（不需要、也不该知道原口令），从此以账号库为准。

自助改口令会自动接管——否则一个来自配置文件的账号「连自己的口令都改不了」，
那太别扭了。

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
| TLS 终结 | 需要域名 + 证书管理。在前面放 nginx/Caddy 更合适，然后打开 `secure_cookie` |
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
| `/__admin/` 一直 403 | 账号没有 `admin = true`；或来源不在 `admin_allowed_cidrs` 里（页面会写明是哪一个） |
| 面板是灰的、按钮点不动 | 账号库不可写。页面顶部有原因，`esc-gateway check` 也会列 |
| 改了 `gateway.toml` 里的口令但不生效 | 该账号在账号库里也有同名条目，后者优先。看启动日志的 WARN 或面板里的「覆盖」标记 |

## 测试

119 个测试（81 单测 + 38 端到端）。端到端用真实 TCP + 真实上游，覆盖：

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

代理端口另外覆盖：新增端口后**真的开始在听**、落盘并能在重启后读回、改上游
下一个请求就生效（同一连接、同一 URL，状态码从 200 变 502 即证明）、端口被占用
时整体不生效且表单回填、删端口后连接被拒、不许删自己脚下的端口、不许删到 0 个。

网关首页另外覆盖：未登录跳登录页、只列有权限的端口（无权的不出现名字）、
管理员才有管理入口、链接是协议相对且带端口。

管理面板另外覆盖：未登录跳登录页/拿 401、普通用户只能进 `/me`、建账号→能登录
→只能进被授权的端口、账号库落盘且不含明文口令、最后一个管理员删不掉也降不了、
删自己被拦、改口令作废其他会话（保留当前）、跨站 POST 被拒、按账号/按前缀踢
下线、只读账号库、`admin_allowed_cidrs` fail-closed、页面不可缓存不可套 iframe。
