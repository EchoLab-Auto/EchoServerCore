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

### 3. 云厂商无 hairpin NAT

在 Hub 本机连接它自己的公网 IP `114.132.241.46:4433` 会失败 —— 腾讯云不做
NAT 回环。本机自测必须走内网 IP `10.1.0.8:4433`，因此 **hub 证书的 SAN
需要同时包含公网 IP 和内网 IP**：

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
