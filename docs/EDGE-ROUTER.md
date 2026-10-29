# 边缘路由插件（`esc.edge-router`）

同一台机器上跑多个服务时，用端口号区分记不住。这个插件按 **HTTP `Host` 的
第一段**把请求分到本机不同端口：

```
http://cad.你的域名:2229/     → 127.0.0.1:2228   摆线针轮工具
http://panel.你的域名:2229/   → 127.0.0.1:8080   EchoAgent 面板
http://report.你的域名:2229/  → 127.0.0.1:9000   任何本机服务
```

只看第一段，所以 base domain 完全不影响路由 —— 换域名、加域名都不用改配置：

| 访问地址 | 提取的键 | 后端 |
| --- | --- | --- |
| `cad.echo-tech.com:2229` | `cad` | 2228 |
| `cad.whatever.cn:2229` | `cad` | 2228 |
| `CAD.Example.COM.:2229` | `cad` | 2228 |
| `cad.114.132.241.46.nip.io:2229` | `cad` | 2228 |
| `114.132.241.46:2229` | （IP 没有前缀） | 421，或 `default_port` |
| `localhost:2229` | （只有一段） | 421，或 `default_port` |

## 与 `esc-gateway` 的分工

两者都"把流量送到别处"，但**目标地址的来源**完全不同：

| | `esc-gateway` | `esc.edge-router` |
| --- | --- | --- |
| 做什么 | 把**远端**节点的端口映射到本机（经 mTLS 隧道） | 把本机**不同端口**按域名分开 |
| 目标地址来自 | 隧道注册表（可能是另一台机器） | 恒为 `127.0.0.1`（本机） |
| 面向 | 内网穿透 / 端口暴露 | 单机多服务的统一入口 |

两者可以叠加：`cad.example.com → 2228` 由本插件分流，而 `2228` 上的服务
本身可以是 `esc-gateway` 暴露出来的远端端口。

## 配置

宿主配置（`/etc/esc/host.toml`，或 `esc-host -c <path>`）：

```toml
[edge_router]
enabled = true            # 默认 false：它要 bind 端口，不能"默认打开"

[edge_router.config]
listen = ["0.0.0.0:2229", "0.0.0.0:80"]   # 也可以写单个字符串
default_port = 9999                        # 可选：无前缀时兜底
timeout_ms = 300000                        # 可选：转发超时，0 = 不设超时

[edge_router.config.routes]
cad = 2228                     # 前缀匹配
panel = 8080
"cad.special.com" = 2229       # 键里带点 → 完整域名精确匹配（优先级高于前缀）
```

规则：

- **键**：`[a-z0-9._-]`，不能以点开头/结尾；大写会被转成小写（配置文件是人写的，
  不该因为大小写失败）。`*` 之类不允许 —— 匹配是精确的，通配只会制造 502。
- **完整域名优先**：`cad.special.com` 命中时不会再看前缀 `cad`。
- **`listen` 必填**：缺失直接挂载失败。默认绑 `:80` 太危险，宁可让配置错误暴露。
- **配置错就挂载失败**（fail-closed）：一个"起来了但全 421"的边缘路由比没起来更难查。
- **持久化是宿主的职责**：运行期增删路由只在内存里生效，改完 TOML 要重启。

## 运行期控制面

插件把 `Arc<dyn EdgeRouterControl>` 注册到 `Ctx` 的
`esc.edge-router.control` 服务键下：

```rust
pub trait EdgeRouterControl {
    async fn status(&self) -> Result<RouterStatus, ControlError>;   // listen / routes / 计数
    async fn upsert_route(&self, prefix: &str, port: u16) -> Result<RouteView, ControlError>;
    async fn delete_route(&self, prefix: &str) -> Result<(), ControlError>;
}
```

宿主侧取用：`Host::edge_router()`（未启用时为 `None`）。错误用本仓统一的
`ControlError`：`ErrorKind` 自带 `http_status()`，面板可以直接把 `not_found`
映射成 404，不会出现两套码表。

> 面板要不要显示路由表、要不要让用户点一下加路由，取决于面板 —— 但**不需要
> 改这个插件**，它已经是一个服务了。

## 错误语义

