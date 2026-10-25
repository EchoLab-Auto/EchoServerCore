# Web 面板使用手册

面板是一个**独立子进程**（stdio 插件），由宿主 `esc-host` 拉起。它只通过
`HostCall("esc.control", <方法>, <参数>)` 跟隧道打交道，因此：

- 面板挂掉 / 重启时，已建立的隧道连接**不受影响**；
- 可以只升级面板，不碰隧道；
- 也可以完全不要面板（`[panel] enabled = false`），隧道照常运行。

结构见 [`ARCHITECTURE.md`](./ARCHITECTURE.md)。

## 启动

```bash
# 1) 引擎配置（旧 hub.toml 就能用，另加一个 store 指向映射表）
cat > /etc/esc/engine.toml <<'EOF'
role  = "hub"
store = "/var/lib/esc/mappings.toml"

[hub]
listen = "0.0.0.0:4433"

[tls]
ca_cert = "/etc/esc/ca.pem"
cert    = "/etc/esc/hub.pem"
key     = "/etc/esc/hub.key"
EOF

# 2) 宿主配置
cat > /etc/esc/host.toml <<'EOF'
[engine]
config = "/etc/esc/engine.toml"

[panel]
enabled = true
command = "/usr/local/libexec/esc/esc-plugin-panel"

[panel.config]
listen = "127.0.0.1:8787"
auth   = "admin:换成你自己的口令"
title  = "隧道面板"
EOF

# 3) 起宿主
esc-host -c /etc/esc/host.toml
```

打开 `http://127.0.0.1:8787/`，浏览器会弹 Basic 认证。

> 面板默认只绑回环。**如果想绑 `0.0.0.0`，必须配 `auth`** —— 否则插件在
> `configure` 阶段就拒绝启动（fail-closed），宿主日志里会看到
> `Failed{invalid_config}` 与原因。理由见文件末尾「安全」。

想从别的机器访问，推荐用 SSH 端口转发而不是直接暴露：

```bash
ssh -N -f -L 18787:127.0.0.1:8787 root@<公网服务器>
# 然后本机浏览器开 http://127.0.0.1:18787/
```

`-N` 只做转发不开 shell，`-f` 放后台。本地端口**不一定用 8787**——很多人
本机 8787 上已经跑着别的东西（部署这套时就是撞上了 EchoAgent XR），
真实情况是端口冲突后 `ssh` 只在 stderr 里吐一句
`bind [127.0.0.1]:8787: Address already in use`，而 `-f` 已经返回 0，
看起来像成功了、实际访问到的是别的服务。所以：**先 `ss -tln | grep 18787`
确认本地端口空闲**，或者干脆选个不常用的高位端口。

把转发写进 `~/.ssh/config` 更省事，以后只敲 `ssh -N -f cloud2`：

```
Host cloud2
  HostName <公网IP>
  User root
  LocalForward 18787 127.0.0.1:8787
  ServerAliveInterval 30
```

## 界面能做什么

| 区域 | 作用 |
|---|---|
| 顶部 | 角色（hub/spoke）、连接状态、运行时长、刷新 / 重新加载 / 导出 / 新建 |
| 概览卡 | 控制端口（hub）或 hub 地址（spoke）、引擎版本、生效映射数 |
| 映射表 | 服务名、公网监听、转发目标、状态、提供者、连接数 / 上下行流量、启用-编辑-删除 |
| spoke 表（hub） | 每个连接的对端地址、版本、已授权服务、在线时长、踢下线 |
| 导出 | 生成 `hub.toml` / `spoke.toml` 快照，可下载 |

状态含义：

| 状态 | 含义 |
|---|---|
| `active` | 端口已绑定（hub）或已声明给 hub（spoke） |
| `disabled` | 明确停用，不占端口 |
| `pending` | 配置为启用但还没生效（例如引擎刚重启） |
| `error` | 生效失败，悬停可以看到原因（端口被占用、地址非法…） |

页面每 3 秒自动刷新；编辑弹窗打开时暂停刷新，避免覆盖你正在输入的内容。

### 新建映射时填哪几个字段

同一条记录描述两端，按这台机器跑的角色填：

| 字段 | hub 角色 | spoke 角色 |
|---|---|---|
| `name` | 必填，要和 spoke 侧一致 | 必填，要和 hub 侧一致 |
| `listen` | 必填，如 `0.0.0.0:2222` | 忽略 |
| `local` | 忽略（仅展示） | 必填，如 `127.0.0.1:8080` |
| `spoke` | 可选，钉死到某个 spoke id | 忽略 |

**hub 侧加了映射但界面显示「提供者」为空**，说明当前没有任何 spoke 声明过这个
名字：去 spoke 那边把同名映射加上（spoke 会重连一次），或者点「踢下线」逼它
重新握手。

## REST API

所有端点都是 JSON。除 `/healthz` 外都要过 Basic 认证（如果配置了口令）。

