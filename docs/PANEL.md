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
store = "/etc/esc/mappings.toml"

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
ssh -L 8787:127.0.0.1:8787 root@<公网服务器>
# 然后本机浏览器开 http://127.0.0.1:8787/
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

错误返回统一是 `{"error": "..."}`，状态码按错误类型给：参数问题 400、名字/端口
冲突 409、找不到 404、引擎不可用 502。所以脚本里 `curl -f` 就能区分成功与失败。

## 配置项

`[panel.config]`（原样透传给插件）：

| 键 | 默认 | 说明 |
|---|---|---|
| `listen` | `127.0.0.1:8787` | HTTP 监听地址；`:0` 表示由系统分配 |
| `auth` | 无 | `user:password`；绑非回环地址时**必填** |
| `title` | `esc 隧道面板` | 页面标题 |
| `behind_proxy` | `false` | 预留：声明自己在反代后面 |

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
