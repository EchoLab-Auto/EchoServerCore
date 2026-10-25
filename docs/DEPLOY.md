# 部署记录与注意事项

本文记录 2026-10-25 的实际部署过程，包含踩到的坑。配置示例见 `examples/`，
systemd 单元见 `deploy/`。

## 现状

| 角色 | 主机 | 状态 |
|---|---|---|
| Hub | cloud-server-2（公网 114.132.241.46 / 内网 10.1.0.8） | ✅ 已部署运行 |
| Spoke | ai-station（10.10.10.250） | ⏸ 二进制与证书就位，等安全组放行后启用 |
| CA | 开发者机器（离线保存 `ca.key`） | ✅ |

## 为什么用 CSR 流程而不是 `pki init`

第一次部署时我直接在一台机器上 `pki init` 生成了 CA + hub 证书 +
所有 spoke 证书，准备把文件拷到各机器。实际发现：

- 各机器之间**没有互通的 SSH**（只能经由人工/跳板转文本）；
- 私钥是 PEM 块，会经过对话、剪贴板、日志 —— 这是不可接受的。

于是补了 `pki keygen` / `pki sign`：**每台机器自己生成私钥，只把公开的
CSR 和签好的证书搬来搬去**。CA 私钥始终只在开发机上，从未落到公网机器。

## 踩坑记录

### 1. 私钥不能靠"手抄 PEM"

把 PEM 手工抄进 `cat > file <<'EOF'` 时，肉眼几乎不可能发现 base64 的
一个字符差异。第一次抄 `ca.pem` 得到的 sha256 与源文件不一致，
`openssl verify` 直接失败。

**正确做法**：一律用 `base64 -w0 源文件` 传输，落地后立刻比对 sha256。
本次所有证书搬移都按这个流程做，并在两边核对 hash。

### 2. 配置文件权限导致服务起不来

`esc-hub.service` 用 `User=esc` 运行，而配置文件默认 `600 root:root`，
表现为启动即退出、systemd 反复重启：

```
Error: cannot read /etc/esc/hub.toml: Permission denied (os error 13)
```

修复：`chown root:esc` + `chmod 640`。同理私钥用 `640 root:esc`。

### 3. 曾被误判为"云厂商无 hairpin NAT"（实为安全组）

一开始在 Hub 本机连接它自己的公网 IP `114.132.241.46:4433` 失败，我判断成
"腾讯云不做 NAT 回环"。**这个结论是错的** —— 安全组放行 4433 / 2222 之后，
从 Hub 本机访问 `114.132.241.46:2222` 立刻返回 `HTTP 200`，回环一直是通的。
当时的失败纯粹是安全组把 SYN 丢了，表现出来和"没有回环"一模一样。

教训：**在云上，"连不上自己的公网 IP" 有两种可能（安全组 / 无回环），
不能只凭现象断言。** 区分方法：从外部探针（check-host.net）测同一个端口，
若外部也不通 → 安全组；若外部通而本机不通 → 才是回环问题。

不过下面这条仍然值得保留，因为 hub 证书的 SAN 同时写公网和内网 IP
才让两种访问方式都能通过校验：

```bash
esc pki sign --ca-dir ... --kind server \
    --host 114.132.241.46 --host 10.1.0.8
```

### 4. 安全组默认不放行新端口

本机 `ss` 显示在监听，主机 iptables 也全 ACCEPT，但外网就是连不上。
用 check-host.net 的全球探针才能区分"本机没监听"和"云防火墙拦截"：

```
tcp:4433  -> 0/4 节点连通     ← 新端口，未放行
tcp:11010 -> 4/4 节点连通     ← 已有端口，放行
```

**结论：部署新端口后必须去控制台放行，并用外部探针验证，本机自测不算数。**

### 5. 某些内网机的出站 TCP 被透明代理接管

ai-station 上，对**任何**目标（包括 `198.51.100.7` 这种 TEST-NET-2
保留地址）做 `TCP connect` 都是 3~4 ms 内"成功"：