| 方法 | 路径 | 等价的控制方法 |
|---|---|---|
| `GET` | `/api/status` | `status` |
| `GET` | `/api/mappings` | `mapping.list` |
| `POST` | `/api/mappings` | `mapping.upsert`（新建，`id` 为空） |
| `POST` | `/api/mappings/{id}` | `mapping.upsert`（更新，URL 里的 `id` 覆盖 body） |
| `DELETE` | `/api/mappings/{id}` | `mapping.delete` |
| `POST` | `/api/mappings/{id}/enabled` | `mapping.set_enabled`（body `{"enabled":bool}`） |
| `POST` | `/api/reload` | `engine.reload`（从磁盘重读映射表） |
| `POST` | `/api/spokes/kick` | `spokes.kick`（body `{"id":"..."}`，省略 = 全部） |
| `GET` | `/api/export?role=hub\|spoke` | `config.export` |
| `GET` | `/healthz` | —（容器探活用，也走认证） |
| `GET` | `/` | 单页前端 |

例子：

```bash
# 看状态
curl -u admin:口令 http://127.0.0.1:8787/api/status

# 加一条映射：公网 2222 → 内网 8080
curl -u admin:口令 -X POST http://127.0.0.1:8787/api/mappings \
  -H 'content-type: application/json' \
  -d '{"name":"4090-8080","listen":"0.0.0.0:2222","local":"127.0.0.1:8080","enabled":true}'

# 停用（端口立即释放）
curl -u admin:口令 -X POST http://127.0.0.1:8787/api/mappings/<id>/enabled \
  -H 'content-type: application/json' -d '{"enabled":false}'

# 导出成可以直接给 esc hub 用的静态配置
curl -u admin:口令 'http://127.0.0.1:8787/api/export?role=hub' | jq -r .toml
```

错误返回统一是 `{"error": "..."}`。状态码由**契约里**的
`ErrorKind::http_status()` 决定（面板不维护第二份表）：

| `ErrorKind` | HTTP | 典型原因 |
|---|---|---|
| `invalid` | 400 | 地址格式错、缺必填字段 |
| `not_found` | 404 | 映射 id 不存在 |
| `conflict` | 409 | 服务名 / 监听地址重复，或端口被占用（`bind`） |
| `unavailable` | 503 | 引擎没起来 / 角色不支持该操作 |
| `other` | 502 | 其他引擎错误 |
| （超时） | 504 | 引擎 10s 未响应 |

所以脚本里 `curl -f` 就能区分成功与失败。

> 超时定为 10 秒（框架默认 30 秒）：引擎侧的操作都是毫秒级的，30 秒只会让
> 人在真的卡住时干等多半分钟。

## 配置项

`[panel.config]`（原样透传给插件）：

| 键 | 默认 | 说明 |
|---|---|---|
| `listen` | `127.0.0.1:8787` | HTTP 监听地址；`:0` 表示由系统分配 |
| `auth` | 无 | `user:password`；绑非回环地址时**必填** |
| `title` | `esc 隧道面板` | 页面标题 |
| `behind_proxy` | `false` | 预留：声明自己在反代后面 |

## 运维：面板可能「永久停摆」，但隧道不会

插件框架给崩溃的子进程插件最多 **5 次**自动重启（退避 200ms→2s）。用完预算
后 supervisor 把它钉在 `Failed` 并且**不再尝试**，面板端口从此消失——而隧道
完全不受影响，映射照常转发。

这意味着一个很隐蔽的故障模式：**面板打不开，但没人知道**。为了不让它悄悄
烂掉，宿主在检测到面板进入 `Failed` 时会打一条 ERROR：

```
面板插件已停摆（重启预算耗尽）——隧道仍在正常转发，但 Web 面板不再可用。
排查原因后重启本服务即可恢复：systemctl restart esc-host。
```

看到这条就说明：

1. 隧道**没事**（不用急着救火，用户的连接没断）；
2. 面板是**反复崩了 5 次**才停的，往上翻 `journalctl -u esc-host` 找前 5 次
   的崩溃原因（通常是端口被占、配置写错、磁盘满）；
3. 修掉那个原因，然后 `systemctl restart esc-host`。

想主动发现，最简单的监控就是探一下 `/healthz`：

```bash
curl -fsS -u admin:口令 http://127.0.0.1:8787/healthz || echo "面板挂了"
```

（`/healthz` 也过认证层。这样设计是为了让「未认证的探测」与「未认证的真实
访问」表现一致，避免给人「这个端点没有防护」的错觉。如果你的探针不方便带
Basic 认证，用 TCP 端口探测即可。）

## 安全

1. **口令不是可选项的前提是回环绑定**。插件在 `configure` 阶段会校验：
   `listen` 不是回环且没有 `auth` → 直接拒绝启动。这条检查放在插件而不是宿主，
   是为了让「谁决定暴露什么」这件事留在有网络能力的那一层。
2. **口令用常量时间比较**，避免时间侧信道（虽然收益很小，但没有理由不做）。
3. **面板不解析隧道流量**，它只改配置。真正暴露到公网的是映射端口，不是面板。
4. **面板 API 能看到完整的基础设施清单**（映射名、内网地址、在线节点），
   所以它和 `Esc` 的管理面同级——别把它当公开站点。
5. 需要更强的认证/审计，请在面板前面放 nginx/Caddy（TLS + OIDC + access log），
   面板本身不适合承担这些。
