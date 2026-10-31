# 服务器面板（server-panel）

一个看主机指标、管 Docker 容器的 Web 面板。默认端口 `2224`。

它是**独立进程**，不是 `esc-host` 的插件——理由在下面「为什么不做成插件」。
和隧道面板（`esc-plugin-tunnel-panel`）共享同一套访问控制（`esc-panel-auth`），
但两者互不依赖：隧道面板挂了不影响服务器面板，反之亦然。

```
esc-host        ── 隧道 + 端口映射面板（:2223）
esc-server-panel ── 主机指标 + Docker 管理（:2224）   ← 本文
```

## 界面有什么

| 区块 | 内容 |
|---|---|
| CPU | 总使用率、每核柱状、负载 1/5/15、核数。多核下进程 CPU% 可超过 100% |
| 内存 | 已用/总量、可用、缓存/缓冲、Swap。**已用 = 总量 − 可用**（不是 − 空闲） |
| 网络 | 各接口累计与瞬时速率（排除回环）、总收发 |
| 系统 | 主机名、发行版、内核、架构、运行时长 |
| 磁盘 | 每个真实块设备的挂载点/设备/类型/使用率，`tmpfs`/`overlay` 等伪文件系统已过滤 |
| Docker | 容器列表（状态灯、镜像、端口映射、Compose 项目分组、整栈启停）、启停/重启/暂停、**创建**、**删除**、**详情**、**改名**、**改重启策略**、增量日志、实时资源 |
| 镜像 | 列表（标签/ID/大小/时间）、拉取（后台任务 + 进度）、删除 |
| Docker 网络 | 列表（按 内部/自定义/系统 分组，标出驱动与网段）、详情与成员容器、**创建**、**删除**、容器**接入/断开** |
| 进程 | Top 20（按 CPU 降序），PID/名称/CPU%/RSS/线程/用户/命令行 |

页面每 3 秒轮询一次主机指标，每 6 秒刷新一次容器列表。前端是 Vue 3 + Vite
构建的（见文末「ui-frame 组件化前端」），产物嵌进二进制，运行时不需要 Node。

## 为什么不做成 `esc-host` 的插件

`esc-host` 的沙箱是给数据面设计的：`User=esc`、`ProtectSystem=strict`、
`PrivateDevices`、`SystemCallFilter=@system-service`。而服务器面板要读
`/var/run/docker.sock` ——**在 Docker 里，能访问这个 socket 就等于 root**
（可以起一个 `--privileged -v /:/host` 的容器）。

把这两件事放进同一个进程，等于让「转发 TCP 的程序」拥有 root 等价权限。
隧道是我们最希望保持"无聊"的组件：它只需要网络和证书，不应该有任何其他能力。

所以拆开：

```
esc-host         User=esc         无 docker socket、文件系统只读
esc-server-panel User=esc-monitor docker 组（能做容器操作，但不是 root）
```

即使服务器面板被攻破，攻击者拿到的是 docker 组权限（实际仍可提权到 root，
但需要额外一步利用）；而隧道进程完全不受影响。

## 部署

```bash
# 1. 装二进制
install -d -m 0755 /usr/local/libexec/esc
install -m 0755 esc-server-panel /usr/local/libexec/esc/esc-server-panel

# 2. 专用用户 + docker 组（没有 docker 的话跳过 usermod）
useradd --system --no-create-home --shell /usr/sbin/nologin esc-monitor 2>/dev/null || true
usermod -aG docker esc-monitor

# 3. 配置
cat > /etc/esc/server-panel.toml <<'EOF'
listen = "0.0.0.0:2224"
title  = "服务器面板"
auth   = "admin:<28 位随机口令>"
allow_cidrs = []
max_auth_failures = 8
auth_fail_window_secs = 60
docker_socket = "/var/run/docker.sock"
allow_actions = true
EOF
chown root:esc-monitor /etc/esc/server-panel.toml
chmod 640 /etc/esc/server-panel.toml

# 4. 起服务
install -m 0644 esc-server-panel.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now esc-server-panel
```

代码里的口令：

```bash
openssl rand -base64 24 | tr -d '/+=' | cut -c1-28
```

## 配置项

