# TLS 终结模块（`esc.tls`）

> 2026-11-12 起，cloud2 上那一层 nginx（容器 `esc-tls`）由本模块接替。
> 原来那份 `nginx.conf` 里的 5 个 `server` 块，现在是 `host.toml` 里的 5 个
> `[[tls.config.site]]`。

## 它做什么

四件事，与原来的 nginx 逐条对应：

| # | 能力 | 实现位置 |
|---|---|---|
| ① | **TLS 终结**，按 SNI 选证书（真证书 / 自签） | `certs.rs` |
| ② | **按端口分流**：每个监听端口一个固定动作（转发 / 301） | `config.rs` + `proxy.rs` |
| ③ | **头部改写**：Host 透传、`X-Forwarded-For` 覆盖、`X-Forwarded-Proto: https` | `proxy.rs` |
| ④ | **WebSocket 透传**：`Upgrade` / `Connection` 保留 + 双向流拼接 | `proxy.rs` + `serve.rs` |

## 为什么放进 `esc-host` 而不是继续用容器

与 `esc.tunnel` / `esc.edge-router` 同一个理由：它是**数据面**，每个进来的 HTTPS
请求都要过一次。独立进程要多一次 IPC 与一份帧编解码，而它的生命周期本来就该与
宿主一致。

真正的收益不在性能，在**配置只有一份**：原来 nginx 的 `nginx.conf` 与
`gateway.toml` 是两套，改一边忘了另一边不会有任何测试报错。

代价说清楚：**它崩了会带走宿主**，而宿主带着隧道。这是 fail-closed 的取舍——见下面
「挂载失败 = 启动失败」。

## 配置

```toml
[tls]
enabled = true

[tls.config.defaults]
cert = "self"                    # SNI 没命中时用哪张
tls12 = true                     # 是否允许 TLS 1.2
max_body_bytes = 12582912        # 12 MiB
handshake_timeout_secs = 10      # 握手超时（挡 slowloris）
hsts = false                     # 见下面「HSTS 为什么默认关」

[[tls.config.cert]]
name = "self"
cert = "/etc/esc/pki/self.crt"
key  = "/etc/esc/pki/self.key"

[[tls.config.cert]]
name = "cloud-tx"
cert = "/etc/esc/pki/cloud.crt"
key  = "/etc/esc/pki/cloud.key"
sni  = ["cloud.tx.evencewu.asia"]

[[tls.config.site]]
name     = "panel"
listen   = "0.0.0.0:2222"
upstream = "127.0.0.1:18790"
websocket = true
log_query = false
read_timeout_secs = 3600
```

### `[[tls.config.site]]` 的字段

| 字段 | 默认 | 说明 |
|---|---|---|
| `name` | 必填 | 唯一。日志里用它标识，不用端口（端口会变） |
| `listen` | 必填 | 单个字符串或数组；数组会展开成多个监听 |
| `upstream` | 与 `redirect` 二选一 | `host:port`，**必须是 IP**（不查 DNS） |
| `redirect` | 与 `upstream` 二选一 | 301 目标模板，见下 |
| `websocket` | `false` | 显式打开才透传 `Upgrade` |
| `log_query` | `false` | 是否把查询串写进访问日志 |
| `read_timeout_secs` | `300` | 等上游响应的超时 |
| `max_body_bytes` | 继承 | 该端口的请求体上限，超限 413 |
| `hsts` | 继承 | 该端口是否发 HSTS |

### `redirect` 的占位符

| 占位符 | 展开成 | 对应 nginx |
|---|---|---|
| `{host}` | Host 头**原样**（含端口） | `$http_host` |
| `{hostname}` | Host **去掉端口** | `$host` |
| `{uri}` | 路径 + 查询串 | `$request_uri` |
| `{path}` | 只有路径 | `$uri` |

跳到**另一个端口**时必须用 `{hostname}`：

```toml
redirect = "https://{hostname}:2224{uri}"     # ✅ → https://x:2224/a/b?c=1
redirect = "https://{host}:2224{uri}"          # ❌ → https://x:2223:2224/a/b?c=1
```

这条错误只有真点一次才发现，`tests/e2e.rs` 里有一条测试专门钉它。

## 与原 nginx 配置的四处有意差异

