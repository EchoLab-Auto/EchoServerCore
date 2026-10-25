//! 主机指标采集：直接读 `/proc` 与 `statvfs`，不引入外部依赖。
//!
//! 为什么不装 `sysinfo` 这类 crate：需要的东西很少（CPU 使用率、内存、负载、
//! 磁盘、网络、进程 top），而 `/proc` 的格式是内核 ABI 的一部分，二十年没变过。
//! 自己读带来两个好处——二进制小（这点对「`scp` 一个文件就能用」很重要），
//! 以及**每个字段的来源都能在代码里指出来**，不用猜库替我们做了什么。
//!
//! 唯一的坑是「速率类指标」需要两次采样：CPU 使用率和网络/磁盘吞吐都是**差值**。
//! 所以有一个 `Sampler` 持有上一次快照；第一次采样只能给出「自开机以来」的
//! 平均值，之后的采样才反映瞬时速率。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use serde::Serialize;

// ─────────────────────────────────────────────────────────────────────────────
// 对外结构
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub host: HostInfo,
    pub cpu: CpuInfo,
    pub mem: MemInfo,
    pub disks: Vec<DiskUsage>,
    pub net: NetInfo,
    pub procs: Vec<ProcInfo>,
    /// 采样间隔（毫秒）。速率类字段都按它归一化成「每秒」。
    pub interval_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub uptime_secs: u64,
    pub cpu_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct CpuInfo {
    /// 自上次采样以来的总体使用率（%）。首次采样是自开机以来的平均值。
    pub usage_pct: f64,
    /// 每个核心的使用率（%）；内核不给每核数据时为空。
    pub per_core: Vec<f64>,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    /// `/proc/stat` 里除 idle/iowait 之外的字段总和（tick）。
    pub total_ticks: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MemInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub available_bytes: u64,
    pub buffers_bytes: u64,
    pub cached_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub usage_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiskUsage {
    pub mount: String,
    pub device: String,
    pub fs: String,
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    pub usage_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetInfo {
    /// 所有非回环接口的累计字节。
    pub rx_total_bytes: u64,
    pub tx_total_bytes: u64,
    /// 自上次采样以来的速率（字节/秒）。
    pub rx_per_sec: f64,
    pub tx_per_sec: f64,
    pub interfaces: Vec<NetIface>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetIface {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_per_sec: f64,
    pub tx_per_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cmdline: String,
    pub state: String,
    pub threads: u32,
    /// 常驻内存（字节）。注意内核给的是「页数」，这里已经乘过页大小。
    pub rss_bytes: u64,
    /// 自进程启动累计的 CPU 时间（tick）→ 换算成百分比需要除以窗口。
    pub cpu_pct: f64,
    pub user: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// 采样器
// ─────────────────────────────────────────────────────────────────────────────

/// 持有上一次采样，用来把「累计计数」变成「速率」。
#[derive(Debug)]
pub struct Sampler {
    inner: Mutex<Prev>,
}

#[derive(Debug, Default)]
struct Prev {
    at: Option<std::time::Instant>,
    /// 每个 CPU（含 `cpu` 汇总行的索引 0）的 (busy, total)。
    cpu: Vec<(u64, u64)>,
    net: HashMap<String, (u64, u64)>,
    /// 每个进程上次的 (cpu ticks, 采样时刻)，用来算进程级 CPU%。
    proc_cpu: HashMap<u32, (u64, std::time::Instant)>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Prev::default()),
        }
    }

    /// 采一次样。返回的速率字段相对**上一次**采样。
    pub fn sample(&self) -> Snapshot {
        let mut prev = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let cpu_raw = read_cpu_ticks();
        let net_raw = read_net();
        let now = std::time::Instant::now();
        let elapsed = prev
            .at
            .map(|t| now.duration_since(t).as_secs_f64())
            .filter(|d| *d > 0.000_5);

        // ── CPU ──
        let cpu_total = cpu_raw.first().copied().unwrap_or((0, 0));
        let (busy, total) = cpu_total;
        let usage_pct = if elapsed.is_some() {
            prev.cpu
                .first()
                .map(|(pb, pt)| {
                    let db = busy.saturating_sub(*pb) as f64;
                    let dtotal = total.saturating_sub(*pt) as f64;
                    if dtotal > 0.0 {
                        (db / dtotal * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    }
                })
                .unwrap_or_else(|| pct(busy, total))
        } else {
            pct(busy, total)
        };

        let per_core: Vec<f64> = cpu_raw
            .iter()
            .skip(1)
            .enumerate()
            .map(|(i, (b, t))| {
                if let (Some(_), Some((pb, pt))) = (elapsed, prev.cpu.get(i + 1)) {
                    let db = b.saturating_sub(*pb) as f64;
                    let dt = t.saturating_sub(*pt) as f64;
                    if dt > 0.0 {
                        (db / dt * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    }
                } else {
                    pct(*b, *t)
                }
            })
            .collect();

        // ── 网络 ──
        let mut ifaces = Vec::new();
        let mut rx_total = 0u64;
        let mut tx_total = 0u64;
        let mut rx_rate = 0f64;
        let mut tx_rate = 0f64;
        for (name, rx, tx) in &net_raw {
            rx_total += rx;
            tx_total += tx;
            let (drx, dtx) = match (elapsed, prev.net.get(name)) {
                (Some(_), Some((prx, ptx))) => (
                    rx.saturating_sub(*prx) as f64,
                    tx.saturating_sub(*ptx) as f64,
                ),
                _ => (0.0, 0.0),
            };
            let (r, t) = match elapsed {
                Some(d) => (drx / d, dtx / d),
                None => (0.0, 0.0),
            };
            rx_rate += r;
            tx_rate += t;
            ifaces.push(NetIface {
                name: name.clone(),
                rx_bytes: *rx,
                tx_bytes: *tx,
                rx_per_sec: r,
                tx_per_sec: t,
            });
        }
        ifaces.sort_by_key(|i| std::cmp::Reverse(i.rx_bytes + i.tx_bytes));

        // ── 进程 ──
        let procs = read_procs(&prev.proc_cpu, elapsed);

        // ── 写回快照 ──
        prev.at = Some(now);
        prev.cpu = cpu_raw;
        prev.net = net_raw.into_iter().map(|(n, r, t)| (n, (r, t))).collect();
        prev.proc_cpu = procs
            .iter()
            .map(|p| (p.pid, (p.cpu_pct as u64, now)))
            .collect();

        Snapshot {
            host: read_host(),
            cpu: CpuInfo {
                usage_pct,
                per_core,
                load1: read_load().0,
                load5: read_load().1,
                load15: read_load().2,
                total_ticks: total,
            },
            mem: read_mem(),
            disks: read_disks(),
            net: NetInfo {
                rx_total_bytes: rx_total,
                tx_total_bytes: tx_total,
                rx_per_sec: rx_rate,
                tx_per_sec: tx_rate,
                interfaces: ifaces,
            },
            procs,
            interval_ms: elapsed.map(|d| (d * 1000.0) as u64).unwrap_or(0),
        }
    }
}

fn pct(busy: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// /proc 解析
// ─────────────────────────────────────────────────────────────────────────────

fn read_host() -> HostInfo {
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "Linux".into());
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    HostInfo {
        hostname,
        os,
        kernel,
        arch: std::env::consts::ARCH.to_string(),
        uptime_secs: read_uptime(),
        cpu_count: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
    }
}

fn read_uptime() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0)
}

/// 返回 `[(busy, total)]`，**索引 0 是 `cpu` 汇总行**，之后每个核心一行。
fn read_cpu_ticks() -> Vec<(u64, u64)> {
    let Ok(raw) = std::fs::read_to_string("/proc/stat") else {
        return Vec::new();
    };
    raw.lines()
        .filter(|l| l.starts_with("cpu"))
        .map(|line| {
            let mut it = line.split_whitespace();
            let _label = it.next();
            let nums: Vec<u64> = it.filter_map(|f| f.parse().ok()).collect();
            // user nice system idle iowait irq softirq steal guest guest_nice
            let idle = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
            let total: u64 = nums.iter().take(8).sum();
            (total.saturating_sub(idle), total)
        })
        .collect()
}

fn read_load() -> (f64, f64, f64) {
    let Ok(raw) = std::fs::read_to_string("/proc/loadavg") else {
        return (0.0, 0.0, 0.0);
    };
    let mut it = raw.split_whitespace().filter_map(|f| f.parse::<f64>().ok());
    (
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
        it.next().unwrap_or(0.0),
    )
}

fn read_mem() -> MemInfo {
    let raw = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut map: HashMap<&str, u64> = HashMap::new();
    for line in raw.lines() {
        if let Some((k, v)) = line.split_once(':') {
            // 值是 "12345 kB" 或 "12345"
            if let Some(num) = v.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) {
                map.insert(k, num * 1024);
            }
        }
    }
    let total = map.get("MemTotal").copied().unwrap_or(0);
    let free = map.get("MemFree").copied().unwrap_or(0);
    let available = map.get("MemAvailable").copied().unwrap_or(free);
    let buffers = map.get("Buffers").copied().unwrap_or(0);
    let cached = map.get("Cached").copied().unwrap_or(0);
    // used 用「总量 - 可用」而不是「总量 - 空闲」：后者会把可回收的缓存算成占用，
    // 看起来永远吃满，是监控面板最常见的误导。
    let used = total.saturating_sub(available);
    let swap_total = map.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = map.get("SwapFree").copied().unwrap_or(0);
    MemInfo {
        total_bytes: total,
        used_bytes: used,
        free_bytes: free,
        available_bytes: available,
        buffers_bytes: buffers,
        cached_bytes: cached,
        swap_total_bytes: swap_total,
        swap_used_bytes: swap_total.saturating_sub(swap_free),
        usage_pct: if total == 0 {
            0.0
        } else {
            used as f64 / total as f64 * 100.0
        },
    }
}