| 键 | 默认 | 说明 |
|---|---|---|
| `listen` | `127.0.0.1:2224` | HTTP 监听地址 |
| `title` | `服务器面板` | 页面标题 |
| `auth` | 无 | `user:password`。**非回环监听时必填**，否则拒绝启动 |
| `allow_cidrs` | `[]` | 来源白名单：IPv4 CIDR 或精确 IP。空 = 不限 |
| `max_auth_failures` | `8` | 同一来源在窗口内的失败次数上限，超出 429 |
| `auth_fail_window_secs` | `60` | 失败计数窗口 |
| `docker_socket` | `/var/run/docker.sock` | Docker socket 路径 |
| `allow_actions` | `true` | 启停 / 重启 / 暂停 / 改名 / 改重启策略；把容器接入或断开网络 |
| `allow_create` | `true` | 创建容器、拉镜像；创建 Docker 网络 |
| `allow_delete` | `true` | 删除容器 / 镜像 / 网络（不可撤销，单独一道闸） |

未知键会**报错**而不是忽略——`allow_action = false` 这种拼写错误如果被静默
忽略，会变成「我明明关了啊」的谜案。

## REST API

全部需要认证（`/healthz` 也要，这样「未认证探测」与「未认证访问」表现一致）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/` | 页面 |
| GET | `/healthz` | `ok`（探活用） |
| GET | `/api/overview` | 主机指标 + Docker 摘要（前端轮询这个） |
| GET | `/api/host` | 只有主机指标 |
| GET | `/api/docker/containers` | 容器列表（含已停止的） |
| GET | `/api/docker/containers/{id}` | 单容器详情（原始 Docker JSON） |
| GET | `/api/docker/containers/{id}/logs?tail=300` | 日志尾部（已解码复用流） |
| GET | `/api/docker/containers/{id}/stats` | 单次资源采样 |
| GET | `/api/docker/images` | 镜像列表 |
| POST | `/api/docker/containers/{id}/{action}` | `start` / `stop` / `restart` / `pause` / `unpause` |
| POST | `/api/docker/containers` | 创建容器（body 见下） |
| DELETE | `/api/docker/containers/{id}` | 删除（强制停止后删） |
| GET | `/api/docker/containers/{id}/detail` | 结构化详情（env/挂载/网络/重启策略…） |
| POST | `/api/docker/containers/{id}/rename` | `{"name": "..."}` |
| POST | `/api/docker/containers/{id}/restart-policy` | `{"policy": "unless-stopped"}` |
| GET | `/api/docker/networks` | 网络列表：`{"networks": [...], "names": [...]}` |
| GET | `/api/docker/networks/{id}` | 单个网络 + 成员容器 |
| POST | `/api/docker/networks` | 创建网络（body 见下） |
| DELETE | `/api/docker/networks/{id}` | 删除网络 |
| POST | `/api/docker/networks/{id}/connect` | `{"container": "...", "ipv4": "..."}`（ipv4 可省） |
| POST | `/api/docker/networks/{id}/disconnect` | `{"container": "...", "force": false}` |
| POST | `/api/docker/images/pull` | `{"image": "nginx:alpine"}` → 立刻返回 `{"job": "..."}` |
| DELETE | `/api/docker/images/{name}` | 删除镜像 |
| GET | `/api/docker/jobs` | 后台任务列表 |
| GET | `/api/docker/jobs/{id}` | 单个任务状态（`running`/`done`/`failed`） |

Docker 不可用时这些端点返回 **503**（服务不可用），而不是 500——前者说
「这个功能在此环境不存在」，后者说「我们写错了」。

### 能力边界：做到了哪，以及刻意没做什么

**有的**：容器启停/重启/暂停、创建、删除、改名、改重启策略、日志、结构化详情；
镜像拉取与删除；**网络查看、创建、删除、容器接入/断开**。

**刻意没有的**：

| 不做 | 为什么 |
|---|---|
| `exec` | 等于给面板一个 root shell，没有任何收敛的余地 |
| volume 管理 | 删卷就是删数据，收益远小于风险；请用 CLI |
| 改容器网络以外的运行参数 | 等价于重建容器，面板假装能改只会让人误以为改成了 |
| 网络驱动 overlay / macvlan / ipvlan | overlay 要 swarm，另两个要指定父网卡与 vlan，都不是「点两下」能讲清的东西 |
| 修改已存在网络的子网 | Docker 本身就不支持；重建才是唯一路径 |
| 批量 prune | 一条命令删一堆东西，和面板「看清楚再动手」的定位冲突 |

容器动作的允许清单在 `docker.rs` 的 `ALLOWED_ACTIONS` 里，有一条测试专门
锁住它——想加东西必须改测试并解释。

理由：面板的定位是「看见不对的东西并把它拉起来」，外加少量「隔离一组容器」
的网络操作。每多一个写操作就多一个可被滥用的入口，而「删容器」和「起一个
特权容器」在效果上都比「重启」严重得多，却只省一次 SSH。

## 安全模型

### docker socket = root

这不是夸张。能访问 docker socket 的人可以：

```bash
docker run --rm -v /:/host -it alpine chroot /host
```

于是 `/etc/shadow`、SSH 私钥、所有数据都在手里。所以：

- **口令必须是长随机串**，不要用你会复用到别处的东西；
- `allow_actions = false` 时即使口令泄露，攻击面也只剩「读指标」；
- 强烈建议配 `allow_cidrs`——它让白名单外的机器连 401 都拿不到。

### 三道闸的细节

实现在 `esc-panel-auth`（与隧道面板共用一份）：

1. **来源白名单** —— 只认 socket 来源，**不看 `X-Forwarded-For`**。那个头谁都能
   伪造，用它做访问控制等于没锁。放在反代后面时，白名单要写反代自己的地址。
2. **失败限速** —— 按来源计数，超预算 429 + `Retry-After`。**正确口令也会被挡**
   （否则限速形同虚设），60 秒后自动恢复。成功登录会清零。
3. **Basic 认证** —— 常量时间比较。

白名单解析失败会**拒绝启动**而不是「当没配」；后者会静默把管理面开给所有人，
而配置文件看起来还锁着。

### 明文 HTTP

面板是纯 HTTP，Basic 认证把 `user:pass` 做 base64 ——**base64 是编码不是加密**，
链路上任何一跳都能读。可信链路（家里宽带、公司内网）可以接受；公共 WiFi 不要用。
需要更强保障就在前面挂 TLS 终结（nginx/Caddy + 域名），面板退回只听回环。

### 沙箱

unit 里给的限制：

```
ProtectSystem=strict         /usr /boot /etc 只读
ProtectHome=read-only        看不到家目录内容
PrivateTmp=true              私有 /tmp
PrivateDevices=true          没有设备节点
NoNewPrivileges=true         不能提权
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
SystemCallFilter=@system-service
```

比 `esc-host` 宽松（要读 `/proc`、`/sys` 和 docker socket），但仍然划了线。

## 没有 Docker 的机器上

照样能用，只是 Docker 区块显示「未安装 Docker」，其余指标正常：

```json
{"docker": {"state": "unavailable", "socket": "/var/run/docker.sock",
            "message": "未检测到 Docker socket（这台机器可能没装 Docker）"}}