### ① 证书按 SNI 全局选一张，不按 server 块各配一份

原来 2223/2226 的块只声明了自签证书，用域名访问它们时浏览器会警告一次。合并之后
那次警告没有了——**这不是退步**。

代价：将来若出现「同一个域名在不同端口要用不同证书」（客户端证书分流之类），这个
模型表达不了，得重新引入按站点的证书绑定。

### ② 访问日志是结构化的，不是 `log_format` 文本行

```
INFO esc_tls::access: access site=panel peer=1.2.3.4:5678 method=GET
     uri=/ws status=101 len="0" user_agent="-" upstream="127.0.0.1:18790" rt_ms=34
```

`log_query = false` 时**根本不存在**带令牌的字段——泄漏面从「记得配 log_format」
变成「这个字段压根没采集」。

需要 nginx 那种一行文本时，`tracing-subscriber` 的 fmt 层就能输出。

### ③ 请求体超限是 413，不是静默拒绝

`Limited` 在流式读的时候兜底（覆盖没有 `content-length` 的分块上传），
`content-length` 超限则在**连上游之前**就返回。

### ④ 没有 `proxy_ignore_client_abort on`

客户端断开时上游请求会被取消。原来的 nginx 配了这个开关。对面板这种短请求场景
没有实际差别；如果将来有「客户端断开但上游必须跑完」的接口，要回来补。

## 部署

### 证书放哪

`esc-host` 以 `User=esc` 运行，读不到 root-only 的私钥。所以证书要放一份到
`/etc/esc/pki/`（`0750 root:esc`）：

```bash
install -d -m 0750 -o root -g esc /etc/esc/pki
install -m 0640 -o root -g esc /etc/ssl/esc/esc.crt   /etc/esc/pki/self.crt
install -m 0640 -o root -g esc /etc/ssl/esc/esc.key   /etc/esc/pki/self.key
install -m 0640 -o root -g esc /etc/letsencrypt/live/cloud.tx.evencewu.asia/fullchain.pem /etc/esc/pki/cloud.crt
install -m 0640 -o root -g esc /etc/letsencrypt/live/cloud.tx.evencewu.asia/privkey.pem   /etc/esc/pki/cloud.key
```

**证书续期后要重新复制一遍并重启 `esc-host`**。本模块不做运行期热加载——「证书
什么时候换的」必须可追溯，而 watch 文件会把它变模糊。certbot 的 `--deploy-hook`
里加这两步即可。

### `enabled = false` 是默认

它要绑 443 这类端口，「默认打开」意味着一次升级就可能撞上别人的 80/443。

### 挂载失败 = 启动失败

任何一项配置错误（证书读不到、端口被占、`upstream` 写成主机名）都会让
**`esc-host` 整体启动失败**，包括隧道。

这是刻意的（与 `esc.edge-router` 一致的 fail-closed）：一个「起来了但某个端口
握手永远失败」的部署，排查成本远高于启动时报一行错。

**所以切端口时顺序很重要**：先腾出端口，再重启，否则新进程起不来而旧进程已停。

### 从 nginx 切过来的实际步骤

```bash
# 1. 备好证书目录与配置（见上）
# 2. 让 nginx 让出端口
docker stop esc-tls
# 3. 立刻重启宿主（此时端口是空的）
systemctl restart esc-host
# 4. 验证
curl -sk -o /dev/null -w '%{http_code}\n' https://127.0.0.1:2222/
echo | openssl s_client -connect 127.0.0.1:2222 -servername cloud.tx.evencewu.asia 2>/dev/null \
  | openssl x509 -noout -subject
```

**回滚**（两条命令，秒级）：

```bash
systemctl stop esc-host && docker start esc-tls && systemctl start esc-host
# 同时把 host.toml 里的 [tls] 段去掉（或 enabled = false）
```

`esc-tls` 容器的 `RestartPolicy=unless-stopped`，所以手动 `docker stop` 之后
**重启机器不会把它拉起来**——回滚位不会自己复活去抢端口。

## 面板上能看到什么

原来的 nginx 那一层是**面板完全看不见**的部分：`2222` 这个端口是谁在听、转发到哪、
要不要登录，只能在几个页面之间来回切才拼得出来。

