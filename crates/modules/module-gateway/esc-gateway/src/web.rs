//! 登录页、错误页，以及管理 API 的响应结构。
//!
//! # 这里为什么还有 HTML
//!
//! 只剩两个页面：**登录页**和**错误页**。它们必须是 HTML——登录是「302 到登录页、
//! 填完表单再回到你原来要去的地方」这条流程的一部分，前端还没有机会跑起来。
//!
//! # 管理页为什么不在这个文件里了
//!
//! 管理页（账号 / 端口 / 会话）现在是 Vue 应用的一个标签页，和隧道面板、服务器
//! 面板同一套 UI。网关这台机器上留下的是 `admin.rs` 里的 **JSON API**，结构定义
//! （`UsersView` / `RoutesView` / …）还留在本文件，`#[derive(Serialize)]` 直接
//! 序列化成前端要的形状。
//!
//! 删掉的那一千多行 HTML 渲染不是"重构着顺手删"——它们的每一个分支都被 JSON
//! 结构体一一对应地承接了，所以删完之后 `cargo test` 里那些断言字段的用例仍然
//! 有效，只是从"页面里找字符串"变成"结构里找字段"。
//!
//! 留下的这两个页面仍然**单文件内联、零外部资源**：如果登录页依赖 CDN 上的字体
//! 或脚本，那么在网络受限的机器上就白屏——而那正是你需要登录进去改配置的时候。

use serde::Serialize;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const STYLE: &str = r#"
:root{--bg:#0e1116;--bg2:#161b22;--bg3:#1c2230;--line:#252d3a;
  --fg:#d7dee8;--fg2:#8b98a9;--fg3:#5b6675;--err:#f85149;--info:#58a6ff;
  --mono:ui-monospace,SFMono-Regular,"SF Mono",Menlo,Consolas,monospace}
*{box-sizing:border-box}
html,body{margin:0;padding:0;background:var(--bg);color:var(--fg);min-height:100%;
  font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif}