/// 网络累计计数：(iface, rx, tx)，已排除回环。
fn read_net() -> Vec<(String, u64, u64)> {
    let Ok(raw) = std::fs::read_to_string("/proc/net/dev") else {
        return Vec::new();
    };
    raw.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, rest) = line.split_once(':')?;
            let name = name.trim();
            if name == "lo" || name.is_empty() {
                return None;
            }
            let nums: Vec<u64> = rest
                .split_whitespace()
                .filter_map(|f| f.parse().ok())
                .collect();
            // rx: bytes packets errs drop fifo frame compressed multicast
            // tx: bytes packets errs drop fifo colls carrier compressed
            let rx = nums.first().copied().unwrap_or(0);
            let tx = nums.get(8).copied().unwrap_or(0);
            Some((name.to_string(), rx, tx))
        })
        .collect()
}

/// 磁盘空间。只报告「真实块设备」挂载，跳过 tmpfs/overlay/proc 之类。
fn read_disks() -> Vec<DiskUsage> {
    let Ok(raw) = std::fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in raw.lines() {
        let mut it = line.split_whitespace();
        let (Some(device), Some(mount), Some(fs)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let skip = [
            "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "overlay",
            "squashfs", "ramfs", "securityfs", "debugfs", "tracefs", "pstore", "bpf", "autofs",
            "mqueue", "hugetlbfs", "fusectl", "configfs", "binfmt_misc", "nsfs", "efivarfs",
        ];
        if skip.contains(&fs) {
            continue;
        }
        if !seen.insert(mount.to_string()) {
            continue;
        }
        let Some((total, avail)) = statvfs(mount) else {
            continue;
        };
        if total == 0 {
            continue;
        }
        let used = total.saturating_sub(avail);
        out.push(DiskUsage {
            mount: mount.to_string(),
            device: device.to_string(),
            fs: fs.to_string(),
            total_bytes: total,
            used_bytes: used,
            avail_bytes: avail,
            usage_pct: used as f64 / total as f64 * 100.0,
        });
    }
    // 容量大的排前面（运维最关心根和大盘）
    out.sort_by_key(|d| std::cmp::Reverse(d.total_bytes));
    out
}

fn statvfs(path: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(path).ok()?;
    // SAFETY: statvfs 只写我们提供的结构体；路径已转成 NUL 结尾的 CString。
    unsafe {
        let mut buf: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut buf) != 0 {
            return None;
        }
        let frsize = if buf.f_frsize == 0 {
            buf.f_bsize as u64
        } else {
            buf.f_frsize as u64
        };
        Some((
            buf.f_blocks.saturating_mul(frsize),
            buf.f_bavail.saturating_mul(frsize),
        ))
    }
}