现在它是 `esc-host` 的一个插件，控制面跟着走完了同一条链路：

```text
面板「网络 › TLS 终结」
   ├─ GET /api/tunnel/tls       → 服务器面板转发 → 隧道面板 → HostCall("esc.tls", "tls.status")
   └─ GET /api/tunnel/mappings ─┐
                                └─▶ 两份数据在前端拼起来
```

拼的依据就一条：**`site.target` 与某条 mapping 的 `listen` 相等**。两条记录指向
同一个 `127.0.0.1:18790`，链路就接上了：

| 监听 | 站点 | 终结之后 | 隧道 | 链路终点 | 状态 |
|---|---|---|---|---|---|
| `0.0.0.0:2222` | `panel` 转发 | `127.0.0.1:18790` ↳ 就是这条映射的监听地址 | `4090-8080` / `evence-4090` | 由 `evence-4090` 决定 | `active` |

「链路终点」写「由 … 决定」而不是给一个地址，是因为 **hub 侧的 mapping 记录里
`local` 是空的** —— 目标地址在 spoke 那台机器上，握手时不上报。面板能看到的
链路终点止于隧道，这是一个该被看见的事实，不该编一个地址填上。

面板还显示三样在配置里容易被忘掉的东西：

* **`log_query`**：承载令牌的端口靠它保证 `?token=` 不落盘。它应该被看见。
* **证书文件路径**：续期后要重新复制到那里再重启，看见路径才想得起来。
* **握手失败计数**：零星几条是端口扫描，**持续增长**才是问题（证书过期、
  端口被安全组打断）。

控制面是**只读**的：没有 upsert/delete。证书与监听端口的改动要重启宿主才生效，
给一套只能改内存、活不过重启的写接口比没有更误导。

## 明确不做

| 不做 | 为什么 |
|---|---|
| **HTTP/2 / HTTP/3** | ALPN 只宣告 `http/1.1`。与 `esc-gateway` 的取舍一致：多路复用对面板没有收益，却要引入一套帧编解码 |
| **证书自动续期** | 那是 certbot 的事。这里只读盘上的 PEM |
| **gzip / 缓存 / 限流** | 不在这一层 |
| **多上游 / 负载均衡** | 一个端口一个上游。要分流用 `esc.edge-router` |
| **路径改写**（`strip_prefix`） | 上游看到的路径就是客户端发的 |

## HSTS 为什么默认关

HSTS 是按**主机名**记的、不区分端口。在非标准端口上发 HSTS，浏览器会把整个
主机名标成 HTTPS-only 并改写到 **443**；443 没通的话结果就是访问超时，页面根本
加载不出来。

原来的 `nginx.conf` 里留着同样的注释和同样关着的开关，这里照抄这个取舍。
443 在云安全组放行之后再把 `hsts` 改成 `true`。

## 已知观察（不是本模块的问题）

`https://<面板>/ws` 在**不带令牌**时也会返回 `101 Switching Protocols`。
这一条绕过本模块直连隧道端点（`http://127.0.0.1:18790/ws`）同样复现，所以是
EchoAgent Panel 自己的行为，与 TLS 层无关——原来的 nginx 是纯 TCP 转发，行为一样。

## 测试

```bash
cargo test -p esc-plugin-tls
```

- **22 条单元测试**：配置校验（重复端口、`upstream`/`redirect` 互斥、未知键报错、
  逐站点覆盖）、证书加载与 SNI 索引（重复 SNI 必须报错）、hop-by-hop 头剥离
  （含 `Connection: X-Foo` 这种 RFC 细节）、绑定失败要释放已绑端口
- **4 条控制面测试**：状态镜像配置（`target` 必须是上游地址原文——面板靠它
  把 TLS 与隧道接起来）、计数单调、空默认值不破坏序列化
- **13 条端到端测试**（`tests/e2e.rs`）：真 TLS 客户端 + 真上游，覆盖
  SNI 选证书（比对出示的证书 DER）、转发、`X-Forwarded-*` 覆盖伪造值、
  301 模板（含 `{hostname}` 去端口）、WebSocket 保留升级头、超限 413、
  上游 502 带地址、HSTS 开关、ALPN 只协商 `http/1.1`