| 状态码 | `error` 字段 | 什么时候 |
| --- | --- | --- |
| `421 Misdirected Request` | `no_route` | Host 没有对应路由（故意不用 404，避免与后端自己的 404 混淆） |
| `501 Not Implemented` | `upgrade_not_supported` | 请求带 `Upgrade`（WebSocket）—— 见下 |
| `502 Bad Gateway` | `bad_gateway` | 后端连不上（响应体里带目标端口） |
| `504 Gateway Timeout` | `upstream_timeout` | 超过 `timeout_ms` |

所有错误都是 JSON，`curl` 一眼能看出问题：

```console
$ curl -s -H 'Host: nope.example.com' http://127.0.0.1:2229/
{"error":"no_route","message":"该域名前缀没有配置路由。…","host":"nope.example.com","prefix":"nope"}
```

## 首部处理

- **逐跳首部全部丢弃**（`connection` / `keep-alive` / `te` / `trailer` /
  `transfer-encoding` / `upgrade` / `proxy-*`），以及 `Connection:` 里点名的
  任意首部（`Connection: x-secret` 时 `x-secret` 也是逐跳的）。
- **`Host` 原样保留**（含端口）：后端可能用它拼绝对链接。
- **`X-Forwarded-*` 由网关重算**：客户端带来的同名值一律丢掉。
- **`X-Forwarded-For` 是追加**（`客户端, 已有值`），这是反向代理的通行做法，
  代价是前面几项可被客户端伪造 —— **后端只能信任最后一项**。
- 多值首部（`Set-Cookie`）逐条保留，不会只剩一个。
- 说明：hyper 输出的首部名统一小写（HTTP 首部名本就大小写不敏感）。

## 已知限制（v1）

| 限制 | 说明 |
| --- | --- |
| **不支持 WebSocket / Upgrade** | 带 `Upgrade` 的请求直接回 `501`（附 JSON 说明），而不是挂着不动。需要 WS 的路径暂时走端口直连。v2 再做 `hyper::upgrade` 双向搬运。 |
| 只做 HTTP/1.1 | 没有 HTTP/2、没有 TLS 终结。要 HTTPS 就在前面放一层（见下）。 |
| 只连本机 | 目标恒为 `127.0.0.1:<port>`，不是通用出网代理（顺手关掉了 SSRF 面）。 |
| 路由表不持久化 | 运行期 `upsert_route` 只改内存。 |

## 与「备案拦截」的关系（腾讯云实测）

国内云厂商会按 `Host` 拦未备案域名（302 到 `dnspod.qcloud.com/webblock`），
**这与端口无关**：同一域名在 `:80` 和 `:2229` 上表现一致，只有 **HTTPS**
（Host 在 TLS 里，中间设备看不到）不触发。所以：

| 情况 | 做法 |
| --- | --- |
| 域名已 ICP 备案 | `http://cad.你的域名/` 直接用 |
| 域名未备案 | 走 HTTPS：在插件前面加一层 TLS 终结（nginx / `esc-tls`），或等插件支持 TLS |
| 自用 | 直接 `http://<ip>:<port>/`，不碰域名 |

> 坑：用**不存在的**假域名自测会得到"能用"的假阳性（不在备案库 → 放行）。
> 真实域名一解析就会被拦。

## 实现要点（改代码前读）

1. **`mount()` 里同步 bind。**
   `mount()` 是同步签名，而 bind 失败（端口被占、没有绑特权端口权限）是必须
   让宿主知道的错误。若像隧道插件那样把启动整个塞进 `spawn`，mount 只能报
   "成功"，错误要等第一次请求才暴露。bind 是微秒级操作，阻塞代价可忽略。

2. **`Disposer` 的 `Drop` 会真的撤销注册。**
   写测试时如果把 `PluginRegistry` 放在辅助函数里、只返回 `Ctx`，注册表一出
   作用域服务就消失了（框架语义：注册即副作用、必须可逆）。表现是
   `resolve()` 报 `service key not available`，看着像没挂上，其实是测试自己撤了。
   真实宿主当然一直持有注册表 —— 测试也要。

3. **`plugin id` 与 crate 名对应**：`esc-plugin-edge-router` ↔ `esc.edge-router`。

4. **默认不挂载**：`Host::start` 只在 `[edge_router] enabled = true` 时才
   `register`，没启用就完全不存在（不注册 → 不挂载 → 不占端口）。