```

装好 Docker 后刷新页面即可，不用改配置也不用重启面板。

Docker 的三态是分开的，因为处置方式完全不同：

| state | 含义 | 怎么办 |
|---|---|---|
| `unavailable` | socket 不存在 | 没装 Docker，或 socket 在非默认路径 |
| `error` | socket 在但连不上 | daemon 没起来 / 用户不在 docker 组 / 权限不足 |
| `ok` | 正常 | — |

## 采样失败时的行为

`/proc` 解析跑在 `spawn_blocking` 里。如果它 panic（曾经发生过：命令行按**字节**
截断到 300，正好切在一个中文的中间），接口**不会**返回 500，而是返回一份空快照。

但空快照必须带标记，否则前端显示「CPU 0% / 内存 0%」会让人以为机器很空闲——
这是最坏的一种降级。所以响应里有 `sampling_error` 字段，前端据此显示红色横幅：

```json
{"sampling_error": "task 24 panicked with message \"end byte index 300 is not a char boundary\""}
```

日志里同时有一条 ERROR。回归测试见 `sys.rs::truncate_never_splits_a_multibyte_character`。

## 排障

```bash
systemctl status esc-server-panel
journalctl -u esc-server-panel -n 50

# 端口是否在听
ss -tlnp | grep 2224

# 自己能不能连
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:2224/healthz   # 401 = 正常（需要认证）

# Docker 权限
docker -H unix:///var/run/docker.sock ps        # 用 root 试，排除面板自身问题
sudo -u esc-monitor docker ps                   # 确认 esc-monitor 也能