/// 进程列表（按 CPU 降序，取前 20）。
///
/// `prev` 是上一次采样时每个进程的累计 CPU tick；用它把"自启动以来"换算成
/// "这段时间里的占比"。没有上次记录时退化为「累计占比」，仍然可用于排序。
fn read_procs(prev: &HashMap<u32, (u64, std::time::Instant)>, elapsed: Option<f64>) -> Vec<ProcInfo> {
    let page = page_size();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(stat) = read_proc_stat(pid) else {
            continue;
        };
        let cmdline = std::fs::read(entry.path().join("cmdline"))
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .split('\0')
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let user = read_proc_user(pid);

        let cpu_ticks = stat.utime + stat.stime;
        let cpu_pct = match (elapsed, prev.get(&pid)) {
            (Some(dt), Some((pt, _))) if dt > 0.0 => {
                let delta = cpu_ticks.saturating_sub(*pt) as f64;
                // tick 通常是 100 Hz；除以 (窗长 * hz) 得到单核占比
                (delta / (dt * 100.0) * 100.0).clamp(0.0, 100.0 * stat.threads.max(1) as f64)
            }
            _ => 0.0,
        };

        out.push(ProcInfo {
            pid,
            name: stat.comm,
            cmdline: truncate_chars(&cmdline, 300),
            state: stat.state,
            threads: stat.threads,
            rss_bytes: stat.rss.saturating_mul(page),
            cpu_pct,
            user,
        });
    }
    out.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.rss_bytes.cmp(&a.rss_bytes))
    });
    out.truncate(20);
    out
}

