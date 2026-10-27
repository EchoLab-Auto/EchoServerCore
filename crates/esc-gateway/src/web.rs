//! 登录页与错误页：**单文件内联**，没有构建步骤、没有外部资源。
//!
//! 和两个面板同一个理由：这个页面要在最受限的环境里也能渲染——如果它依赖
//! CDN 上的字体或脚本，那么在「网络受限」的机器上就白屏，而那正是你需要
//! 登录进去改配置的时候。

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

/// 「管理面板在另一个端口」的说明页。
///
/// 出现在**普通转发端口**上（配了 `admin_listen` 之后）。它不自动跳转：
/// 跳转目标要用 `Host` 拼，而 `Host` 是客户端给的。
pub fn admin_elsewhere_page(title: &str, href: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>管理面板不在这个端口 · {title_esc}</title><style>{STYLE}</style></head>
<body><div class="card">
  <h1>管理面板不在这个端口</h1>
  <p class="sub">这个端口只转发面板，不提供账号管理。</p>
  <div class="detail">管理面板（含登录页）在一个独立端口上：
    <p><a href="{href}">{href}</a></p>
    这样管理面和业务面不共用同一个公网端口，少一处暴露面。
  </div>
  <div class="foot">这是 {title_esc}</div>
</div></body></html>"#,
        title_esc = esc(title),
        href = esc(href),
        STYLE = STYLE,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// 网关首页
// ─────────────────────────────────────────────────────────────────────────────

/// 首页上的一个端口入口。
#[derive(Debug, Clone)]
pub struct HomeLink {
    pub name: String,
    /// 协议相对链接（`//host:port/`），前面挂 TLS 也不用改。
    pub href: String,
}

#[derive(Debug, Clone)]
pub struct HomeView {
    pub title: String,
    pub user: String,
    pub admin: bool,
    pub routes: Vec<HomeLink>,
    /// 当前是从哪个端口进来的，标一下。
    pub current_route: String,
}

pub fn home_page(v: &HomeView) -> String {
    let mut body = String::new();
    body.push_str(&format!("<h2>你好，{}</h2>", esc(&v.user)));
    body.push_str(
        "<p class=\"hint\" style=\"margin-bottom:18px\">\
这是 esc 网关自己的页面。你登录了网关，下面这些端口不用再输口令。\
想回到某个面板，点进去即可。</p>",
    );

    // 入口列表
    body.push_str("<h2>你可以访问</h2>");
    if v.routes.is_empty() {
        body.push_str(
            "<div class=\"banner warn\">这个账号没有任何端口权限。找管理员给 <code>routes</code> 加上端口名。</div>",
        );
    } else {
        body.push_str("<table><thead><tr><th>端口</th><th>入口</th></tr></thead><tbody>");
        for r in &v.routes {
            let here = if r.name == v.current_route {
                "<span class=\"badge\">你在这里</span>"
            } else {
                ""
            };
            body.push_str(&format!(
                "<tr><td class=\"mono\">{name}{here}</td>\
<td><a href=\"{href}\">打开 {name}</a></td></tr>",
                name = esc(&r.name),
                href = esc(&r.href),
                here = here,
            ));
        }
        body.push_str("</tbody></table>");
    }

    // 账号相关
    body.push_str("<h2>账号</h2><div class=\"row\">");
    body.push_str("<a class=\"btn\" href=\"/__admin/me\">我的账号 / 改口令</a>");
    if v.admin {
        body.push_str("<a class=\"btn pri\" href=\"/__admin/\">管理账号</a>");
        body.push_str("<a class=\"btn\" href=\"/__admin/sessions\">在线会话</a>");
    }
    body.push_str("<a class=\"btn\" href=\"/__auth/logout\">退出登录</a></div>");

    if !v.admin {
        body.push_str(
            "<p class=\"hint\">管理账号、给别人发权限需要管理员权限；改自己的口令不需要。</p>",
        );
    }

    // 用首页自己的一套版式（顶栏那套是管理面板的，这里不需要导航）
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>网关 · {title_esc}</title><style>{STYLE}</style></head>
<body class="wide">
<div class="wrap" style="max-width:720px">{body}</div>
</body></html>"#,
        title_esc = esc(&v.title),
        body = body,
        STYLE = STYLE,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// 代理（端口 → 上游）
// ─────────────────────────────────────────────────────────────────────────────

/// 端口列表里的一行。
#[derive(Debug, Clone)]
pub struct RouteRow {
    pub name: String,
    pub source_label: String,
    pub editable: bool,
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

#[derive(Debug, Clone)]
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

pub fn routes_page(v: &RoutesView) -> String {
    let mut body = String::new();
    body.push_str("<h2>代理端口</h2>");
    for b in &v.banners {
        body.push_str(&b.render());
    }
    body.push_str(&format!(
        "<p class=\"hint\" style=\"margin-bottom:14px\">路由库：{}</p>",
        esc(&v.store_line)
    ));

    if v.rows.is_empty() {
        body.push_str("<div class=\"empty\">一个端口都没有——网关现在什么都不转发</div>");
    } else {
        body.push_str(
            "<table><thead><tr><th>端口名</th><th>监听</th><th>上游</th><th>其他</th><th></th></tr></thead><tbody>",
        );
        for r in &v.rows {
            let name = esc(&r.name);
            let here = if r.here {
                "<span class=\"badge adm\">你在这里</span>"
            } else {
                ""
            };
            let ro = if r.editable {
                String::new()
            } else {
                "<span class=\"badge ro\">只读</span>".to_string()
            };
            let dead = if r.live {
                String::new()
            } else {
                "<span class=\"badge ro\" title=\"注册表里找不到这个端口\">未监听</span>"
                    .to_string()
            };
            let mut actions = String::new();
            if r.editable {
                actions.push_str(&format!(
                    "<a class=\"btn\" href=\"/__admin/routes/{n}\">编辑</a>",
                    n = urlencode_attr(&r.name)
                ));
                actions.push_str(&format!(
                    " <form class=\"inline\" method=\"post\" action=\"/__admin/routes/{n}/delete\" \
onsubmit=\"return confirm('删除端口 {n}？它会立刻停止监听。')\">\
<button class=\"btn danger\" type=\"submit\">删除</button></form>",
                    n = urlencode_attr(&r.name)
                ));
            } else {
                actions.push_str(&format!(
                    "<form class=\"inline\" method=\"post\" action=\"/__admin/routes/{n}/adopt\">\
<button class=\"btn\" type=\"submit\">接管</button></form>",
                    n = urlencode_attr(&r.name)
                ));
            }
            body.push_str(&format!(
                "<tr><td><span class=\"mono\">{name}</span>{here}{dead}{ro}\
<div class=\"hint\">{src}</div></td>\
<td class=\"mono\">{listen}</td><td class=\"mono dim\">{upstream}</td>\
<td class=\"hint\">{detail}</td>\
<td><div class=\"row\" style=\"justify-content:flex-end\">{actions}</div></td></tr>",
                src = esc(&r.source_label),
                listen = esc(&r.listen),
                upstream = esc(&r.upstream),
                detail = esc(&r.detail),
            ));
        }
        body.push_str("</tbody></table>");
    }

    body.push_str(
        "<div class=\"row\" style=\"margin-top:16px\">\
<a class=\"btn pri\" href=\"/__admin/routes/new\">新增端口</a>\
<span class=\"hint\">保存即生效：先绑端口，绑不上就整体不变（不会出现改了一半）。</span></div>",
    );
    body.push_str(&format!(
        "<p class=\"hint\">你现在是从端口 <span class=\"mono\">{}</span> 访问管理面板的。{}</p>",
        v.current_port,
        if v.dedicated_admin {
            "它是独立的管理端口，不参与转发——所以删业务端口不会把自己关在外面。"
        } else {
            "管理面板挂在这个端口上，删掉它会把当前连接一起断掉，所以面板不允许这么做。"
        }
    ));

    layout(&v.title, &v.actor, v.is_admin, "routes", &body)
}

/// 新建 / 编辑端口的表单。
#[derive(Debug, Clone)]
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

pub fn route_form_page(v: &RouteFormView) -> String {
    let mut body = String::new();
    let heading = if v.is_new {
        "新增端口".to_string()
    } else {
        format!("编辑端口 {}", v.name)
    };
    body.push_str(&format!("<h2>{}</h2>", esc(&heading)));
    for b in &v.banners {
        body.push_str(&b.render());
    }

    let action = if v.is_new {
        "/__admin/routes".to_string()
    } else {
        format!("/__admin/routes/{}", urlencode_attr(&v.name))
    };
    body.push_str(&format!("<form method=\"post\" action=\"{action}\">"));

    // 名字
    if v.name_locked {
        body.push_str(&format!(
            "<div class=\"field\"><label>端口名</label><div class=\"mono\">{}</div>\
<p class=\"hint\">端口名不可修改——账号的授权是按名字引用的，改名等于换一个端口。\
要改就新建一个再删掉这个。</p></div>",
            esc(&v.name)
        ));
    } else {
        body.push_str(
            "<div class=\"field\"><label for=\"n\">端口名</label>\
<input id=\"n\" name=\"name\" autocomplete=\"off\" autocapitalize=\"none\" spellcheck=\"false\" required \
placeholder=\"例如 server-panel\">\
<p class=\"hint\">只用来标识和授权，不会出现在 URL 里。字母、数字、<code>.</code> <code>-</code> <code>_</code>。</p></div>",
        );
    }

    // 监听 → 上游
    body.push_str(&format!(
        "<div class=\"grid2\">\
<div class=\"field\"><label for=\"l\">对外监听</label>\
<input id=\"l\" name=\"listen\" required placeholder=\"0.0.0.0:2224\" value=\"{listen}\">\
<p class=\"hint\">浏览器连的地址。<code>0.0.0.0</code> = 所有网卡。</p></div>\
<div class=\"field\"><label for=\"u\">上游</label>\
<input id=\"u\" name=\"upstream\" required placeholder=\"127.0.0.1:18788\" value=\"{upstream}\">\
<p class=\"hint\">转发到哪。上游应当只听回环——认证由网关独占。</p></div></div>",
        listen = esc(&v.listen),
        upstream = esc(&v.upstream),
    ));

    // 可选三项
    body.push_str("<fieldset><legend>可选（留空即默认）</legend>");
    body.push_str(&format!(
        "<div class=\"grid2\">\
<div class=\"field\"><label for=\"sp\">剥掉路径前缀</label>\
<input id=\"sp\" name=\"strip_prefix\" placeholder=\"/panel\" value=\"{sp}\">\
<p class=\"hint\">上游按 <code>/</code> 认路径时用。必须以 <code>/</code> 开头。</p></div>\
<div class=\"field\"><label for=\"mb\">请求体上限（字节）</label>\
<input id=\"mb\" name=\"max_body_bytes\" inputmode=\"numeric\" placeholder=\"10485760\" value=\"{mb}\">\
<p class=\"hint\">留空 = 10 MB。只读面板其实用不到大体积上传。</p></div></div>",
        sp = esc(&v.strip_prefix),
        mb = esc(&v.max_body_bytes),
    ));
    body.push_str(&format!(
        "<div class=\"field\"><label for=\"cidrs\">来源白名单（一行一个，留空 = 不限）</label>\
<textarea id=\"cidrs\" name=\"allowed_cidrs\" rows=\"3\" placeholder=\"203.0.113.0/24\" \
style=\"width:100%;font:inherit;background:#0a0d12;color:var(--fg);border:1px solid var(--line);\
border-radius:7px;padding:9px 11px;font-family:var(--mono);font-size:12.5px\">{cidrs}</textarea>\
<p class=\"hint\">和登录认证是<strong>与</strong>关系：既要有权限，也要来自这些网段。</p></div>",
        cidrs = esc(&v.allowed_cidrs),
    ));
    body.push_str("</fieldset>");

    body.push_str(&format!(
        "<div class=\"banner warn\" style=\"margin:0 0 14px\">保存会立刻生效：网关先试着绑上这个端口，\
绑不上就什么都不改（旧端口继续正常工作）。\
要监听 1024 以下的端口，非 root 运行需要额外权限（容器里是 <code>NET_BIND_SERVICE</code>）。</div>\
<div class=\"row\"><button class=\"btn pri\" type=\"submit\"{}>{}</button>\
<a class=\"btn\" href=\"/__admin/routes\">取消</a></div></form>",
        if v.writable { "" } else { " disabled" },
        if v.is_new { "添加并开始监听" } else { "保存并生效" }
    ));

    layout(&v.title, &v.actor, v.is_admin, "routes", &body)
}

// ─────────────────────────────────────────────────────────────────────────────
// 管理面板
// ─────────────────────────────────────────────────────────────────────────────

/// 相对时间。面板上不显示绝对时间戳，理由是这些全是**本次运行内**的短时
/// 事件（会话、刚才的操作），而绝对时间要引入时区与日历格式化——收益为零，
/// 代价是一个依赖和一堆「服务器时区是 UTC 还是 CST」的困惑。
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
#[derive(Debug, Clone)]
pub struct Banner {
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
    fn render(&self) -> String {
        format!(
            "<div class=\"banner {}\">{}</div>",
            self.kind,
            highlight(&self.text)
        )
    }
}

/// 把反引号里的内容渲染成 `<code>`，其余部分转义。
///
/// 提示文案里到处是文件名、账号名（`users_file`、`admin`），手写一长串
/// `<code>` 拼接既难看又容易漏转义。约定一个最小的标记，渲染时统一处理。
fn highlight(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for (i, part) in text.split('`').enumerate() {
        if i % 2 == 1 {
            out.push_str("<code>");
            out.push_str(&esc(part));
            out.push_str("</code>");
        } else {
            out.push_str(&esc(part));
        }
    }
    out
}

/// 面板外壳：顶栏 + 内容。
fn layout(title: &str, actor: &str, is_admin: bool, active: &str, body: &str) -> String {
    let item = |href: &str, label: &str, key: &str| {
        let on = if key == active { " class=\"on\"" } else { "" };
        format!("<a href=\"{href}\"{on}>{label}</a>")
    };
    let mut nav = item("/__admin/", "账号", "users");
    if is_admin {
        nav.push_str(&item("/__admin/routes", "代理", "routes"));
        nav.push_str(&item("/__admin/sessions", "会话", "sessions"));
    }
    nav.push_str(&item("/__admin/me", "我的账号", "me"));
    // 回网关首页的出口。否则进去了就只能靠浏览器后退键。
    nav.push_str(&item("/__auth/", "网关首页", "home"));

    let role = if is_admin { "管理员" } else { "用户" };
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>{title_esc} · 管理</title><style>{STYLE}</style></head>
<body class="wide">
<div class="top">
  <span class="brand">{title_esc}</span>
  <nav class="nav">{nav}</nav>
  <span class="who"><span>{actor_esc}<span class="badge">{role}</span></span>
    <a href="/__auth/logout">退出</a></span>
</div>
<div class="wrap">{body}</div>
</body></html>"#,
        title_esc = esc(title),
        actor_esc = esc(actor),
        nav = nav,
        role = role,
        body = body,
        STYLE = STYLE,
    )
}

/// 账号列表里的一行。
#[derive(Debug, Clone)]
pub struct UserRow {
    pub name: String,
    pub source_label: String,
    pub editable: bool,
    pub admin: bool,
    pub routes_label: String,
    pub sessions: usize,
}

/// 账号列表页。
#[derive(Debug, Clone)]
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

pub fn users_page(v: &UsersView) -> String {
    let mut body = String::new();
    body.push_str("<h2>账号</h2>");
    for b in &v.banners {
        body.push_str(&b.render());
    }
    body.push_str(&format!(
        "<p class=\"hint\" style=\"margin-bottom:14px\">账号库：{}</p>",
        esc(&v.store_line)
    ));

    if v.rows.is_empty() {
        body.push_str("<div class=\"empty\">还没有账号</div>");
    } else {
        body.push_str(
            "<table><thead><tr><th>账号</th><th>授权端口</th><th>会话</th><th></th></tr></thead><tbody>",
        );
        for r in &v.rows {
            let name = esc(&r.name);
            let adm = if r.admin {
                "<span class=\"badge adm\">管理员</span>"
            } else {
                ""
            };
            let ro = if r.editable {
                String::new()
            } else {
                "<span class=\"badge ro\">只读</span>".to_string()
            };
            let mut actions = String::new();
            if r.editable {
                actions.push_str(&format!(
                    "<a class=\"btn\" href=\"/__admin/users/{name_enc}\">编辑</a>",
                    name_enc = urlencode_attr(&r.name)
                ));
                if r.sessions > 0 {
                    actions.push_str(&format!(
                        " <form class=\"inline\" method=\"post\" action=\"/__admin/users/{}/revoke\">\
<button class=\"btn\" type=\"submit\">踢下线</button></form>",
                        urlencode_attr(&r.name)
                    ));
                }
            } else {
                actions.push_str(&format!(
                    "<form class=\"inline\" method=\"post\" action=\"/__admin/users/{}/adopt\">\
<button class=\"btn\" type=\"submit\">接管</button></form>",
                    urlencode_attr(&r.name)
                ));
            }
            body.push_str(&format!(
                "<tr><td><span class=\"mono\">{name}</span>{adm}{ro}<div class=\"hint\">{src}</div></td>\
<td class=\"mono dim\">{routes}</td><td class=\"dim\">{sess}</td>\
<td><div class=\"row\" style=\"justify-content:flex-end\">{actions}</div></td></tr>",
                src = esc(&r.source_label),
                routes = esc(&r.routes_label),
                sess = r.sessions,
                actions = actions,
            ));
        }
        body.push_str("</tbody></table>");
    }

    if v.is_admin {
        body.push_str(&format!(
            "<div class=\"row\" style=\"margin-top:16px\">\
<a class=\"btn pri\" href=\"/__admin/users/new\">新建账号</a>\
<span class=\"hint\">共 {} 个账号，其中 {} 个管理员。网关至少要有 1 个管理员。</span></div>",
            v.rows.len(),
            v.admin_count
        ));

        if !v.recent.is_empty() {
            body.push_str("<h2>最近操作</h2><table><thead><tr><th>时间</th><th>动作</th><th>目标</th><th>操作人</th></tr></thead><tbody>");
            for (when, action, target, actor) in &v.recent {
                body.push_str(&format!(
                    "<tr><td class=\"dim\">{}</td><td>{}</td><td class=\"mono\">{}</td><td class=\"dim\">{}</td></tr>",
                    esc(when),
                    esc(action),
                    esc(target),
                    esc(actor),
                ));
            }
            body.push_str("</tbody></table><p class=\"hint\">只保留本次运行内的最近 50 条，重启清空；完整审计看网关日志（tracing）。</p>");
        }
    }

    layout(&v.title, &v.actor, v.is_admin, "users", &body)
}

/// 新建 / 编辑账号的表单。
#[derive(Debug, Clone)]
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

pub fn user_form_page(v: &UserFormView) -> String {
    let mut body = String::new();
    let heading = if v.is_new {
        "新建账号".to_string()
    } else {
        format!("编辑账号 {}", v.name)
    };
    body.push_str(&format!("<h2>{}</h2>", esc(&heading)));
    for b in &v.banners {
        body.push_str(&b.render());
    }

    let action = if v.is_new {
        "/__admin/users".to_string()
    } else {
        format!("/__admin/users/{}", urlencode_attr(&v.name))
    };

    body.push_str(&format!(
        "<form method=\"post\" action=\"{action}\">",
        action = action
    ));

    // 用户名
    if v.name_locked {
        body.push_str(&format!(
            "<div class=\"field\"><label>账号</label><div class=\"mono\">{}</div>\
<p class=\"hint\">账号名不可修改。要换名字请新建一个再删掉这个。</p></div>",
            esc(&v.name)
        ));
    } else {
        body.push_str(
            "<div class=\"field\"><label for=\"n\">账号</label>\
<input id=\"n\" name=\"name\" autocomplete=\"off\" autocapitalize=\"none\" spellcheck=\"false\" required \
placeholder=\"只允许字母、数字、点、下划线、减号\">\
<p class=\"hint\">只允许字母、数字、<code>.</code> <code>-</code> <code>_</code>。</p></div>",
        );
    }

    // 口令
    let pw_label = if v.is_new {
        "口令"
    } else {
        "新口令（留空 = 不修改）"
    };
    let pw_required = if v.is_new { " required" } else { "" };
    body.push_str(&format!(
        "<div class=\"grid2\"><div class=\"field\"><label for=\"p\">{pw_label}</label>\
<input id=\"p\" name=\"pass\" type=\"password\" autocomplete=\"new-password\"{pw_required}></div>\
<div class=\"field\"><label for=\"p2\">再输一次</label>\
<input id=\"p2\" name=\"pass2\" type=\"password\" autocomplete=\"new-password\"{pw_required}></div></div>\
<p class=\"hint\">至少 8 个字符。改口令会让这个账号**其他**已登录的会话立刻失效。</p>",
        pw_label = esc(pw_label),
        pw_required = pw_required,
    ));

    // 授权端口
    body.push_str("<fieldset><legend>可以访问哪些端口</legend>");
    let all_checked = v.routes.iter().any(|r| r == "*");
    body.push_str(&format!(
        "<label class=\"chk\"><input type=\"checkbox\" name=\"all_routes\" value=\"1\"{}>\
<span>全部端口 <span class=\"badge\">含以后新增的</span></span></label>",
        if all_checked { " checked" } else { "" }
    ));
    body.push_str("<div class=\"hint\" style=\"margin:10px 0 6px\">或者逐个指定：</div>");
    if v.available.is_empty() {
        body.push_str("<div class=\"empty\">配置里没有 route</div>");
    }
    for r in &v.available {
        let checked = v.routes.iter().any(|x| x == r);
        body.push_str(&format!(
            "<label class=\"chk\"><input type=\"checkbox\" name=\"route\" value=\"{val}\"{}>\
<span class=\"mono\">{val}</span></label>",
            if checked { " checked" } else { "" },
            val = esc(r)
        ));
    }
    body.push_str("</fieldset>");

    // 管理员
    body.push_str(&format!(
        "<fieldset><legend>管理权限</legend>\
<label class=\"chk\"><input type=\"checkbox\" name=\"admin\" value=\"1\"{}>\
<span>允许进入管理面板（能改所有账号、能给别人发权限）</span></label>\
<p class=\"hint\">这一项等于把「给所有人发权限」的能力交出去，只在确实需要时打开。</p></fieldset>",
        if v.admin { " checked" } else { "" }
    ));

    body.push_str(&format!(
        "<div class=\"row\"><button class=\"btn pri\" type=\"submit\"{}>{}</button>\
<a class=\"btn\" href=\"/__admin/\">取消</a></div></form>",
        if v.writable { "" } else { " disabled" },
        if v.is_new { "创建" } else { "保存" }
    ));

    layout(&v.title, &v.actor, v.is_admin, "users", &body)
}

/// 会话列表里的一行。
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// 短 id（前 12 字符）。完整 sid 不进 HTML，避免一次 XSS 就等于交出会话。
    pub short: String,
    pub user: String,
    pub source: String,
    pub created: String,
    pub seen: String,
    pub admin: bool,
}

#[derive(Debug, Clone)]
pub struct SessionsView {
    pub title: String,
    pub actor: String,
    pub rows: Vec<SessionRow>,
    pub banners: Vec<Banner>,
}

pub fn sessions_page(v: &SessionsView) -> String {
    let mut body = String::new();
    body.push_str("<h2>在线会话</h2>");
    for b in &v.banners {
        body.push_str(&b.render());
    }
    if v.rows.is_empty() {
        body.push_str("<div class=\"empty\">当前没有在线会话</div>");
    } else {
        body.push_str("<table><thead><tr><th>账号</th><th>来源</th><th>登录于</th><th>最近活动</th><th></th></tr></thead><tbody>");
        for r in &v.rows {
            let adm = if r.admin {
                "<span class=\"badge adm\">管理员</span>"
            } else {
                ""
            };
            body.push_str(&format!(
                "<tr><td><span class=\"mono\">{}</span>{}</td><td class=\"mono dim\">{}</td>\
<td class=\"dim\">{}</td><td class=\"dim\">{}</td>\
<td><div class=\"row\" style=\"justify-content:flex-end\">\
<form class=\"inline\" method=\"post\" action=\"/__admin/sessions/{}/revoke\">\
<button class=\"btn danger\" type=\"submit\">踢下线</button></form></div></td></tr>",
                esc(&r.user),
                adm,
                esc(&r.source),
                esc(&r.created),
                esc(&r.seen),
                esc(&r.short),
            ));
        }
        body.push_str("</tbody></table>");
    }
    body.push_str(
        "<p class=\"hint\">会话存在内存里，网关重启即全部失效。这里只显示被截短后的会话 id。</p>",
    );
    layout(&v.title, &v.actor, true, "sessions", &body)
}

/// 「我的账号」页。
#[derive(Debug, Clone)]
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

pub fn me_page(v: &MeView) -> String {
    let mut body = String::from("<h2>我的账号</h2>");
    for b in &v.banners {
        body.push_str(&b.render());
    }
    body.push_str(&format!(
        "<table><tbody>\
<tr><td class=\"dim\">账号</td><td class=\"mono\">{}</td></tr>\
<tr><td class=\"dim\">授权端口</td><td class=\"mono\">{}</td></tr>\
<tr><td class=\"dim\">管理权限</td><td>{}</td></tr>\
<tr><td class=\"dim\">账号来源</td><td>{}</td></tr>\
</tbody></table>",
        esc(&v.name),
        esc(&v.routes_label),
        if v.admin { "是" } else { "否" },
        esc(&v.source_label),
    ));

    body.push_str("<h2>改口令</h2>");
    body.push_str(&format!(
        "<form method=\"post\" action=\"/__admin/me\">\
<div class=\"field\"><label for=\"o\">当前口令</label>\
<input id=\"o\" name=\"old\" type=\"password\" autocomplete=\"current-password\" required></div>\
<div class=\"grid2\"><div class=\"field\"><label for=\"p\">新口令</label>\
<input id=\"p\" name=\"pass\" type=\"password\" autocomplete=\"new-password\" required></div>\
<div class=\"field\"><label for=\"p2\">再输一次</label>\
<input id=\"p2\" name=\"pass2\" type=\"password\" autocomplete=\"new-password\" required></div></div>\
<p class=\"hint\">要输当前口令：光有 cookie 就能改口令的话，一次会话劫持就变成了永久占有。\
改完之后这个账号的其他会话会立刻失效，当前这个保留。</p>\
<div class=\"row\"><button class=\"btn pri\" type=\"submit\"{}>改口令</button></div></form>",
        if v.writable { "" } else { " disabled" }
    ));

    layout(&v.title, &v.actor, v.is_admin, "me", &body)
}

/// 属性值里的百分号编码（账号名只含 ASCII 标识符字符，但仍然统一走一遍）。
fn urlencode_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
