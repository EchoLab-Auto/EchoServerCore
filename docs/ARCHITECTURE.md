# 架构：插件化的隧道内核 + 可替换的 Web 面板

本文说明 2026-10-25 那次重构后的结构：**隧道内核与图形界面被拆成两个互不
依赖的进程**，中间由一个极薄的宿主（host）用插件框架接起来。

改造前的状态是「一个二进制 + 静态 TOML」：`esc hub -c hub.toml` 在启动时
把 `[[service]]` 全部绑上公网端口，此后没有任何运行时控制面。想加一个映射
只能改文件 + 重启（而重启会断开所有已建立的用户连接）。

## 一张图

```
                    ┌─────────────────────── esc-host（宿主进程）───────────────────────┐
   浏览器            │                                                                 │
     │  HTTP        │  Ctx（服务定位器）                                               │
     ▼              │   └─ ServiceKey<Arc<dyn TunnelControl>>  "esc.tunnel.control"    │
┌─────────────────┐ │        ▲                           ▲                            │
│ esc-plugin-panel│ │        │ provide（mount 时）        │ require                     │
│  （子进程插件）  │ │  ┌─────┴───────────┐        ┌──────┴────────────┐               │
│  axum + 单页前端 │ │  │ 进程内插件       │        │ 宿主服务          │               │
│                 │ │  │ esc.tunnel      │        │ esc.control       │               │
│  REST 端点       │ │  │ (esc-engine)    │◀───────│ HostCall 的落点    │               │
└────────┬────────┘ │  └─────────────────┘        └───────────────────┘               │
         │ HostCall  │          │                                                      │
         └───────────┼──────────┘                                                      │
                     │          │ 真的绑端口 / mTLS / yamux / 转发                     │
                     │          ▼                                                      │
                     │   es[c] hub 引擎 ────────── mTLS + yamux ─────────▶ spoke 引擎    │
                     └─────────────────────────────────────────────────────────────────┘
```

三层的职责边界：

| 层 | 认识什么 | 不认识什么 |
|---|---|---|
| `esc-plugin-panel`（子进程） | `esc-control-api` 的方法名与 JSON 形状、HTTP | 隧道、TLS、yamux、`esc-engine`、宿主 |
| `esc-host`（宿主） | 插件框架、`Ctx`、`esc-control-api` 的请求信封 | HTTP、隧道实现、yamux |
| `esc-plugin-tunnel`（进程内） | `esc-engine`、`esc-control-api` | HTTP、宿主、插件框架之外的东西 |

`esc-engine` 则完全不认识「插件」「Web」这些词——它只提供一个
`TunnelControl` trait 的实现。CLI（`esc hub`）和插件用的是同一份内核。

## crate 地图

```
crates/
  esc-common/        配置、TLS、线协议、yamux 驱动、PKI         （未改动）
  esc-engine/        ★ 新增：可编程内核
      hub/           动态公网监听：运行时 add/remove、每映射计数器
      spoke.rs       可变服务表 + 变配置自动重新握手
      store.rs       映射表（TOML 持久化，原子写）
      config.rs      兼容旧 hub.toml/spoke.toml 的引擎配置
      pipe.rs        带计数的双向搬运（替代 copy_bidirectional）
  esc-control-api/   ★ 新增：控制面契约（DTO + 方法名 + 信封 + trait）
  esc-plugin-tunnel/ ★ 新增：进程内插件，把引擎注册进 Ctx
  esc-plugin-panel/  ★ 新增：子进程插件，HTTP + 单页前端
  esc-host/          ★ 新增：组合根（宿主二进制 + 可复用 lib）
  esc-hub/           保留：`esc hub` 的薄封装（行为不变）
  esc-spoke/         保留：`esc spoke` 的薄封装（行为不变）
  esc-cli/           保留：`esc` 二进制（hub / spoke / check / pki）
```

## 三个关键决策

### 1. 为什么隧道在进程内，面板在子进程

两者的失败模式不同：

- **隧道是数据面**。它一重启，所有已建立的用户连接就断了。所以它跟宿主同生
  共死，不引入额外的进程边界和帧编解码延迟。
- **面板是控制面**。它崩了只是没人能改配置，流量照走。放进独立子进程后，
  它还能被单独重启 / 单独升级（`PluginHandle::upgrade`），完全不碰数据面。

反过来做（隧道子进程序、面板进程内）会把最需要稳定的一层放到最容易被误杀的
位置——不划算。

### 2. 为什么要有 `esc-control-api` 这层契约

因为「解耦」必须是**编译期**成立的，而不是靠约定。面板的 `Cargo.toml` 里没有
`esc-engine`、没有 `rustls`、没有 `yamux`：

```bash
$ cargo tree -p esc-plugin-panel --depth 1
esc-plugin-panel
├── axum
├── echo-plugin-sdk      # 插件侧运行时
├── esc-control-api      # ← 唯一的领域依赖：只有 serde 结构体和方法名
└── ...
```

契约本身只有两样东西：**方法名**（`mapping.upsert` …）和**JSON 形状**
（`Mapping` / `MappingView` / `EngineStatus`）。所以：

- 面板可以整个扔掉，隧道不受影响；
- 面板可以换成另一个实现（Go/Python/纯静态页 + curl 都行），只要讲同一套方法；
- 引擎可以在内部随便重构，只要不动契约。

`esc-control-api::dispatch` 是一个单点分发表：面板能调到的每个方法，都对应
`TunnelControl` 上的一个真实方法。加方法时漏了实现会编译失败，不会出现
「文档里有、实际 404」。

### 3. 为什么宿主用 `HostCall` 而不是直接调函数