# 手动对照 Docker API（与面板走同一条路）
curl --unix-socket /var/run/docker.sock http://d/version
curl --unix-socket /var/run/docker.sock http://d/containers/json?all=1 | head -c 400
```

| 现象 | 多半是 |
|---|---|
| 端口在听但外部连不上 | 云厂商安全组没放行 |
| 面板显示「Docker socket 存在但访问失败」 | `esc-monitor` 不在 docker 组，或 daemon 没起 |
| 所有指标都是 0 | 看页头有没有采样失败横幅，再看日志里的 ERROR |
| 容器动作返回 403 | 配置了 `allow_actions = false` |
| 容器动作返回 503 | Docker 不可用（未安装或 daemon 挂了） |
| 删除网络返回 403 | 预定义网络（bridge/host/none），或仍有容器接在上面 |
| 创建网络返回「already exists」 | 撞名。Docker 不会覆盖，换个名字 |
| 网络列表看不到自己的容器 | 已停止的容器不再有 endpoint，Docker 就不报它了 |

## 与隧道面板的关系

两者可以部署在同一台机器上（cloud2 就是这样：`2223` 隧道面板、`2224` 服务器面板），
也可以只装一个。

| | 隧道面板 `esc-plugin-tunnel-panel` | 服务器面板 `esc-server-panel` |
|---|---|---|
| 形态 | `esc-host` 的子进程插件 | 独立 systemd 服务 |
| 端口 | 2223 | 2224 |
| 管什么 | 端口映射（公网 → 内网） | 主机指标 + Docker 容器 |
| 权限 | 无特权（`User=esc`） | docker 组（能管容器） |
| 挂了会怎样 | 隧道照常转发 | 与隧道无关 |
| 共享 | 访问控制（`esc-panel-auth`）、前端风格 | 同左 |

## 部署时踩到的两个坑（2026-10-25 实操）

这两个都只有真部署才会遇到：本地测试是普通用户跑的，既没有 systemd 沙箱，
也不涉及多个服务用户的目录权限。

### 坑 A：`SupplementaryGroups=` 指向不存在的组 = 硬失败

没装 Docker 的机器上没有 `docker` 组，而 unit 里写着
`SupplementaryGroups=docker`。systemd 的反应是：

```
esc-server-panel.service: Failed to determine supplementary groups: No such process
esc-server-panel.service: Failed at step GROUP spawning ...: No such process
esc-server-panel.service: Main process exited, code=exited, status=216/GROUP
```

错误信息里**完全没有"组不存在"这个词**，`No such process` 看起来像进程问题。
而且它会被 `Restart=on-failure` 变成无限重启（很快撞上
`Start request repeated too quickly`）。

`SupplementaryGroups=` 不支持 `-` 前缀（不能"忽略失败"），所以用 drop-in 清空：

```ini
# /etc/systemd/system/esc-server-panel.service.d/no-docker.conf
[Service]
SupplementaryGroups=
```

装了 Docker 之后删掉这个文件即可。**注意清空后要 `systemctl reset-failed`**，
否则之前的失败状态和重启限流会挡住新的启动。

### 坑 B：目录权限 `drwxr-x---` 会让服务用户读不了自己的配置

`/etc/esc` 是 `drwxr-x--- root:esc`（隧道用户的地盘）。服务器面板以
`esc-monitor` 运行，**不在 `esc` 组里**，于是连目录都进不去：

```
Error: 读取配置文件 /etc/esc/server-panel.toml 失败
Caused by: Permission denied (os error 13)
```

配置文件本身是 `640 root:esc-monitor`（该用户可读），但目录没有 `x` 权限，
路径解析就失败了。

两种解法，权衡后选后者：

| 方案 | 问题 |
|---|---|
| 把 `esc-monitor` 加入 `esc` 组 | 它会连 `hub.key`（隧道私钥）一起读到 |
| 目录改 `751`（other 有 `x` 无 `r`） | 能按已知路径穿越，但列不出目录内容 |

```bash
chmod 751 /etc/esc
```

验证（用 `setpriv` 模拟目标用户，比 `sudo -u` 在受限环境里可靠）：

```bash
setpriv --reuid=esc-monitor --regid=esc-monitor --clear-groups \
        cat /etc/esc/server-panel.toml >/dev/null && echo 可读
setpriv --reuid=esc-monitor --regid=esc-monitor --clear-groups \
        ls /etc/esc 2>/dev/null || echo "不能列目录（预期的）"
