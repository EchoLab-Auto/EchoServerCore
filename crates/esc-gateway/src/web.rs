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
  <div class="foot">会话在你的浏览器里保存一个 cookie，关闭后需重新登录。</div>
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_escapes_every_injected_value() {
        let html = login_page("t<\"'>", "r<&>", "/a\"b", Some("e<script>"));
        // 标题 / route / next / 错误信息都不能原样出现
        assert!(!html.contains("<script>"), "错误信息里的标签必须被转义");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&quot;"), "引号要转义（因为它落在 value=\"...\" 里）");
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
        assert!(html.contains("<code>bob</code>"), "已转义的 detail 应原样渲染");
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