面板在另一个进程里，能用的通道只有一条：插件框架的 `HostCall`
（`service="esc.control"` + `method=<控制方法>`）。宿主把这条调用转成
`dispatch(ControlRequest)`，落到 `Ctx` 里的引擎服务上。

代价是每次 API 调用多两次 JSON 编解码（面板 → 宿主 → 引擎），在本地管道上
是亚毫秒级；换来的是面板崩溃不会带走隧道、面板可以单独升级。

> 框架的 `HostService::call` 是**同步**的，而引擎 API 是异步的。宿主里唯一
> 一处 `block_in_place` + `block_on` 就是为此存在的（要求 multi-thread 运行时）。

### 4. 为什么 release 刻意不用 `panic = "abort"`

改造前 `[profile.release]` 里是 `panic = "abort"`，理由是体积。做控制面之后
这个取舍反过来了——隧道是**数据面**：

| 场景 | `abort` | `unwind`（现在） |
|---|---|---|
| 某条连接的转发任务里有个 bug/越界 | 整个 hub **进程退出**，所有映射端口消失、所有连接断掉 | tokio 隔离该任务，其余连接继续转发 |
| 面板插件里 panic | 只死面板进程（子进程），supervisor 按退避重启 | 同 |
| 宿主某处 panic | 隧道与面板一起没了 | 主任务退出（同样会停），但转发任务不受牵连 |

实测（同样的代码，只改 profile）：

```
=== debug（默认 unwind）===
坏任务的 JoinHandle 结果: Err（已被隔离）
健康任务仍在运行: 0 … 4
进程存活到结束 ✅                      exit=0

=== release + panic=abort ===
（没有任何输出）                        exit=134  Aborted (core dumped)
```

`abort` 下连健康任务的输出都没来得及打印——这就是「一个坏连接带走全部隧道」。
代价是三个二进制各大约 +13~15%（`esc` 3.10 MB → 3.51 MB，`esc-host` 3.08 MB →
3.55 MB，`esc-plugin-panel` 1.52 MB → 1.74 MB），对一台服务器上的常驻进程来说
可以忽略。

配套地，数据面上的 `expect()` 也都改成了优雅降级（状态查询路径宁可返回一条
`error` 视图，也不要 panic 掉进程；锁中毒时放弃记录句柄而不是 panic）。

## 运行时能做什么

改造后在**不重启**的前提下可以：

| 操作 | 怎么生效 |
|---|---|
| 新增映射 | 立刻 `bind` 公网端口；已连接的 spoke 只要在 `Hello` 里声明过这个名字，**无需重连**就被授权（`Registry::grant`） |
| 删除 / 停用映射 | 取消监听任务 → 端口立即释放 → 从注册表摘掉该服务 |
| 改映射的目标 | hub 侧重绑端口；spoke 侧改 `local` 表（后续开流立即用新目标） |
| 查询状态 | 每个映射的连接数 / 活跃数 / 上下行字节数（增量计数，不必等连接关闭） |
| 踢掉某个 spoke | 取消它的控制连接 → spoke 按退避策略自动重连并重新握手 |
| 从磁盘重载映射表 | 手工编辑 `mappings.toml` 后点「重新加载」 |

### 一个映射为什么能同时描述两端

```toml
[[mapping]]
name    = "4090-8080"
enabled = true
listen  = "0.0.0.0:2222"     # hub 侧用这个
local   = "127.0.0.1:8080"   # spoke 侧用这个
spoke   = "evence-4090"      # 可选：钉住 provider
```

一条记录就能在面板上显示「公网 2222 → 内网 8080」这条完整链路，也能据此导出
任意一侧的静态配置。角色决定哪几个字段生效：hub 只读 `listen`/`spoke`，
spoke 只读 `local`。

## 兼容性

- `esc hub -c hub.toml` / `esc spoke -c spoke.toml` **行为不变**：原来的
  `[[service]]` 会被当成「初始映射」，且这种模式下不落盘（配置就是事实）。
- 引擎配置文件是旧 TOML 的**超集**：`role` 可省略（按 `[hub]`/`[spoke]` 推断），
  新增 `store`、`[[mapping]]` 两个可选键。
- 线协议 `PROTO_VERSION` 仍是 1，ALPN 仍是 `esc/1`。老 spoke 能连新 hub。
- 新增的运行时可观测字段都在 `MappingView` / `EngineStatus` 里，缺失字段一律
  `#[serde(default)]`，三个进程版本不一致也能互相解码。

## 想扩展的时候

| 想做的事 | 改哪里 |
|---|---|
| 加一个控制方法 | `esc-control-api`：加方法名 + `TunnelControl` 方法 + `dispatch` 分支 → 引擎实现 → 面板加 HTTP 路由 |
| 换前端 | 只改 `esc-plugin-panel/web/index.html`（单文件，无构建步骤） |
| 换个面板实现（别的语言） | 讲同一套 `HostCall("esc.control", method, params)` 即可，宿主不用改 |
| 加一个新插件（比如证书签发 UI） | 写一个 `PluginHandler` 子进程插件 + 在宿主里 `supervisor.start(...)`；它通过 `HostCall` 拿数据 |
| 换隧道实现 | 在 Ctx 里注册同一个 `ServiceKey<Arc<dyn TunnelControl>>`，面板完全不用知道 |

## 已知边界

- 面板目前只管**映射**，不签发证书（`esc pki` 仍是 CLI）。
- 映射表是单文件、单进程写；没有多副本一致性。Hub 仍是单点。
- 面板的认证只有 HTTP Basic（够用，但不是 SSO）；默认只允许绑回环，绑公网
  地址必须配口令，否则插件拒绝启动。
- 没有审计日志：谁在什么时候改了哪条映射，目前只体现在 journald 的引擎日志里。