setpriv --reuid=esc-monitor --regid=esc-monitor --clear-groups \
        cat /etc/esc/hub.key 2>/dev/null || echo "读不到私钥（预期的）"
setpriv --reuid=esc --regid=esc --clear-groups \
        cat /etc/esc/engine.toml >/dev/null && echo "隧道用户不受影响"
```

### cloud2 上 `chmod 751 /etc/esc` 记一笔

这是对既有配置目录权限的**放宽**（`other` 从 `---` 到 `--x`）。评估如下：

- 能穿越目录，但不能列出内容 —— 需要**知道确切文件名**才能访问；
- 各文件自己还有权限位：`hub.key` 是 `640 root:esc`，`esc-monitor` 读不到；
- 公钥证书（`ca.pem` / `hub.pem`）是 `644`，本来全世界可读，不敏感；
- 隧道用户 `esc` 属于 `esc` 组，权限不变（`drwxr-x--x` 中组仍是 `r-x`）。

如果将来不想保留这个放宽，替代方案是把服务器面板的配置放到独立目录
（如 `/etc/esc-monitor/`），并在 unit 里用 `--config` 指过去。

## 在 cloud2 上安装 Docker（2026-10-25 实操）

面板先上线、Docker 后装，正好验证了「没有 Docker 也能用、装完不用改配置」。

### 为什么用 Ubuntu 仓库的 `docker.io` 而不是官方 CE

```bash
apt-cache policy docker.io
#   Candidate: 29.1.3-0ubuntu4.1      ← Ubuntu 26.04 universe
curl -s -o /dev/null -w "%{http_code}" https://download.docker.com/linux/ubuntu/dists/
#   000                               ← 国内直连不通
```

官方 CE 需要先加 `download.docker.com` 的源，而这台机器拉不到。Ubuntu 仓库的
`docker.io` 是 **29.1.3**——与官方 CE 同步发布的打包版本，不缺功能，而且走
腾讯云内网镜像（`mirrors.tencentyun.com`）装起来很快。

### 坑 C：腾讯云内网镜像加速器在这台机器上不通

装完直接配了 `mirror.ccs.tencentyun.com`（腾讯云 CVM 的内网加速器）：

```
Registry Mirrors: http://mirror.ccs.tencentyun.com/
docker pull hello-world
→ dial tcp 169.254.0.51:80: i/o timeout
```

它解析到 `169.254.0.51`（链路本地地址），这台机器访问不到——**内网加速器只对
同 VPC 的 CVM 生效**，不是"在腾讯云上就一定可用"。

换成公共镜像（从 cloud2 上实测 `/v2/` 响应）：

| 镜像 | 响应 | 说明 |
|---|---|---|
| `docker.m.daocloud.io` | 401 | 活的（401 = 需要 token，正常） |
| `docker.1ms.run` | 401 | 活的 |
| `docker.xuanyuan.me` | 401 | 活的 |
| `docker.1panel.live` | 200 | 活的 |
| `registry-1.docker.io` | 超时 | 直连不通 |

配三个做冗余（docker 会依次尝试）：

```json
{
  "registry-mirrors": [
    "https://docker.m.daocloud.io",
    "https://docker.1ms.run",
    "https://docker.xuanyuan.me"
  ],
  "log-driver": "json-file",
  "log-opts": { "max-size": "20m", "max-file": "3" },
  "live-restore": true
}
```

### 那两个 daemon 配置不是可选项

- **`log-opts` 必须设**。docker 默认的 `json-file` 驱动**没有大小上限**，
  一个话多的容器（nginx access log、应用的 debug log）能几个月吃满磁盘。
  `20m × 3` = 单容器最多 60 MB，够排查问题又不至于失控。
- **`live-restore: true`** 让 `systemctl restart docker` 不杀掉运行中的容器。
  升级 docker 或改 daemon.json 时不至于把所有服务停一遍。

### 装完之后还要做三件事，否则面板还是「未安装」

```bash
# ① 面板用户加入 docker 组
/usr/sbin/usermod -aG docker esc-monitor

# ② 删掉没 Docker 时用的 drop-in（SupplementaryGroups= 需要 docker 组存在）
rm /etc/systemd/system/esc-server-panel.service.d/no-docker.conf

# ③ 重载并重启（缺失会踩坑 A 的 216/GROUP）
systemctl daemon-reload && systemctl reset-failed esc-server-panel
systemctl restart esc-server-panel