```
hub 4433                     OK   4ms
TEST-NET-2 198.51.100.7:4433 OK   3ms   ← 不可能成功的目标也成功
```

所以在这些机器上 `nc`/`telnet`/`/dev/tcp` 式的端口探测**全部不可信**，
会得到大量假阳性。判断可达性只能从第三方探针（check-host.net）或
真正跑协议握手。

这同时解释了为什么 spoke 从 ai-station 连 Hub 时报
`TLS handshake with hub failed`：TCP 握手是被本地代理解的，TLS 根本
没到对端。**解决顺序是先把安全组放行，再看是否仍失败**。

### 6. Spoke 可以完全无 root 运行

Hub 需要绑公网端口（4433），但 Spoke **只做出站连接**，不绑定任何特权端口。
所以 Spoke 可以用 systemd 用户单元跑，不需要 root、不需要 sudo：

```bash
install -m755 target/release/esc ~/.local/bin/esc
mkdir -p ~/.config/esc
# 证书放 ~/esc/，配置放 ~/.config/esc/spoke.toml
systemctl --user enable --now esc-spoke
loginctl enable-linger "$USER"     # 让它在没有登录会话时也活着
```

本次 ai-station 上就是这么装的（该机的 `echo-agent-core.service` 设了
`NoNewPrivileges=true`，任何 sudo 都被禁止，用户单元正好绕开这个限制而
不涉及提权）。

## 验证清单

部署后至少确认这几项：

```bash
# 1. 服务与端口
systemctl is-active esc-hub && ss -tlnp | grep -E ':4433|:2222'

# 2. 控制面强制 mTLS（应看到 alert 116 certificate required）
openssl s_client -connect 127.0.0.1:4433 -CAfile /etc/esc/ca.pem -alpn esc/1 < /dev/null

# 3. 外部可达性（本机自测不可信）
curl -s "https://check-host.net/check-tcp?host=<公网IP>:4433&max_nodes=4" -H "Accept: application/json"

# 4. 端到端：公网端口应能到达 spoke 后面的服务
curl -sv http://<公网IP>:<服务端口>/ 2>&1 | head
```

### 7. 一个必须知道的取舍：`panic = "abort"` 已移除

