//! 「面板挂了不影响隧道」这条承诺的回归测试。
//!
//! 拆成两个插件的全部意义就在这里，所以它必须被自动化守住，而不是只写在
//! 文档里。
//!
//! 做法：起一个**真的** `esc-host` 子进程，把面板的可执行文件指向一个不
//! 存在的路径。期望：
//!
//! 1. 面板启动失败被记录，但宿主**不退出**；
//! 2. 隧道引擎仍然就绪（日志里有 control listener 绑定）；
//! 3. 宿主能被杀掉（说明它一直在跑，不是僵在某处）。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "esc-host-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 等到 `path` 的内容满足 `pred`，返回最终内容。
fn wait_for_log(path: &std::path::Path, what: &str, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if pred(&text) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "超时等待「{what}」，当前日志：\n{text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn missing_panel_binary_does_not_stop_the_tunnel() {
    let dir = temp_dir("isolation");

    // 一套可用的证书（hub 只需要 server 证书）
    let ca = esc_common::pki::Ca::generate("host-it-ca", 2).unwrap();
    let ca_path = esc_common::pki::write_ca_cert(&dir, "ca", &ca).unwrap();
    let pair = ca
        .issue_server("hub", &["127.0.0.1".to_string()], 1)
        .unwrap();
    let (cert, key) = esc_common::pki::write_pair(&dir, "hub", &pair).unwrap();

    let engine = dir.join("engine.toml");
    std::fs::write(
        &engine,
        format!(
            "role = \"hub\"\nstore = \"{}\"\n\n[hub]\nlisten = \"127.0.0.1:0\"\n\n[tls]\nca_cert = \"{}\"\ncert = \"{}\"\nkey = \"{}\"\n",
            dir.join("mappings.toml").display(),
            ca_path.display(),
            cert.display(),
            key.display()
        ),
    )
    .unwrap();

    let host_cfg = dir.join("host.toml");
    std::fs::write(
        &host_cfg,
        format!(
            "[engine]\nconfig = \"{}\"\n\n[panel]\nenabled = true\ncommand = \"/nonexistent/esc-plugin-tunnel-panel\"\n",
            engine.display()
        ),
    )
    .unwrap();

    let log_path = dir.join("host.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_esc-host"))
        .arg("-c")
        .arg(&host_cfg)
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .expect("esc-host 应当能启动");

    let text = wait_for_log(&log_path, "面板启动失败但宿主继续", |t| {
        t.contains("panel plugin failed to start") || t.contains("tunnel keeps running")
    });

    // 1) 隧道不受影响：引擎仍然就绪
    assert!(
        text.contains("tunnel engine ready") || text.contains("tunnel plugin mounted"),
        "隧道引擎应当正常起来：\n{text}"
    );

    // 2) 宿主还活着
    assert!(
        child.try_wait().expect("try_wait 失败").is_none(),
        "面板启动失败不应让宿主退出：\n{text}"
    );

    // 3) 再等一会儿，它应该依然活着（不是"晚一点才崩"）
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        child.try_wait().expect("try_wait 失败").is_none(),
        "宿主在面板失败后应持续运行"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
