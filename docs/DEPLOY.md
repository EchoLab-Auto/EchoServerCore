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
  └─ 子进程插件 esc.panel   → HTTP :8787（只绑回环）+ 单页前端
```

### 1. 装二进制

```bash
cargo build --release --bin esc --bin esc-host --bin esc-plugin-panel
install -m755 target/release/esc             /usr/local/bin/esc
install -m755 target/release/esc-host        /usr/local/bin/esc-host
install -d -m755 /usr/local/libexec/esc
install -m755 target/release/esc-plugin-panel /usr/local/libexec/esc/esc-plugin-panel
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
command = "/usr/local/libexec/esc/esc-plugin-panel"

[panel.config]
listen = "127.0.0.1:8787"
auth   = "admin:<口令>"
```

### 3. 权限（这一步不做会 repeat 坑 #2）

映射表要能写，而单元里 `ProtectSystem=strict`：

```bash
touch /etc/esc/mappings.toml
chown esc:esc /etc/esc/mappings.toml
chmod 600 /etc/esc/mappings.toml
# 单元里已声明 ReadWritePaths=/etc/esc/mappings.toml，其余 /etc/esc 仍只读
systemctl disable --now esc-hub      # 先停掉旧的静态 hub
systemctl enable --now esc-host
```

首次启动会把 `engine.toml` 里的 `[[service]]` 播种进 `mappings.toml`，
日志里能看到 `seeded mapping store from static config`。

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