改造前 release 用 `panic = "abort"` 压体积。做控制面之后必须改成 `unwind`，
否则任意一条连接的转发任务 panic 会让**整个 hub 退出、所有隧道一起断**。
实测与体积代价见 [`ARCHITECTURE.md`](./ARCHITECTURE.md#4-为什么-release-刻意不用-panic--abort)。

如果你的部署脚本里显式加了 `-C panic=abort`（或用了自己的 profile），请去掉。

## 升级流程

```bash
git pull && cargo build --release -p esc-cli
install -m755 target/release/esc /usr/local/bin/esc
systemctl restart esc-hub          # 或 systemctl --user restart esc-spoke
```

Hub 重启期间 Spoke 会自动退避重连（实测 0.40 s 恢复，见 `TESTING.md`）。

## 生产实例（第一组映射）

```
公网 114.132.241.46:2222  ──►  Hub(cloud-server-2)  ──mTLS+yamux──►
     Spoke(evence-4090-24)  ──►  127.0.0.1:8080  (EchoAgent Panel)
```

Hub 侧配置（`/etc/esc/hub.toml`）：

```toml
[[service]]
name   = "4090-8080"
listen = "0.0.0.0:2222"
spoke  = "evence-4090"     # 固定到指定 spoke，别的机器无法顶替这个服务
```

Spoke 侧（4090，`~/.config/esc/spoke.toml`）：用户级 systemd 单元
`esc-spoke.service` + `loginctl enable-linger`，不依赖 root。

安装步骤（在 4090 上）：

```bash
# 1) 从已有节点经 overlay 拷二进制（避免重复编译）
scp 10.10.10.250:~/.local/bin/esc ~/.local/bin/esc    # 双向校验 sha256
# 2) 生成私钥 + CSR，把 CSR 拿到持 CA 的机器上签
~/.local/bin/esc pki keygen --out ~/esc --id evence-4090
# 3) 签好的 .pem 拷回 ~/esc/，连同 ca.pem
# 4) 写配置与 unit
~/.local/bin/esc check -c ~/.config/esc/spoke.toml --role spoke
systemctl --user enable --now esc-spoke
loginctl enable-linger "$USER"
```

## 面板化部署：把 `esc hub` 换成 `esc-host`（2026-10-25）

`esc hub` 仍然是可用的（行为没变），但想要**不重启就能改映射**，改用宿主：

```
esc-host（systemd）
  ├─ 进程内插件 esc.tunnel  → esc-engine（hub 角色，绑 :4433 与各映射端口）
  └─ 子进程插件 esc.tunnel-panel → HTTP :2223（只绑回环）+ 单页前端
```

### 1. 装二进制

```bash
cargo build --release --bin esc --bin esc-host --bin esc-plugin-tunnel-panel
install -m755 target/release/esc             /usr/local/bin/esc
install -m755 target/release/esc-host        /usr/local/bin/esc-host
install -d -m755 /usr/local/libexec/esc
install -m755 target/release/esc-plugin-tunnel-panel /usr/local/libexec/esc/esc-plugin-tunnel-panel
```

> 面板是宿主**拉起的子进程**，不是 systemd 服务。想升级面板就换这个文件再
> `systemctl restart esc-host`；想只换面板不影响隧道，可以调
> `PluginHandle::upgrade`（框架能力，目前没接 CLI）。

### 2. 配置

引擎配置 = 原来的 `hub.toml` 加两行（`docs/../examples/engine.toml` 有全文）：

```toml
role  = "hub"
store = "/etc/esc/mappings.toml"
```

宿主配置 `examples/host.toml`：

```toml
[engine]
config = "/etc/esc/engine.toml"

[panel]
enabled = true
command = "/usr/local/libexec/esc/esc-plugin-tunnel-panel"

[panel.config]
listen = "127.0.0.1:8787"
auth   = "admin:<口令>"
```

### 3. 权限（这一步不做会 repeat 坑 #2 与坑 #8）

映射表是**运行时状态**，放在 `/var/lib/esc/mappings.toml`（FHS 惯例），
由 unit 里的 `StateDirectory=esc` 自动创建目录并赋予 `User=esc` 的属主——
`StateDirectory=` 隐含一条 `ReadWritePaths`，所以 `ProtectSystem=strict` 下
这个目录仍然可写。`/etc/esc` 保持整体只读（配置与证书不该被写）。

```bash
# engine.toml 里：store = "/var/lib/esc/mappings.toml"
systemctl disable --now esc-hub      # 先停掉旧的静态 hub
systemctl enable --now esc-host      # StateDirectory 会自动建 /var/lib/esc
```

#### 坑 #10：`ReadWritePaths=<单文件>` 不够，原子写要的是**目录**可写

第一次部署时映射表放在 `/etc/esc/mappings.toml`，单元里写
`ReadWritePaths=-/etc/esc/mappings.toml`。结果是：**服务起得来、隧道正常、
面板能读，但一增删映射就 502**：

```json
{"error":"保存映射表失败: Read-only file system (os error 30)"}
```

原因是原子写的实现是「同目录建临时文件 → rename」（跨文件系统 rename 会
EXDEV，所以临时文件必须同目录）。只放开一个文件的可写权限，目录仍然是
`ProtectSystem=strict` 下的只读挂载，`File::create("mappings.tmp")` 直接 EROFS。

教训：**状态与配置要分开挂**。配置/证书 `/etc/esc` 只读，状态 `/var/lib/esc`
可写；不要为了省事把状态塞进配置目录再逐文件放开权限。

现在引擎对这两类错误（EACCES / EROFS）都会附上成因与修法，不再只给 `os error 30`。

首次启动会把 `engine.toml` 里的 `[[service]]` 播种进 `mappings.toml`，
日志里能看到 `seeded mapping store from static config`。

#### 坑 #8：不要用 root「试跑一下」esc-host

映射表用「临时文件 + `rename`」原子写，而 `rename` 会把文件属主重置成
**写它的那个进程**。所以：

```
# 以 root 手动跑一次（哪怕只是想看看 --help 修好没）
root# esc-host -c /etc/esc/host.toml
root# ls -l /etc/esc/mappings.toml
-rw------- 1 root root 268 ...        ← 属主变 root 了
root# systemctl start esc-host
     ↳ 无法读取映射表 ...: Permission denied (os error 13)   ← 服务起不来
```

2026-10-25 部署时就是这样踩了一次（那次靠自动回滚脚本救回了生产）。

- **`--help` / `--version` 不会写文件**（0.1.1 起参数解析严格，未知参数直接报错，
  不会再"用默认配置启动服务"）；
- 真要用 root 验证配置，用 `esc check -c ...`（只读，不写映射表）；
- 万一属主已经错了：`chown esc:esc /etc/esc/mappings.toml && chmod 600`，然后
  `systemctl restart esc-host`。

引擎现在对这类错误会附带属主/权限与修复命令，不再只给一句
`Permission denied (os error 13)`。

### 4. 验证

```bash
systemctl is-active esc-host
ss -tlnp | grep -E ':4433|:2222|:8787'     # 控制端口 / 映射端口 / 面板
curl -su admin:<口令> http://127.0.0.1:8787/api/status | jq
curl -su admin:<口令> http://127.0.0.1:8787/api/mappings | jq

# 运行时加一条映射，端口应立刻出现（不重启、不断已有连接）
curl -su admin:<口令> -X POST http://127.0.0.1:8787/api/mappings \
  -H 'content-type: application/json' \
  -d '{"name":"try","listen":"0.0.0.0:2300","local":"127.0.0.1:8080","enabled":true}'
ss -tlnp | grep 2300
```

外部可达性仍然要用第三方探针（见坑 #4），本机自测不算数。

### 5. 从外部访问面板

不要把 8787 直接开到公网。两种做法：

```bash
# 首选：SSH 隧道
ssh -L 8787:127.0.0.1:8787 root@114.132.241.46

# 或者：让 esc 自己把面板映射出来，但在前面加一层认证反代
#（注意这会让面板成为「公网上的管理面」，请确认口令强度）
```

### 6. 回滚

```bash
systemctl disable --now esc-host
systemctl enable --now esc-hub      # 旧的静态配置仍在，逐字节兼容
```

## 2026-10-25 在 cloud-server-2 的实际部署记录

按上文流程实做了一遍。记下真实过程、踩到的两个坑与最终数据。

### 构建（在 cloud2 上从源码构建，不传二进制）

cloud2 没有对外 SSH 入口，二进制没法 `scp`；而它自己有 cargo + GitHub 可达，
所以直接在机器上构建：

```bash
cd /opt/src/EchoServerCore
git fetch https://github.com:443/EchoLab-Auto/EchoServerCore.git main   # 见坑 #9
git merge --ff-only FETCH_HEAD
cargo build --release -j 2 --bin esc --bin esc-host --bin esc-plugin-tunnel-panel
```

| 项 | 实测 |
|---|---|
| 冷构建（含拉 60+ 新依赖） | 2m36s（2 vCPU / 2 GB，`-j 2`） |
| 增量构建（只改 esc-host） | 30s |
| 峰值内存 | ~400 MB（LTO 链接期），2 GB 机器安全 |

#### 坑 #9：gh-proxy 的 url 重写会给出**过期**的 main

cloud2 的 `/root/.gitconfig` 里有：

```
url.https://gh-proxy.com/https://github.com/.insteadof https://github.com/
```

它让 `git fetch https://github.com/...` 走国内镜像，而镜像的 main **滞后**：
`git fetch` 报 `Already up to date`，实际远端已有新提交。绕过办法是让
`insteadof` 匹配不上——加显式端口即可：

```bash
git fetch https://github.com:443/EchoLab-Auto/EchoServerCore.git main
```

（`https://github.com:443/` 不匹配 `https://github.com/` 前缀，于是直连 GitHub。
cloud2 到 github.com 本身通，只是慢一点。）

### 切换（带自动回滚）

```bash
systemctl disable --now esc-hub
systemctl enable  --now esc-host
# 20s 内 esc-host 没 active 或 4433 没监听 → 自动回滚：
#   systemctl disable --now esc-host && systemctl enable --now esc-hub
```

第一次切换**真的触发了回滚**（映射表属主被 root 写坏，见坑 #8），生产在
约 20s 内自动恢复，没有任何人工介入。修好后又撞上坑 #10（只读文件系统），
第三次才完全通过。

### 最终状态

| 检查项 | 结果 |
|---|---|
| `esc check -c /etc/esc/engine.toml --role hub` | ✅ 旧 hub.toml 兼容（`role`/`store` 被忽略） |
| `systemctl is-active esc-host` / `is-enabled` | ✅ active / enabled（`esc-hub` 已 disable） |
| 监听 | `0.0.0.0:4433` 控制、`0.0.0.0:2222` 映射、`127.0.0.1:8787` 面板 |
| 进程与内存 | `esc-host` RSS 6.5 MB（User=esc）+ `esc-plugin-tunnel-panel` RSS 4.4 MB |
| 面板认证 | 无口令 401；带口令 200，标题 `esc 隧道面板 · cloud-server-2` |
| 映射表 | `/var/lib/esc/mappings.toml`，`esc:esc 600`，由 `[[service]]` 播种 |
| 运行时增删映射 | ✅ POST 200 → `127.0.0.1:2399` 立即监听 → 落盘 → 停用 100ms 内释放 → 删除 |
| 重启韧性 | ✅ `systemctl restart esc-host` 后 2s 内面板恢复、spoke 0.5s 重连、2222 继续 200 |
| 端到端（本机侧） | ✅ 公网 2222 → spoke → 本机 8080，HTTP 200 / 5551 B / 32~130 ms |
| TCP 探针（check-host 6 节点） | ✅ 6/6 连通，166–288 ms |
| HTTP 探针（check-host 5 节点） | ✅ 5/5 `HTTP 200`，0.40–0.66 s |
| 日志 ERROR | 0 条 |

### 客户端侧同时升级

本机（`evence-4090`）的 spoke 从改造前的旧二进制升到同一版本，用户级单元
（`systemctl --user restart esc-spoke`）重连后 `granted=["4090-8080"]`，
端到端继续 200。

顺带验证了两件事：

1. **线协议向后兼容**——升级前，旧 spoke 已经连着新 `esc-host` 正常工作
   （`agent=esc/0.1.0`、`granted=["4090-8080"]`），说明改架构没动线上格式；
2. **升级只影响自己那侧**——先升 hub、再升 spoke，中间隧道只断了约 0.5s。

### 备份与回滚

| 文件 | 备份 |
|---|---|
| `/usr/local/bin/esc` | `/usr/local/bin/esc.bak-20261025` |
| `~/.local/bin/esc`（本机） | `~/.local/bin/esc.bak-20261025` |
| `/etc/esc/hub.toml` + `esc-hub.service` | 原地保留（未删未改） |

```bash
# 回滚整条链路
systemctl disable --now esc-host
systemctl enable  --now esc-hub      # 旧 unit 与 /etc/esc/hub.toml 都还在
```

面板凭据存在 cloud2 的 `/root/esc-panel-credentials.txt`（600 root）。外网访问
用 SSH 转发，不要把 8787 开到公网：

```bash
ssh -L 8787:127.0.0.1:8787 root@114.132.241.46
```

## 把面板暴露到公网（可选）

默认只听回环。要改成公网访问：

```bash
# /etc/esc/host.toml
[panel.config]
listen = "0.0.0.0:2223"
auth   = "admin:<20 位以上随机口令>"
allow_cidrs = ["<你的出口 IP 段>"]      # 强烈建议
max_auth_failures = 8                   # 默认值，可省略
auth_fail_window_secs = 60
```

```bash
systemctl restart esc-host
ss -tlnp | grep 2223                    # 确认监听
```

### 三个容易漏掉的前提

1. **云厂商安全组**要放行该端口。主机上 `ss` 看着在监听、外部却连不上，九成是
   安全组没开（面板自身不会碰防火墙）；
2. **别和映射端口撞车**。映射用的是 `2222` 这类公网端口，面板换个别的，
   否则引擎绑端口会报 `conflict`；
3. **改了 `listen` 记得看日志有没有那条 WARN**：面板在非回环地址上启动会打
   「管理面已暴露到网络」的警告，说明它确实生效了。

### 回退到只监听回环

```bash
sed -i 's/^listen = "0.0.0.0:2223"/listen = "127.0.0.1:8787"/' /etc/esc/host.toml
systemctl restart esc-host
```

## 2026-10-25：面板改为公网暴露（cloud2，端口 2223）

按上面的「把面板暴露到公网」实做了一遍，记录实测结果。

### 改动

```toml
# /etc/esc/host.toml
[panel.config]
listen = "0.0.0.0:2223"
auth   = "admin:<20 位随机口令>"
max_auth_failures     = 8
auth_fail_window_secs = 60
allow_cidrs = []          # 未设白名单：家用宽带 IP 会变，写死容易把自己关在外面
```

备份：`/etc/esc/host.toml.bak-<时间戳>`。

### 实测

| 检查项 | 结果 |
|---|---|
| 监听 | `0.0.0.0:2223`（esc-plugin-tunnel-panel），4433/2222 不变 |
| 启动 WARN | ✅ 打出「管理面已暴露到网络 / 口令以 Basic 明文传输」 |
| 无口令 | 401 |
| 带口令 | 200，能读 status / mappings |
| 经公网增删映射 | ✅ POST 200 → 绑定 `127.0.0.1:2398` → DELETE 200 → 列表回到 1 条 |
| 生产隧道未受影响 | ✅ 2222 继续 200 |
| 公网可达（check-host 6 节点） | ✅ 6/6，120–1207 ms |
| 限速：连发 8 次错口令 | 全部 401（预算用尽） |
| 限速：第 9 次用**正确**口令 | ✅ 429 + `retry-after: 58`（正确口令同样被挡，否则限速无意义） |
| 限速：伪造 `X-Forwarded-For: 5.6.7.8` | ✅ 仍 429（只认 socket 来源） |
| 窗口过期后 | ✅ 自动恢复 200，连续请求不再受限 |

主机侧**没有任何 iptables/nftables 规则**，过滤全在云厂商安全组；2223 是通的，
说明安全组已放行（如果外部超时而后端在监听，就是安全组的问题）。

### 为什么没开 `allow_cidrs`

观察到的出口是 `111.55.204.111`（spoke 的 peer 地址）。国内家用宽带多为动态
IP，写死网段后一旦换 IP 就把自己锁在外面，而恢复需要另想办法登进云主机。
所以这次只上「强口令 + 失败限速」，白名单留空；要收紧时改一行即可，方法写在
`PANEL.md` 与本机 `/root/esc-panel-credentials.txt` 里。

### 仍然存在的风险（如实记）

1. **明文 HTTP**：口令是 base64，链路上任何一跳都能读。需要更强保障就得加
   TLS 终结（nginx/Caddy + 域名），面板退回只听回环；
2. **面板权限很大**：能增删公网端口映射，等于能改内网暴露面；
3. **`/healthz` 也过认证层**，探针要么带 Basic、要么做 TCP 探活。

## 可靠的部署通道：git bundle + scp

cloud2 直连 github 极不稳定（多次 130 s 超时，`git fetch` 报
`Failed to connect to github.com port 443`）。但**它自己的 sshd 是通的**
（本机公钥在 `/root/.ssh/authorized_keys` 里）。所以用这条路更新源码：

```bash
# 本机（有完整仓库）
cd <repo> && git bundle create /tmp/esc.bundle main

# 本机 — 传到共享分区（不要用 /tmp，见下）
scp /tmp/esc.bundle root@<host>:/opt/src/esc.bundle

# 远端
cd /opt/src/EchoServerCore
git fetch /opt/src/esc.bundle main
git reset --hard FETCH_HEAD        # [local] 提交会被丢弃（上游已含等价内容）
```

实测：bundle 290 KB，scp **1.7 秒**完成——比 `git fetch` 反复超时快两个数量级。
校验 `sha256sum` 一致再 fetch，避免半包。

### 为什么传到 `/opt/src` 而不是 `/tmp`

`/tmp` 是 tmpfs，而且**在 agent 的沙箱里可能不是同一份**：sandbox 有自己的
mount namespace，`scp` 写进真机的 `/tmp`，而沙箱里的 `/tmp` 是另一份，
于是「scp 成功、文件不存在」。`/opt`、`/var`、`/etc` 在根分区，两边一致。

```bash
# 校验（远端）
ls -l /opt/src/esc.bundle && sha256sum /opt/src/esc.bundle
```

### 为什么不直接传二进制

可以（同一个 x86_64、旧 glibc 编译的二进制能在新系统上跑），但传源码有三个
好处：cloud2 自己 `cargo build` 保证依赖与本地一致；`node --check` 之类只能
在源码上跑；出问题时 `git log` 在机器上就是完整的。

只有在一台机器完全没装 Rust 时才值得传二进制。

## 前端 JS 的语法校验

面板前端是单文件 HTML（没有构建步骤），所以没有天然的类型/语法检查点。本机
没有 node，但 **cloud2 有 node v22**：

```bash
# 远端
python3 -c "
import re, pathlib
html = pathlib.Path('crates/esc-server-panel/web/index.html').read_text()
js = re.search(r'<script>(.*?)</script>', html, re.S).group(1)
pathlib.Path('/tmp/sp-check.js').write_text(js)
print('JS 行数:', len(js.splitlines()))
"
node --check /tmp/sp-check.js && echo "✅ 语法正确"
```

`node --check` 只验语法（不执行），但能抓住模板字符串里漏引号、括号不配对
这类会在浏览器里变成「整页白屏」的错误。**每次改前端都该跑一次**——
它的成本是两秒，而白屏的排查成本是半小时。

## 改名插件时三处必须同步

`esc-plugin-panel` → `esc-plugin-tunnel-panel` 这次改名在部署时验证到一个有用的
安全网：**宿主会校验子进程 `Hello` 里的 plugin id 与自己的配置是否一致**。

三处必须一起改，漏一处不会静默降级，而是直接起不来：

| 位置 | 内容 |
|---|---|
| 磁盘上的二进制 | `/usr/local/libexec/esc/esc-plugin-tunnel-panel` |
| `/etc/esc/host.toml` | `[panel] command = ".../esc-plugin-tunnel-panel"` |
| 代码里的 plugin id | 宿主 `TUNNEL_PANEL_PLUGIN_ID` 与插件 `PANEL_KEY` 两侧 |

只改了前两处、没改第三处时，日志会明确说出来：

```
ERROR echo_plugin_host::supervisor: plugin handshake failed
      plugin=esc.tunnel-panel
      error=protocol error: plugin id mismatch: expected "esc.tunnel-panel", got "esc.panel"
```

**这是好事**：如果框架不校验，宿主会以为插件起来了，然后所有 `HostCall` 打空。
现在的行为是「立刻失败并说清原因」，比静默错配好得多。

顺带一个 sed 的坑：改 `host.toml` 时我用
`s|.../esc-plugin-panel$|...|` 没匹配上——因为行尾还有一对双引号。
`$` 锚定的是行尾而非字符串结尾，**用 `cat -A` 看行尾**比猜快。