# 验证进程确实拿到了 docker 组（gid 通常是 107）
grep ^Groups /proc/$(systemctl show esc-server-panel -p MainPID --value)/status
#   Groups:	107 982
```

### 装完验证的结果

| 检查项 | 结果 |
|---|---|
| `docker --version` | 29.1.3 |
| 面板 `docker.state` | `ok`，API 1.52 |
| 拉镜像 | ✅ `hello-world`、`alpine` 都成功 |
| 列表 / 日志 / 资源 | ✅ 日志里的中文正常显示（复用流按帧解码，不会切多字节字符） |
| 面板 pause / unpause | ✅ 200，`docker ps` 显示 `(Paused)` |
| 面板 start | ✅ 200 |
| **面板 stop** | ⚠️ **第一次报 504，见下** |
| 磁盘 | `/var/lib/docker` 204 KB（两个基础镜像），换到 33% |

## Docker 管理能力（2026-10-25 补齐）

早期的面板只够「看」：启停、日志快照、资源采样。下面这些是日常运维真正会
用到、而且不该逼人回去敲 CLI 的部分。

### 创建容器

表单覆盖日常 90% 的字段：镜像、名称、命令（覆盖 CMD）、环境变量、端口映射、
目录挂载、网络、重启策略、是否自启。

```
镜像     nginx:alpine          ← 不存在会自动拉取（可能要等几十秒）
名称     web
端口     8080:80              ← 每行一条；也支持 127.0.0.1:8443:443 与 53:53/udp
环境     TZ=Asia/Shanghai
挂载     /srv/www:/usr/share/nginx/html:ro
重启策略 unless-stopped
```

创建出的容器会打上 `esc.server-panel=1` 标签——以后想分辨「哪些是从 Web 建的」,
`docker ps --filter label=esc.server-panel` 就够了。

**刻意不提供的字段**：`Privileged`、`CapAdd`、`PidMode`、`NetworkMode=host`、
`--device`。把这些摆到 Web 表单上，等于把「一条命令拿到宿主机 root」包装成
一个下拉框。需要那些能力就用 `docker run`——它至少有 shell history 留痕。

### 删除

强制停止后删除。前端有二次确认，并写明后果：容器内的临时数据会丢，
挂载的卷/目录不受影响。

### 详情

Docker 的 `/containers/{id}/json` 有上千个字段、层级很深。面板把它收敛成
固定形状（`ContainerDetail`），前端直接渲染——顺带这也就成了一层 API 契约：
Docker 改字段时只需要改一处。

展示：状态/退出码/启动时间、重启策略（**`no` 会标黄并提示「机器重启后不会
自启」**）、端口、网络与 IP、工作目录、Entrypoint/Cmd、内存与 CPU 限制、
是否特权、挂载、环境变量。

### 改重启策略

运维里最常见的一种补救：容器跑得好好的，机器一重启没起来——因为当初
`docker run` 没带 `--restart`。面板现在能直接改成 `unless-stopped`。

### 增量日志

`GET /api/docker/containers/{id}/logs?tail=300&since=<unix秒>`

Docker 的 `since` 有两个坑，都踩到了：

1. **只认 unix 秒**。文档说也接受 RFC3339，但实测 29.8.x 对非法格式**静默
   忽略**——「只要新日志」变成「返回全部」。前端增量刷新于是越滚越长、内容
   重复。所以后端强制数字格式，传错直接 400。
2. **粒度只到秒，且是「包含」语义**。同一秒内的行必然重复返回。前端因此按
   RFC3339 时间戳**逐行去重**（同精度下字典序等价于时间序）。

日志面板带「自动刷新」（2 秒）与「跟随底部」两个开关，日志量大时可以关掉
跟随，慢慢往上翻。

### 镜像管理

- **拉取**走后台任务：`POST /api/docker/images/pull` 立刻返回 `job` id，
  前端轮询 `/api/docker/jobs/{id}` 显示进度。
  同步等是有害的：大镜像要十分钟，浏览器/代理早就超时，使用者看到「失败」
  于是重试，两个拉取并行。给 job id 就不会有这种误解。
- **删除**镜像会提示后果（有容器在用时它们会变成悬空）。
- 镜像页会显示 daemon 的 `registry-mirrors`。**没配加速器 + 国内网络 = 基本
  拉不动**，而 docker 的报错只会说 `failed to do request ... i/o timeout`，
  完全指不到这里。

### 网络管理（2026-10-31 补齐）

面板的「网络」标签页把 Docker 网络从「创建容器时的一排名字」变成了可查看、
可管理的对象。

**列表**按三组排列，顺序就是运维关心的顺序：

| 分组 | 内容 | 能做 |
|---|---|---|
| 内部网络 | `--internal` 创建的，`internal: true` | 详情 / 接入 / 删除 |
| 自定义网络 | compose 建的 `*_default`、手建的普通网络 | 同上 |
| 系统网络 | `bridge` / `host` / `none` / ingress | 只能看 |

系统网络在**解析阶段**就被标出来（`NetworkInfo::system`），前端直接把按钮
置灰、后端也不会发那条注定 403 的请求。

**internal 网络到底是什么**（这是最容易被误解的一点）：Docker 不给它配
NAT 与默认路由，于是

- 容器**没有外网出口**；
- `-p 8080:80` 这种端口映射**不生效**（宿主根本没有到它的转发路径）；
- 容器只能与同一网络内的成员通信 —— 这正是「隔离一组服务」想要的效果。

实测对照（同一镜像，只看 `/proc/net/route`）：

```
--network <internal>   dst=00FC1FAC gw=00000000      ← 只有直连路由，无默认网关
--network bridge       dst=00000000 gw=010011AC      ← 有默认网关
```

**创建**只支持 `bridge` 驱动，参数是名字 / 是否 internal / 子网 / 网关 / IPv6：

```json
{ "name": "isolated-net", "internal": true, "subnet": "172.30.0.0/24", "gateway": "172.30.0.1" }
```

校验全在本机做完，不靠 Docker 报错：

| 规则 | 原因 |
|---|---|
| 名字 1–63 字符，字母数字开头，只允许 `_ . -` | Docker 的限制；提前拒能说清是哪一条 |
| 子网前缀必须 `/24`–`/29` | 太大一次吃掉整个地址池，太小装不下几个容器 |
| 子网必须是网络地址（主机位为 0） | 报错时直接给出正确写法：`172.30.0.7/24` → 「应写成 172.30.0.0/24」 |
| 网关必须落在子网内，且不是网络/广播地址 | 这两个地址不能分配给接口 |
| 只填网关不填子网 | 说不清意图，拒 |

子网**留空**是最常用的路径：不带 `IPAM` 字段，让 Docker 自己挑一个不与现有
网络冲突的网段（实测在建了 8 个网络的机器上会自动挑 172.23.0.0/16）。

**接入 / 断开**容器走 `allow_actions` 而不是 `allow_create` —— 它不新增东西，
改的是既有的网络归属，与「启停容器」同一档风险。

- 接入立即生效，**容器不重启**，网络栈会短暂中断；
- 断开时 `force` 默认 `false`：Docker 默认拒绝摘掉运行中容器的最后一个网络
  （摘掉之后它彻底没网），所以面板把它做成详情页里一个显式的「强制断开」开关，
  而不是藏在二次确认里。

**删除**是不可撤销的，所以 UI 上要**手打网络名**才解锁「永久删除」按钮
（比删容器的一步 `confirm()` 更重）。Docker 还有两道天然保护：
预定义网络拒绝删除（403 `is a pre-defined network`），有成员时拒绝删除
（403 `has active endpoints`）。

### 网络列表为什么同时返回 `networks` 和 `names`

`GET /api/docker/networks` 原来返回 `{"networks": ["bridge", ...]}`，扩成完整
对象数组会打断所有按名字取值的调用方 —— 首当其冲的是「创建容器」弹窗里的
网络下拉。所以响应里同时给出：

```json
{ "networks": [{"name": "bridge", "driver": "...", "internal": false, ...}], "names": ["bridge", ...] }
```

前端下拉继续用 `names`，网络页用 `networks`。这是**故意保留的冗余**，不是
忘了删：留着它，外部脚本和未来新增的调用点都不会因为这次扩展而碎掉。

### 整栈启停

容器列表按 Compose 项目分组，组头带「全部启动 / 全部停止」。批量操作是
**串行**的，某个失败也继续往下（一个坏容器不该拖住整栈）。结束后汇总
「N 个成功 / M 个失败（首个错误原因）」。

### 权限分级

三个独立开关，都在 `[panel]` 同级（配置文件顶层）：

| 开关 | 控制 |
|---|---|
| `allow_actions` | 启停 / 重启 / 暂停 / 改名 / 改重启策略 |
| `allow_create` | 创建容器、拉镜像 |
| `allow_delete` | 删除容器 / 镜像 |

全关 = 纯只读监控（页面上连按钮都不出现，顶部显示「只读」）。常见组合：

```toml
# 值班同学：只能重启，不能建也不能删
allow_actions = true
allow_create  = false
allow_delete  = false
```

前端会按开关隐藏按钮，后端每个写端点也各自校验（返回 403 并说明是哪个开关
关着）——**不能只靠前端隐藏**。

## 全部 API（Docker 部分）

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/docker/containers` | 列表（含已停止） |
| POST | `/api/docker/containers` | 创建（body 见下） |
| GET | `/api/docker/containers/{id}` | Docker 原始 inspect JSON |
| GET | `/api/docker/containers/{id}/detail` | 结构化详情 |
| DELETE | `/api/docker/containers/{id}` | 删除（force） |
| GET | `/api/docker/containers/{id}/logs?tail=&since=` | 日志（增量） |
| GET | `/api/docker/containers/{id}/stats` | 单次资源采样 |
| POST | `/api/docker/containers/{id}/{action}` | start/stop/restart/pause/unpause |
| POST | `/api/docker/containers/{id}/rename` | `{"name": "..."}` |
| POST | `/api/docker/containers/{id}/restart-policy` | `{"policy": "..."}` |
| GET | `/api/docker/images` | 镜像列表 |
| POST | `/api/docker/images/pull` | `{"image": "..."}` → `{"job": "..."}` |
| DELETE | `/api/docker/images/{name}` | 删除镜像 |
| GET | `/api/docker/networks` | 网络列表（含 `names` 兜底） |
| GET | `/api/docker/networks/{id}` | 网络 + 成员容器 |
| POST | `/api/docker/networks` | 创建网络 |
| DELETE | `/api/docker/networks/{id}` | 删除网络 |
| POST | `/api/docker/networks/{id}/connect` | 接入容器 |
| POST | `/api/docker/networks/{id}/disconnect` | 断开容器 |
| GET | `/api/docker/jobs` · `/api/docker/jobs/{id}` | 后台任务状态 |

