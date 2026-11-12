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
//!
//! # 样式为什么是手写的一份 `STYLE`
//!
//! 登录页是用户看到的**第一屏**，紧接着就是 Vue 面板。两个界面风格不接续，观感上
//! 就是"跳"了一下。所以 `STYLE` 逐值复刻了 `vendor/ui-frame/src/styles/tokens.scss`
//! 里 `[data-theme='dark']` 的那套新拟态令牌（背景 `#1c1c1c`、主色 `#cdfa4e`、
//! 双向阴影、8/24px 圆角），而不是自己另起一套配色。
//!
//! 手写而不是引 Vue 组件库，是因为这一刻前端还没跑起来（见上一段）。
//! **代价是这份 CSS 会漂移**：改 `tokens.scss` 的暗色值时要回来同步这几个变量。
//! 判断标准很简单——登录页与面板并排放，两者之间不该有可见的色差。

use serde::Serialize;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const STYLE: &str = r#"
/* 与 ui-frame 的 dark 主题（`tokens.scss` 的 [data-theme=dark]）逐值对齐。
   登录页是用户看到的第一屏，颜色/圆角/阴影跟面板对不上就会"咔"一下。
   —— 值来自 vendor/ui-frame，改那边时这里要跟着改。 */
:root{
  --nm-bg-color:#1c1c1c;
  --nm-text-primary:#c0c0c0;
  --nm-text-secondary:#999999;
  --nm-text-placeholder:#777777;
  --nm-primary-color:#cdfa4e;
  --nm-text-on-primary:#000000;
  --nm-color-error:#f85149;
  --nm-color-info:#58a6ff;
  --nm-shadow-dark:rgba(0,0,0,.5);
  --nm-shadow-light:rgba(255,255,255,.14);
  --nm-shadow-dark-strong:rgba(0,0,0,.6);
  --nm-shadow-light-strong:rgba(255,255,255,.18);
  --nm-radius-sm:8px;
  --nm-radius-lg:24px;
  --mono:ui-monospace,SFMono-Regular,"SF Mono",Menlo,Consolas,monospace;
}
*{box-sizing:border-box}
html,body{margin:0;padding:0;min-height:100%}
body{
  background:var(--nm-bg-color);color:var(--nm-text-primary);
  font:14px/1.55 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,"Helvetica Neue",Arial,
    "PingFang SC","Microsoft YaHei",sans-serif;
  -webkit-font-smoothing:antialiased;-moz-osx-font-smoothing:grayscale;
  display:flex;align-items:center;justify-content:center;padding:28px 20px;
}
/* 新拟态靠双向阴影表达层次 —— 所以卡片**不描边**，描边会把它打回普通卡片 */
.card{
  width:100%;max-width:392px;background:var(--nm-bg-color);
  border-radius:var(--nm-radius-lg);padding:30px 30px 26px;
  box-shadow:10px 10px 22px var(--nm-shadow-dark),-10px -10px 22px var(--nm-shadow-light);
}
h1{font-size:17px;margin:0 0 5px;font-weight:600;letter-spacing:.2px}
.sub{font-size:12.5px;color:var(--nm-text-secondary);margin:0 0 22px}
.sub code,.detail code{
  font-family:var(--mono);background:var(--nm-bg-color);padding:2px 7px;
  border-radius:6px;box-shadow:inset 2px 2px 5px var(--nm-shadow-dark),inset -2px -2px 5px var(--nm-shadow-light);
}
label{display:block;font-size:12px;color:var(--nm-text-secondary);margin:0 0 6px}
input{
  width:100%;font:inherit;color:var(--nm-text-primary);
  background:var(--nm-bg-color);border:0;border-radius:var(--nm-radius-sm);
  padding:11px 13px;margin-bottom:16px;
  box-shadow:inset 4px 4px 9px var(--nm-shadow-dark),inset -4px -4px 9px var(--nm-shadow-light);
  transition:box-shadow .18s ease;
}
input::placeholder{color:var(--nm-text-placeholder)}
/* 焦点：新拟态的阴影变化对高对比需求不够，额外套一圈主色 */
input:focus{
  outline:none;
  box-shadow:inset 4px 4px 9px var(--nm-shadow-dark),inset -4px -4px 9px var(--nm-shadow-light),
    0 0 0 2px var(--nm-primary-color);
}
button{
  width:100%;font:inherit;font-weight:600;cursor:pointer;
  color:var(--nm-text-on-primary);background:var(--nm-primary-color);
  border:0;border-radius:var(--nm-radius-sm);padding:11px 14px;
  box-shadow:5px 5px 12px var(--nm-shadow-dark-strong),-5px -5px 12px var(--nm-shadow-light-strong);
  transition:transform .12s ease,box-shadow .12s ease,filter .12s ease;
}
@media(hover:hover){button:hover{filter:brightness(1.06)}}
button:active{
  transform:translateY(1px);
  box-shadow:inset 3px 3px 8px rgba(0,0,0,.3),inset -3px -3px 8px rgba(255,255,255,.22);
}
button:focus-visible{outline:2px solid var(--nm-primary-color);outline-offset:3px}
/* 错误条：照 NeumorphismAlert 的做法——红色文字 + 左侧实色竖线，底用内凹 */
.err{
  display:flex;gap:9px;align-items:flex-start;
  color:var(--nm-color-error);font-size:12.5px;line-height:1.6;
  border-radius:var(--nm-radius-sm);border-left:3px solid var(--nm-color-error);
  padding:10px 13px;margin:0 0 16px;background:rgba(248,81,73,.08);
}
.err svg{flex:none;margin-top:2px}
.foot{margin-top:18px;font-size:11.5px;color:var(--nm-text-placeholder);text-align:center;line-height:1.75}
.detail{font-size:13px;color:var(--nm-text-secondary);line-height:1.7}
a{color:var(--nm-color-info);text-decoration:none}
a:hover{text-decoration:underline}
a:focus-visible{outline:2px solid var(--nm-primary-color);outline-offset:2px;border-radius:4px}
@media(prefers-reduced-motion:reduce){*{transition:none!important}}
"#;

/// 登录页。
pub fn login_page(title: &str, route: &str, next: &str, error: Option<&str>) -> String {
    // 错误条照 NeumorphismAlert 的 error 变体：circle-close 图标 + 左竖线 + `role=alert`。
    // 图标内联 SVG —— 这个页面不许有外部资源（网络受限时正是要登录进来改配置的时候）。
    let err_html = error
        .map(|e| {
            format!(
                concat!(
                    r#"<div class="err" role="alert" aria-live="assertive">"#,
                    r#"<svg viewBox="0 0 16 16" width="14" height="14" aria-hidden="true">"#,
                    r#"<circle cx="8" cy="8" r="6.6" fill="none" stroke="currentColor" stroke-width="1.4"/>"#,
                    r#"<path d="M5.7 5.7l4.6 4.6M10.3 5.7l-4.6 4.6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#,
                    r#"</svg><span>{}</span></div>"#
                ),
                esc(e)
            )
        })
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