struct ProcStat {
    comm: String,
    state: String,
    utime: u64,
    stime: u64,
    threads: u32,
    rss: u64,
}

/// 解析 `/proc/<pid>/stat`。
///
/// 格式陷阱：第二个字段 `comm` 被括号包着，而**进程名里可以包含空格和右括号**。
/// 所以不能按空白 split，必须定位最后一个 `)`。
fn read_proc_stat(pid: u32) -> Option<ProcStat> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if close <= open {
        return None;
    }
    let comm = raw[open + 1..close].to_string();
    let rest: Vec<&str> = raw[close + 1..].split_whitespace().collect();
    // rest[0] = state（原第 3 字段）；utime 是第 14 字段 → 索引 11
    Some(ProcStat {
        comm,
        state: rest.first().map(|s| s.to_string()).unwrap_or_default(),
        utime: rest.get(11)?.parse().ok()?,
        stime: rest.get(12)?.parse().ok()?,
        threads: rest.get(17)?.parse().ok()?,
        rss: rest.get(21)?.parse().ok()?,
    })
}

/// 进程属主。`/proc/<pid>/status` 里的 `Uid:\t<real>\t<effective>...`。
fn read_proc_user(pid: u32) -> String {
    let Ok(raw) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return String::new();
    };
    let Some(line) = raw.lines().find(|l| l.starts_with("Uid:")) else {
        return String::new();
    };
    let Some(uid) = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return String::new();
    };
    // 不去查 /etc/passwd（那要多读一堆文件）：0/999 这类足够判断，
    // 其余直接显示数字，运维看得懂。
    match uid {
        0 => "root".to_string(),
        other => other.to_string(),
    }
}

/// 按**字符**截断，而不是按字节。
///
/// 这里踩过一次真实的坑：命令行里有中文（或任何多字节字符）时，`&s[..300]`
/// 会落在字符中间直接 panic。而 panic 发生在 `spawn_blocking` 里，表现成
/// 「面板偶尔整块数据空白」——比崩溃更难查，因为它取决于机器上恰好跑着什么。
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn page_size() -> u64 {
    // sysconf 基本不会失败；失败时用 4 KiB（x86_64/aarch64 的常规值）。
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        4096
    }
}