创建容器的 body（全部字段可选，除 `image`）：

```json
{
  "image": "nginx:alpine",
  "name": "web",
  "cmd": ["nginx", "-g", "daemon off;"],
  "env": ["TZ=Asia/Shanghai"],
  "ports": ["8080:80", "53:53/udp"],
  "binds": ["/srv/www:/usr/share/nginx/html:ro"],
  "restart": "unless-stopped",
  "network": "bridge",
  "auto_start": true
}
```

`restart` 取值：`no` / `always` / `unless-stopped` / `on-failure[:N]`。
写错会 400 并列出允许值——`on-failure:x` 也曾被静默当成「不限次数」接受，
现在会报「重试次数 'x' 不是数字」。

## 服务器核心插件树标签页

服务器面板仍然**不是** `esc-host` 的插件。「插件树」通过一个窄的只读 HTTP
客户端复用隧道面板的 `GET /api/plugins`，不会获得引擎或宿主的内部句柄。
在 `server-panel.toml` 中配置：

```toml
plugin_tree_url = "http://127.0.0.1:8787/api/plugins"
# 隧道面板配置了 auth 时才需要：
plugin_tree_auth = "admin:隧道面板的口令"
```

`plugin_tree_url` 应指向**隧道面板自身**的监听地址，而不是网关的外部端口。
本机回环可以用 HTTP；非回环地址强制 HTTPS。请求有 5 秒超时、不跟随重定向，
凭据只允许放在 `plugin_tree_auth`，不能嵌进 URL。

不配置时其它功能不受影响，插件树页返回 503 并显示配置方法，不会把「未配置」
误报成「0 个插件」。页面中的父子缩进和服务依赖都来自 `plugin-framework`
的真实快照，具体语义见 `docs/PANEL.md`。

## ui-frame 组件化前端

本面板与隧道面板共用 `panel-ui/` 的 Vue 3 工程及 ui-frame 主题。容器、镜像和
插件树均使用真实组件渲染，静态 JS/CSS 由本进程提供且嵌入二进制。源码与重建
步骤见 `panel-ui/README.md`。