body{display:flex;align-items:center;justify-content:center;padding:24px}
.card{width:100%;max-width:380px;background:var(--bg2);border:1px solid var(--line);
  border-radius:12px;padding:26px 26px 22px;box-shadow:0 18px 50px #000000aa}
h1{font-size:17px;margin:0 0 4px;font-weight:600;letter-spacing:.2px}
.sub{font-size:12.5px;color:var(--fg2);margin:0 0 20px}
.sub code{font-family:var(--mono);background:var(--bg3);padding:1px 6px;border-radius:4px;
  border:1px solid var(--line)}
label{display:block;font-size:12px;color:var(--fg2);margin:0 0 5px}
input{width:100%;font:inherit;background:#0a0d12;color:var(--fg);
  border:1px solid var(--line);border-radius:7px;padding:9px 11px;margin-bottom:14px}
input:focus{outline:none;border-color:#3d6fd4;box-shadow:0 0 0 3px #3d6fd422}
button{width:100%;font:inherit;font-weight:500;cursor:pointer;color:#fff;
  background:#1f6feb;border:1px solid #1f6feb;border-radius:7px;padding:9px 12px;transition:.15s}
button:hover{background:#2a7cf5}
.err{background:#f8514914;border:1px solid #f8514944;color:#ff9d97;border-radius:7px;
  padding:9px 12px;font-size:12.5px;margin:0 0 16px}
.foot{margin-top:16px;font-size:11.5px;color:var(--fg3);text-align:center}
.detail{font-size:13px;color:var(--fg2);line-height:1.65}
.detail code{font-family:var(--mono);background:var(--bg3);padding:1px 6px;border-radius:4px;
  border:1px solid var(--line)}
a{color:var(--info)}
/* ── 管理面板 ── */
body.wide{display:block;padding:0}
.wrap{max-width:980px;margin:0 auto;padding:22px 20px 60px}
.top{display:flex;align-items:center;gap:14px;padding:14px 20px;background:var(--bg2);
  border-bottom:1px solid var(--line);position:sticky;top:0;z-index:9}
.brand{font-weight:600;font-size:14.5px;margin-right:6px}
.nav{display:flex;gap:2px;flex-wrap:wrap}
.nav a{padding:6px 11px;border-radius:7px;font-size:13px;color:var(--fg2);text-decoration:none}
.nav a:hover{background:var(--bg3);color:var(--fg)}
.nav a.on{background:#1f6feb22;color:#7cb0ff}
.who{margin-left:auto;font-size:12.5px;color:var(--fg2);display:flex;gap:12px;align-items:center}
h2{font-size:16px;margin:22px 0 12px;font-weight:600}
h2:first-of-type{margin-top:4px}
.banner{border-radius:8px;padding:10px 13px;font-size:12.5px;margin:0 0 14px;line-height:1.6}
.banner.warn{background:#bb800914;border:1px solid #bb800944;color:#e3b341}
.banner.err{background:#f8514914;border:1px solid #f8514944;color:#ff9d97}
.banner.ok{background:#3fb95014;border:1px solid #3fb95044;color:#7ee787}
.banner code{font-family:var(--mono);background:#00000044;padding:1px 5px;border-radius:4px}
table{width:100%;border-collapse:collapse;font-size:13px}
th{text-align:left;font-weight:500;color:var(--fg2);font-size:11.5px;letter-spacing:.4px;
  text-transform:uppercase;padding:8px 10px;border-bottom:1px solid var(--line)}
td{padding:9px 10px;border-bottom:1px solid var(--line);vertical-align:middle}
tr:last-child td{border-bottom:none}
tbody tr:hover{background:#ffffff05}
.mono{font-family:var(--mono);font-size:12.5px}
.dim{color:var(--fg2)}
.badge{display:inline-block;font-size:10.5px;padding:1px 6px;border-radius:99px;
  border:1px solid var(--line);color:var(--fg2);margin-left:6px;white-space:nowrap}
.badge.adm{background:#1f6feb1a;border-color:#1f6feb55;color:#7cb0ff}
.badge.ro{background:#bb80091a;border-color:#bb800955;color:#e3b341}
.row{display:flex;align-items:center;gap:10px;flex-wrap:wrap}
.btn{font:inherit;font-size:12.5px;cursor:pointer;color:var(--fg);background:var(--bg3);
  border:1px solid var(--line);border-radius:7px;padding:6px 11px;transition:.15s;width:auto}
.btn:hover{border-color:#3d6fd4;color:#fff}
.btn.pri{background:#1f6feb;border-color:#1f6feb;color:#fff}
.btn.pri:hover{background:#2a7cf5}
.btn.danger{color:#ff9d97;border-color:#f8514944}
.btn.danger:hover{background:#f8514914;border-color:#f85149}
.btn:disabled{opacity:.45;cursor:not-allowed}
form.inline{display:inline}
fieldset{border:1px solid var(--line);border-radius:9px;padding:14px 15px;margin:0 0 16px}
legend{font-size:12px;color:var(--fg2);padding:0 6px}
label.chk{display:flex;align-items:center;gap:8px;font-size:13px;color:var(--fg);margin:0 0 8px}
label.chk input{width:auto;margin:0}
label.chk .mono{margin:0}
.hint{font-size:11.5px;color:var(--fg3);line-height:1.6;margin:6px 0 0}
.field{margin:0 0 14px}
.field input[type=password],.field input[type=text]{margin-bottom:0}
.grid2{display:grid;grid-template-columns:1fr 1fr;gap:14px}
@media(max-width:640px){.grid2{grid-template-columns:1fr}}
.empty{color:var(--fg3);font-size:13px;padding:18px 0;text-align:center}
"#;

/// 登录页。
pub fn login_page(title: &str, route: &str, next: &str, error: Option<&str>) -> String {
    let err_html = error
        .map(|e| format!("<div class=\"err\">{}</div>", esc(e)))
        .unwrap_or_default();
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>登录 · {title}</title><style>{STYLE}</style></head>
<body>
<form class="card" method="post" action="/__auth/login" autocomplete="on">
  <h1>{title}</h1>
  <p class="sub">要访问的端口：<code>{route}</code></p>
  {err_html}
  <input type="hidden" name="next" value="{next}">
  <label for="u">用户名</label>
  <input id="u" name="user" autocomplete="username" autocapitalize="none"
         autocorrect="off" spellcheck="false" required autofocus>
  <label for="p">口令</label>
  <input id="p" name="pass" type="password" autocomplete="current-password" required>
  <button type="submit">登录</button>
  <div class="foot">会话在你的浏览器里保存一个 cookie，关闭后需重新登录。<br>
  登录后可在 <a href="/__auth/">网关首页</a> 看到你能访问的端口与账号设置。</div>
</form>
</body></html>"#,
        title = esc(title),
        route = esc(route),
        next = esc(next),
        err_html = err_html,
        STYLE = STYLE,
    )
}

/// 错误页（403 / 502 之类，给人看的中文说明）。
pub fn error_page(title: &str, heading: &str, detail_html: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>{heading} · {title}</title><style>{STYLE}</style></head>
<body>
<div class="card">
  <h1>{heading}</h1>
  <p class="sub">{title}</p>
  <div class="detail">{detail_html}</div>
  <div class="foot"><a href="/__auth/logout">换个账号登录</a></div>
</div>
</body></html>"#,
        title = esc(title),
        heading = esc(heading),
        detail_html = detail_html, // 调用方保证已转义
        STYLE = STYLE,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// 网关首页
// ─────────────────────────────────────────────────────────────────────────────

/// 首页上的一个端口入口。
#[derive(Debug, Clone, Serialize)]
pub struct RouteRow {
    pub name: String,
    pub source_label: String,
    /// 已下架：不监听，只列出来供恢复。
    pub disabled: bool,
    /// 能不能**真删除**（只有路由库自己拥有的条目可以；配置里也有同名的就不行，
    /// 因为删掉之后配置那条会复活）。
    pub deletable: bool,
    pub listen: String,
    pub upstream: String,
    /// 剥前缀 / 白名单 / 体积上限，拼成一句话。
    pub detail: String,
    /// 端口是否真的在听（注册表里有它）。理论上总能对上，对不上就是真出事了。
    pub live: bool,
    /// 是不是你现在正用的这个端口。
    pub here: bool,
    pub port: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutesView {
    pub title: String,
    pub actor: String,
    pub is_admin: bool,
    pub rows: Vec<RouteRow>,
    pub store_line: String,
    pub banners: Vec<Banner>,
    pub current_port: u16,
    /// 管理面板是否在独立端口上（决定底部那句提示怎么写）。
    pub dedicated_admin: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteFormView {
    pub title: String,
    pub actor: String,
    pub is_admin: bool,
    pub name: String,
    pub is_new: bool,
    pub name_locked: bool,
    pub listen: String,
    pub upstream: String,
    pub strip_prefix: String,
    pub max_body_bytes: String,
    pub allowed_cidrs: String,
    pub writable: bool,
    pub banners: Vec<Banner>,
}

pub fn ago_text(t: std::time::Instant) -> String {
    let secs = t.elapsed().as_secs();
    if secs < 60 {
        format!("{secs} 秒前")
    } else if secs < 3600 {
        format!("{} 分钟前", secs / 60)
    } else if secs < 86400 {
        format!("{} 小时前", secs / 3600)
    } else {
        format!("{} 天前", secs / 86400)
    }
}

/// 一条提示条。`kind` 取 `warn` / `err` / `ok`。
#[derive(Debug, Clone, Serialize)]
pub struct Banner {
    /// `ok` / `warn` / `err`——前端按它挑颜色。
    pub kind: &'static str,
    pub text: String,
}

impl Banner {
    pub fn warn(t: impl Into<String>) -> Self {
        Self {
            kind: "warn",
            text: t.into(),
        }
    }
    pub fn err(t: impl Into<String>) -> Self {
        Self {
            kind: "err",
            text: t.into(),
        }
    }
    pub fn ok(t: impl Into<String>) -> Self {
        Self {
            kind: "ok",
            text: t.into(),
        }
    }
}

/// 把反引号里的内容渲染成 `<code>`，其余部分转义。
///
/// 提示文案里到处是文件名、账号名（`users_file`、`admin`），手写一长串
/// `<code>` 拼接既难看又容易漏转义。约定一个最小的标记，渲染时统一处理。
#[derive(Debug, Clone, Serialize)]
pub struct UserRow {
    pub name: String,
    pub source_label: String,
    pub editable: bool,
    pub admin: bool,
    pub routes_label: String,
    pub sessions: usize,
}

/// 账号列表页。
#[derive(Debug, Clone, Serialize)]
pub struct UsersView {
    pub title: String,
    pub actor: String,
    pub is_admin: bool,
    pub rows: Vec<UserRow>,
    /// 账号库状态说明（路径 + 是否可写），显示在标题下。
    pub store_line: String,
    pub banners: Vec<Banner>,
    /// (多久前, 动作, 目标, 操作人)
    pub recent: Vec<(String, String, String, String)>,
    pub admin_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserFormView {
    pub title: String,
    pub actor: String,
    pub is_admin: bool,
    pub name: String,
    pub is_new: bool,
    /// 用户名输入框是否锁死（编辑时锁死：改名等于换账号，容易搞混）。
    pub name_locked: bool,
    pub admin: bool,
    /// 已选授权（含 `*`）。
    pub routes: Vec<String>,
    /// 可勾选的 route 名。
    pub available: Vec<String>,
    pub writable: bool,
    pub banners: Vec<Banner>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    /// 短 id（前 12 字符）。完整 sid 不进 HTML，避免一次 XSS 就等于交出会话。
    pub short: String,
    pub user: String,
    pub source: String,
    pub created: String,
    pub seen: String,
    pub admin: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionsView {
    pub title: String,
    pub actor: String,
    pub rows: Vec<SessionRow>,
    pub banners: Vec<Banner>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MeView {
    pub title: String,
    pub actor: String,
    pub is_admin: bool,
    pub name: String,
    pub admin: bool,
    pub routes_label: String,
    pub source_label: String,
    pub writable: bool,
    pub banners: Vec<Banner>,
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_escapes_every_injected_value() {
        let html = login_page("t<\"'>", "r<&>", "/a\"b", Some("e<script>"));
        // 标题 / route / next / 错误信息都不能原样出现
        assert!(!html.contains("<script>"), "错误信息里的标签必须被转义");
        assert!(html.contains("&lt;script&gt;"));
        assert!(
            html.contains("&quot;"),
            "引号要转义（因为它落在 value=\"...\" 里）"
        );
        // 表单必须 POST 到网关自己的端点
        assert!(html.contains(r#"action="/__auth/login""#));
        assert!(html.contains(r#"method="post""#));
        // next 必须作为隐藏字段带回去
        assert!(html.contains(r#"name="next""#));
    }

    #[test]
    fn login_page_without_error_has_no_error_block() {
        let html = login_page("t", "r", "/", None);
        assert!(!html.contains("class=\"err\""));
    }

    #[test]
    fn login_page_marks_password_field_and_autocomplete_hints() {
        let html = login_page("t", "r", "/", None);
        assert!(html.contains(r#"type="password""#));
        assert!(html.contains(r#"autocomplete="username""#));
        assert!(html.contains(r#"autocomplete="current-password""#));
        // 口令框不能被浏览器/搜索引擎收录
        assert!(html.contains(r#"name="robots" content="noindex,nofollow""#));
    }

    #[test]
    fn error_page_renders_given_detail() {
        let html = error_page("网关", "无权访问", "当前账号 <code>bob</code> 被拒绝");
        assert!(html.contains("无权访问"));
        assert!(
            html.contains("<code>bob</code>"),
            "已转义的 detail 应原样渲染"
        );
        // 提供一条回到登录的路径
        assert!(html.contains("/__auth/logout"));
    }

    #[test]
    fn style_is_inlined_and_has_no_external_reference() {
        // 这条是护栏：一旦有人加了 @import 或 <link>，离线环境就会白屏
        assert!(!STYLE.contains("@import"));
        let html = login_page("t", "r", "/", None);
        assert!(!html.contains("<link"), "不能引用外部样式表");
        assert!(!html.contains("http://"), "不能引用外部资源");
        assert!(!html.contains("https://"));
    }
}