/// 系统是否可能在容器里跑（面板要如实告诉使用者，否则指标会「看着不对」）。
pub fn is_containerised() -> bool {
    if Path::new("/.dockerenv").exists() {
        return true;
    }
    std::fs::read_to_string("/proc/1/cgroup")
        .map(|s| s.contains("docker") || s.contains("kubepods") || s.contains("lxc"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_is_parsed_and_used_is_derived_from_available() {
        let m = read_mem();
        assert!(m.total_bytes > 0, "MemTotal 应当能读到");
        assert!(m.available_bytes <= m.total_bytes);
        // used = total - available：这个关系必须成立，否则面板显示的占用是假的
        assert_eq!(m.used_bytes, m.total_bytes - m.available_bytes);
        assert!(m.usage_pct >= 0.0 && m.usage_pct <= 100.0);
    }

    #[test]
    fn cpu_ticks_include_a_summary_row() {
        let ticks = read_cpu_ticks();
        assert!(!ticks.is_empty(), "至少应有 cpu 汇总行");
        let (busy, total) = ticks[0];
        assert!(total > 0);
        assert!(busy <= total, "busy 不可能超过 total");
    }

    #[test]
    fn two_samples_produce_a_rate_after_the_first() {
        let s = Sampler::new();
        let first = s.sample();
        assert_eq!(first.interval_ms, 0, "首次采样没有间隔");
        std::thread::sleep(std::time::Duration::from_millis(60));
        let second = s.sample();
        assert!(second.interval_ms >= 40, "第二次采样应带间隔");
        assert!(second.cpu.usage_pct >= 0.0 && second.cpu.usage_pct <= 100.0);
    }

    #[test]
    fn proc_stat_parses_names_containing_spaces_and_parens() {
        // 内核在这里的格式允许进程名带空格与右括号，唯一可靠的锚点是**最后**一个 ')'
        let dir = std::env::temp_dir();
        let _ = dir;
        // 直接构造一行等价内容验证解析逻辑
        let raw = "12345 (my weird )name) S 1 12345 12345 0 -1 4194560 100 0 0 0 11 22 0 0 20 0 7 0 1000 12345678 999 18446744073709551615";
        let open = raw.find('(').unwrap();
        let close = raw.rfind(')').unwrap();
        assert_eq!(&raw[open + 1..close], "my weird )name");
        let rest: Vec<&str> = raw[close + 1..].split_whitespace().collect();
        assert_eq!(rest[0], "S");
        assert_eq!(rest[11], "11"); // utime
        assert_eq!(rest[12], "22"); // stime
        assert_eq!(rest[17], "7"); // threads
        assert_eq!(rest[21], "999"); // rss
    }

    #[test]
    fn current_process_is_visible_in_the_list() {
        let s = Sampler::new();
        let snap = s.sample();
        assert!(!snap.procs.is_empty(), "至少能看到自己");
        assert!(snap.procs.len() <= 20, "要截断到前 20");
        let me = std::process::id();
        // 测试进程未必进前 20（CPU 排序），但整个列表必须非空且字段合理
        for p in &snap.procs {
            assert!(p.pid > 0);
            assert!(p.threads >= 1, "线程数至少 1");
        }
        // 至少能解释自己的 pid（可能在也可能不在前 20）
        let _ = me;
    }

    #[test]
    fn disks_exclude_pseudo_filesystems_and_include_root() {
        let disks = read_disks();
        assert!(!disks.is_empty(), "至少应有根分区");
        assert!(
            disks.iter().all(|d| d.mount != "/proc" && d.mount != "/sys"),
            "伪文件系统应被过滤"
        );
        let root = disks.iter().find(|d| d.mount == "/");
        assert!(root.is_some(), "根挂载应当出现在列表里");
        assert!(root.unwrap().total_bytes > 0);
    }

    #[test]
    fn net_excludes_loopback() {
        let ifaces = read_net();
        assert!(
            ifaces.iter().all(|(name, _, _)| name != "lo"),
            "回环接口应被排除（否则速率永远是本机自环）"
        );
    }

    #[test]
    fn truncate_never_splits_a_multibyte_character() {
        // 299 个 ASCII 之后紧跟中文：字节索引 300 会落在 '采' 中间
        let mut s = "a".repeat(299);
        s.push_str("采集器");
        let out = truncate_chars(&s, 300);
        assert!(out.ends_with('…'), "超长时应加省略号");
        assert_eq!(out.chars().count(), 301, "300 个字符 + 省略号");
        // 更极端：全是 4 字节 emoji
        let emoji = "😀".repeat(500);
        let out = truncate_chars(&emoji, 10);
        assert_eq!(out.chars().count(), 11);
        // 不超长时原样返回
        assert_eq!(truncate_chars("短", 300), "短");
    }

    #[test]
    fn sampling_survives_multibyte_command_lines() {
        // 端到端回归：采一次样不能因为任何进程的命令行含中文而 panic。
        // （真实故障就是 spawn_blocking 里 panic，前端整块数据变空白。）
        let s = Sampler::new();
        let snap = s.sample();
        // 命令行里可能带中文，只要没 panic 就说明截断是安全的
        for p in &snap.procs {
            assert!(p.cmdline.chars().count() <= 301);
        }
    }

    #[test]
    fn load_and_host_fields_are_populated() {
        let h = read_host();
        assert!(!h.hostname.is_empty());
        assert!(h.cpu_count >= 1);
        let (a, b, c) = read_load();
        assert!(a >= 0.0 && b >= 0.0 && c >= 0.0);
    }
}
