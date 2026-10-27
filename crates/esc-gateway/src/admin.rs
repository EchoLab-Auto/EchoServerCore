//! 管理面板：账号的增删改查、在线会话、自助改口令。
//!
//! # 为什么它挂在网关自己身上
//!
//! 网关是**唯一**知道「谁是谁、谁能看哪个端口」的进程。把账号管理放到别处
//! （另一个服务、另一个端口、另一个数据库）意味着两套事实来源，而它们迟早
//! 会不一致。所以面板就是网关的一个内部端点：`/__admin/`，不转发上游。
//!
//! 不额外开端口：2223/2224 已经是公网入口、已经有 TLS 与防火墙策略，多开
//! 一个端口就多一处需要同步维护的东西。想「只从内网改账号」用
//! `admin_allowed_cidrs`，不必为此多占端口。
//!
//! # 安全边界
//!
//! | 威胁 | 对策 |
//! |---|---|
//! | 未登录访问 | 和普通页面同一套会话；没有会话 → 302 登录页 |
//! | 普通用户越权 | `session.admin`（登录时固化），非管理员的请求在分发前就被拒 |
//! | CSRF | `SameSite=Strict` 是主防线，另加 POST 必过 `same_origin` |
//! | 会话被劫持后改口令 | 自助改口令**必须输当前口令**，且同样走登录失败限速 |
//! | 改完权限旧会话还在用 | 每次变更显式 `revoke_user`（除当前会话） |
//! | 把自己锁在门外 | 禁止删掉/降级最后一个管理员；配置文件账号永远可登 |
//! | XSS | 所有输出过 `esc`；会话 id 只显示前缀；`X-Frame-Options: DENY` |
//! | 缓存泄露 | 所有面板响应 `Cache-Control: no-store` |
//!
//! # 一个刻意的「麻烦」
//!
//! 来自 `gateway.toml` 的账号在面板里是**只读**的，要改动得先点「接管」
//! （把哈希复制进账号库，从此以账号库为准）。这一下点击换来的是：运维手改
//! 配置文件不会被面板悄悄覆盖，而且配置文件始终留作救援通道。

use std::net::IpAddr;

use axum::http::{header, HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::{Request as HyperRequest, Response as HyperResponse};

use crate::auth::{self, Denied};
use crate::config::valid_user_name;
use crate::proxy::{html, json_error, redirect, text, urlencode, Account, Gateway, ProxyBody};
use crate::store::MIN_PASSWORD_LEN;
use crate::web::{
    self, Banner, MeView, SessionRow, SessionsView, UserFormView, UserRow, UsersView,
};

/// 面板端点的公共前缀。单独提出来是因为它同时出现在 URL 匹配、登录跳转和
/// 文档里，改一处漏一处就会出现「点了跳到 404」。
const PREFIX: &str = "/__admin";

/// 表单体上限。账号表单最多几百字节，8 KB 已经非常宽松。
const MAX_FORM_BYTES: usize = 8 * 1024;

pub async fn handle(
    gw: &Gateway,
    source: Option<IpAddr>,
    req: HyperRequest<Incoming>,
) -> HyperResponse<ProxyBody> {
    let path = req.uri().path().to_string();

    // ① 来源白名单。放在最前面：不在名单里就连「需要登录」都不告诉他。
    if !gw.cfg.admin_source_allowed(source) {
        tracing::warn!(source = ?source, "admin panel source not allowed");
        return page(
            StatusCode::FORBIDDEN,
            web::error_page(
                gw.title(),
                "管理面板不可达",
                "当前来源地址不在 <code>admin_allowed_cidrs</code> 允许范围内。",
            ),
        );
    }

    // ② 少一个斜杠也认，尽量别让人对着 404 发愣
    if path == PREFIX {
        return redirect(StatusCode::FOUND, &format!("{PREFIX}/"));
    }

    let want_html = auth::wants_html(req.headers());
    let sid_owned = auth::cookie_value(req.headers(), &gw.cfg.cookie_name).map(str::to_string);
    let session = match sid_owned.as_deref().and_then(|s| gw.auth.get(s)) {
        Some(s) => s,
        None => {
            // 和普通页面同一条规则：浏览器跳登录页，脚本拿 401 JSON
            if want_html {
                return redirect(
                    StatusCode::FOUND,
                    &format!(
                        "/__auth/login?next={}",
                        urlencode(&format!("{path}{}", query_suffix(&req)))
                    ),
                );
            }
            return json_error(Denied::Unauthenticated, "管理面板需要登录");
        }
    };

    let segs = segments(&path);

    // ③ 普通用户只有「我的账号」这一亩地。其余在分发前就拒掉，
    //    这样即使将来加了新端点，也不会忘记加权限判断。
    let is_self_only = matches!(segs.as_slice(), [] | ["me"]);
    if !session.admin && !is_self_only {
        tracing::warn!(user = %session.user, path = %path, "non-admin tried to open the admin panel");
        return page(
            StatusCode::FORBIDDEN,
            web::error_page(
                gw.title(),
                "需要管理员权限",
                &format!(
                    "账号 <code>{}</code> 不是管理员。可以访问的只有 <a href=\"{PREFIX}/me\">我的账号</a>。",
                    crate::proxy::html_escape(&session.user)
                ),
            ),
        );
    }

    let is_post = req.method() == Method::POST;

    // ④ 所有变更都是 POST，且必须过 Origin 校验（纵深防御，主防线是 SameSite）
    if is_post && !auth::same_origin(req.headers()) {
        tracing::warn!(user = %session.user, source = ?source, path = %path, "cross-origin admin write rejected");
        return page(
            StatusCode::FORBIDDEN,
            web::error_page(
                gw.title(),
                "跨站请求被拒绝",
                "这个表单不是从本网关的页面提交的。",
            ),
        );
    }
    if !is_post && req.method() != Method::GET && req.method() != Method::HEAD {
        return text(StatusCode::METHOD_NOT_ALLOWED, "只支持 GET 与 POST");
    }

    // flash 从会话里取一次就清掉：刷新页面不会再重复出现，
    // 也没法靠构造链接往里塞内容。
    let sid = sid_owned.unwrap_or_default();
    let flash = Flash::take(gw, &sid);
    let ctx = Ctx {
        gw,
        actor: session.user.clone(),
        admin: session.admin,
        sid,
        source,
        flash,
    };

    match (is_post, segs.as_slice()) {
        // 账号
        (false, []) => ctx.users_page(),
        (false, ["users", "new"]) => ctx.user_form_new(),
        (true, ["users"]) => ctx.user_create(req).await,
        (false, ["users", name]) => ctx.user_form_edit(name),
        (true, ["users", name]) => ctx.user_update(name, req).await,
        (true, ["users", name, "delete"]) => ctx.user_delete(name),
        (true, ["users", name, "adopt"]) => ctx.user_adopt(name),
        (true, ["users", name, "revoke"]) => ctx.user_revoke_sessions(name),

        // 会话
        (false, ["sessions"]) => ctx.sessions_page(),
        (true, ["sessions", id, "revoke"]) => ctx.session_revoke(id),

        // 我的账号
        (false, ["me"]) => ctx.me_page(None),
        (true, ["me"]) => ctx.me_change_password(req).await,

        _ => text(StatusCode::NOT_FOUND, "没有这个管理端点"),
    }
}

/// 一次请求的上下文，省得每个处理函数都拖一长串参数。
struct Ctx<'a> {
    gw: &'a Gateway,
    actor: String,
    admin: bool,
    sid: String,
    source: Option<IpAddr>,
    flash: Flash,
}

/// 一次性提示。来源是**会话**，不是 URL —— 见 `auth::Session::flash`。
#[derive(Debug)]
struct Flash(Option<Banner>);

impl Flash {
    fn take(gw: &Gateway, sid: &str) -> Self {
        Self(gw.auth.take_flash(sid).map(|(ok, msg)| {
            if ok {
                Banner::ok(msg)
            } else {
                Banner::err(msg)
            }
        }))
    }

    fn banners(&self) -> Vec<Banner> {
        self.0.iter().cloned().collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 页面：账号列表
// ─────────────────────────────────────────────────────────────────────────────

impl Ctx<'_> {
    fn users_page(&self) -> HyperResponse<ProxyBody> {
        // 非管理员走到这里是 `/__admin/`，直接带他去看自己的账号
        if !self.admin {
            return redirect(StatusCode::FOUND, &format!("{PREFIX}/me"));
        }

        let gw = self.gw;
        let sessions = gw.auth.list();
        let accounts = gw.accounts();

        let rows: Vec<UserRow> = accounts
            .iter()
            .map(|a| UserRow {
                name: a.user.name.clone(),
                source_label: a.source.label().to_string(),
                editable: a.source.editable(),
                admin: a.user.admin,
                routes_label: routes_label(&a.user),
                sessions: sessions
                    .iter()
                    .filter(|(_, s)| s.user == a.user.name)
                    .count(),
            })
            .collect();

        let mut banners = self.flash.banners();
        banners.extend(self.store_banners());
        if !accounts.iter().any(|a| a.user.admin) {
            banners.push(Banner::warn(
                "还没有管理员账号，谁也进不了这个面板。编辑某个账号勾上「管理权限」，\
                 或给 `gateway.toml` 里的某个 `[[user]]` 加 `admin = true`。",
            ));
        }
        for a in &accounts {
            if a.source == crate::proxy::AccountSource::StoreOverConfig {
                banners.push(Banner::warn(format!(
                    "账号 `{}` 同时存在于账号库和 gateway.toml，当前以账号库为准。\
                     建议从 gateway.toml 里删掉它，避免以后改错地方。",
                    a.user.name
                )));
            }
        }

        let view = UsersView {
            title: gw.title().to_string(),
            actor: self.actor.clone(),
            is_admin: true,
            rows,
            store_line: store_line(gw),
            banners,
            recent: gw
                .store
                .recent()
                .into_iter()
                .map(|a| (web::ago_text(a.at), a.action, a.target, a.actor))
                .collect(),
            admin_count: gw.admin_count(),
        };
        let body = web::users_page(&view);
        page(StatusCode::OK, body)
    }

    /// 账号库自身的状态说明：可写 / 只读原因 / 坏条目。
    fn store_banners(&self) -> Vec<Banner> {
        let store = &self.gw.store;
        let mut v = Vec::new();
        if let Some(note) = store.note() {
            v.push(Banner::warn(note));
        }
        for r in store.rejected() {
            v.push(Banner::err(format!("账号库里有条目被拒绝（已跳过）：{r}")));
        }
        v
    }

    // ── 新建 ──

    fn user_form_new(&self) -> HyperResponse<ProxyBody> {
        self.render_form(FormState {
            name: String::new(),
            is_new: true,
            name_locked: false,
            admin: false,
            routes: vec!["*".to_string()],
            banners: self.flash.banners(),
        })
    }

    async fn user_create(&self, req: HyperRequest<Incoming>) -> HyperResponse<ProxyBody> {
        let form = match read_form(req).await {
            Ok(f) => f,
            Err(e) => return text(StatusCode::BAD_REQUEST, e),
        };
        let input = UserInput::from_form(&form);

        let routes = match input.validate(self.gw, true, None) {
            Ok(r) => r,
            Err(e) => {
                return self.render_form(FormState {
                    name: input.name.clone(),
                    is_new: true,
                    name_locked: false,
                    admin: input.admin,
                    routes: input.selected_routes(),
                    banners: vec![Banner::err(e)],
                })
            }
        };

        match self
            .gw
            .store
            .create(&self.actor, &input.name, &input.pass, routes, input.admin)
        {
            Ok(()) => {
                tracing::info!(
                    actor = %self.actor, target = %input.name, source = ?self.source,
                    admin = input.admin, "account created"
                );
                self.done(
                    true,
                    &format!("{PREFIX}/"),
                    &format!("账号 {} 已创建", input.name),
                )
            }
            Err(e) => self.render_form(FormState {
                name: input.name.clone(),
                is_new: true,
                name_locked: false,
                admin: input.admin,
                routes: input.selected_routes(),
                banners: vec![Banner::err(e)],
            }),
        }
    }

    // ── 编辑 ──

    fn user_form_edit(&self, name: &str) -> HyperResponse<ProxyBody> {
        let Some(acct) = self.account(name) else {
            return self.not_found_user(name);
        };
        let mut banners = self.flash.banners();
        if !acct.source.editable() {
            // 列表页对这类账号只给「接管」按钮，正常点不到这里；
            // 直接敲 URL 才会看到——那就把话说清楚，别让人白填一遍表单。
            banners.push(Banner::warn(format!(
                "账号 {} 来自 gateway.toml，面板里改不了。保存前要先在列表里「接管」它。",
                acct.user.name
            )));
        }
        self.render_form(FormState {
            name: acct.user.name.clone(),
            is_new: false,
            name_locked: true,
            admin: acct.user.admin,
            routes: raw_routes(&acct.user),
            banners,
        })
    }

    async fn user_update(
        &self,
        name: &str,
        req: HyperRequest<Incoming>,
    ) -> HyperResponse<ProxyBody> {
        let Some(acct) = self.account(name) else {
            return self.not_found_user(name);
        };
        if !acct.source.editable() {
            return self.done(
                false,
                &format!("{PREFIX}/"),
                &format!(
                    "账号 {name} 来自 gateway.toml，面板里改不了。先点「接管」把它复制进账号库。"
                ),
            );
        }
        let form = match read_form(req).await {
            Ok(f) => f,
            Err(e) => return text(StatusCode::BAD_REQUEST, e),
        };
        let input = UserInput::from_form(&form);
        let routes = match input.validate(self.gw, false, Some(name)) {
            Ok(r) => r,
            Err(e) => {
                return self.render_form(FormState {
                    name: name.to_string(),
                    is_new: false,
                    name_locked: true,
                    admin: input.admin,
                    routes: input.selected_routes(),
                    banners: vec![Banner::err(e)],
                })
            }
        };

        // 最后一个管理员不能被降级——否则从此没人能进面板
        if acct.user.admin && !input.admin && self.gw.admin_count() <= 1 {
            return self.render_form(FormState {
                name: name.to_string(),
                is_new: false,
                name_locked: true,
                admin: input.admin,
                routes: input.selected_routes(),
                banners: vec![Banner::err(format!(
                    "{name} 是最后一个管理员，不能取消管理权限。先给别的账号勾上管理权限。"
                ))],
            });
        }

        let new_pass = if input.pass.is_empty() {
            None
        } else {
            Some(input.pass.as_str())
        };
        match self
            .gw
            .store
            .update(&self.actor, name, new_pass, routes, input.admin)
        {
            Ok(()) => {
                // 权限 / 口令变了，旧会话必须立刻作废（当前这个除外）
                let keep = (name == self.actor).then_some(self.sid.as_str());
                let killed = self.gw.auth.revoke_user(name, keep);
                tracing::info!(
                    actor = %self.actor, target = %name, source = ?self.source,
                    sessions_revoked = killed, "account updated"
                );
                let extra = if killed > 0 {
                    format!("，已作废 {killed} 个旧会话")
                } else {
                    String::new()
                };
                self.done(
                    true,
                    &format!("{PREFIX}/"),
                    &format!("账号 {name} 已保存{extra}"),
                )
            }
            Err(e) => self.render_form(FormState {
                name: name.to_string(),
                is_new: false,
                name_locked: true,
                admin: input.admin,
                routes: input.selected_routes(),
                banners: vec![Banner::err(e)],
            }),
        }
    }

    // ── 删除 / 接管 / 踢下线 ──

    fn user_delete(&self, name: &str) -> HyperResponse<ProxyBody> {
        let Some(acct) = self.account(name) else {
            return self.not_found_user(name);
        };
        if !acct.source.editable() {
            return self.done(
                false,
                &format!("{PREFIX}/"),
                &format!("账号 {name} 来自 gateway.toml，请改配置文件删掉它"),
            );
        }
        if acct.user.admin && self.gw.admin_count() <= 1 {
            return self.done(
                false,
                &format!("{PREFIX}/"),
                &format!("{name} 是最后一个管理员，不能删除。先给别的账号勾上管理权限。"),
            );
        }
        // 删自己会让当前会话立刻失效，用户看到的是一个莫名其妙的登录页。
        // 与其「删成功然后被弹出去」，不如说清楚：换一个管理员账号来删。
        if acct.user.name == self.actor {
            return self.done(
                false,
                &format!("{PREFIX}/"),
                "不能用正在使用的账号删除自己。请换另一个管理员账号来删，或直接改 gateway.toml。",
            );
        }
        match self.gw.store.delete(&self.actor, name) {
            Ok(()) => {
                let killed = self.gw.auth.revoke_user(name, None);
                tracing::info!(
                    actor = %self.actor, target = %name, source = ?self.source,
                    sessions_revoked = killed, "account deleted"
                );
                self.done(true, &format!("{PREFIX}/"), &format!("账号 {name} 已删除"))
            }
            Err(e) => self.done(false, &format!("{PREFIX}/"), &e),
        }
    }

    fn user_adopt(&self, name: &str) -> HyperResponse<ProxyBody> {
        let Some(acct) = self.account(name) else {
            return self.not_found_user(name);
        };
        if acct.source != crate::proxy::AccountSource::Config {
            return self.done(
                false,
                &format!("{PREFIX}/"),
                &format!("账号 {name} 已经在账号库里了"),
            );
        }
        match self.gw.store.adopt(&self.actor, &acct.user) {
            Ok(()) => {
                tracing::info!(actor = %self.actor, target = %name, "config account adopted into the store");
                self.done(
                    true,
                    &format!("{PREFIX}/"),
                    &format!(
                        "账号 {name} 已接管进账号库，现在可以编辑了。\
                         它在 gateway.toml 里的那条现在被覆盖——建议删掉以免混淆。"
                    ),
                )
            }
            Err(e) => self.done(false, &format!("{PREFIX}/"), &e),
        }
    }

    fn user_revoke_sessions(&self, name: &str) -> HyperResponse<ProxyBody> {
        let keep = (name == self.actor).then_some(self.sid.as_str());
        let killed = self.gw.auth.revoke_user(name, keep);
        tracing::info!(
            actor = %self.actor, target = %name, source = ?self.source,
            sessions_revoked = killed, "sessions revoked by admin"
        );
        let msg = if killed == 0 {
            format!("账号 {name} 没有其他在线会话")
        } else {
            format!("已踢下线 {killed} 个会话（{name}）")
        };
        self.done(true, &format!("{PREFIX}/"), &msg)
    }

    // ── 会话 ──

    fn sessions_page(&self) -> HyperResponse<ProxyBody> {
        let rows: Vec<SessionRow> = self
            .gw
            .auth
            .list()
            .into_iter()
            .map(|(sid, s)| SessionRow {
                short: sid.chars().take(12).collect(),
                user: s.user,
                source: s
                    .source
                    .map(|ip| ip.to_string())
                    .unwrap_or_else(|| "—".into()),
                created: web::ago_text(s.created),
                seen: web::ago_text(s.last_seen),
                admin: s.admin,
            })
            .collect();
        let view = SessionsView {
            title: self.gw.title().to_string(),
            actor: self.actor.clone(),
            rows,
            banners: self.flash.banners(),
        };
        page(StatusCode::OK, web::sessions_page(&view))
    }

    fn session_revoke(&self, short_id: &str) -> HyperResponse<ProxyBody> {
        let all = self.gw.auth.list();
        let matches: Vec<String> = all
            .iter()
            .filter(|(sid, _)| sid.starts_with(short_id))
            .map(|(sid, _)| sid.clone())
            .collect();
        match matches.as_slice() {
            [] => self.done(
                true,
                &format!("{PREFIX}/sessions"),
                "这个会话已经不在线了（可能自己退出了）",
            ),
            [sid] if *sid == self.sid => self.done(
                false,
                &format!("{PREFIX}/sessions"),
                "不能在这里踢掉自己——要退出请用右上角的「退出」",
            ),
            [sid] => {
                let who = all
                    .iter()
                    .find(|(s, _)| s == sid)
                    .map(|(_, sess)| sess.user.clone())
                    .unwrap_or_default();
                self.gw.auth.logout(sid);
                tracing::info!(
                    actor = %self.actor, target = %who, source = ?self.source,
                    "session revoked by admin"
                );
                self.done(
                    true,
                    &format!("{PREFIX}/sessions"),
                    &format!("已踢下线：{who}"),
                )
            }
            _ => self.done(
                false,
                &format!("{PREFIX}/sessions"),
                "这个前缀对应多个会话，请用账号列表里的「踢下线」（按账号全踢）",
            ),
        }
    }

    // ── 我的账号 ──

    fn me_page(&self, error: Option<Banner>) -> HyperResponse<ProxyBody> {
        let Some(acct) = self.account(&self.actor) else {
            // 会话还在但账号已经被删掉了：诚实地说出来并让他退出
            return page(
                StatusCode::FORBIDDEN,
                web::error_page(
                    self.gw.title(),
                    "账号已不存在",
                    "你的账号已经被删除，会话会在过期后失效。<a href=\"/__auth/logout\">退出</a>",
                ),
            );
        };
        let mut banners = self.flash.banners();
        if let Some(e) = error {
            banners.insert(0, e);
        }
        banners.extend(self.store_banners());
        if !self.gw.store.writable() {
            banners.push(Banner::warn(
                "账号库是只读的，这里改不了口令。请让 `users_file` 指向可写路径。",
            ));
        }
        let view = MeView {
            title: self.gw.title().to_string(),
            actor: self.actor.clone(),
            is_admin: self.admin,
            name: acct.user.name.clone(),
            admin: acct.user.admin,
            routes_label: routes_label(&acct.user),
            source_label: acct.source.label().to_string(),
            writable: self.gw.store.writable(),
            banners,
        };
        page(StatusCode::OK, web::me_page(&view))
    }

    async fn me_change_password(&self, req: HyperRequest<Incoming>) -> HyperResponse<ProxyBody> {
        let Some(acct) = self.account(&self.actor) else {
            return self.me_page(Some(Banner::err("你的账号已经被删除")));
        };
        let form = match read_form(req).await {
            Ok(f) => f,
            Err(e) => return text(StatusCode::BAD_REQUEST, e),
        };

        // 限速。没有这一条，一个拿到 cookie 的人就能在这里无限次试「当前口令」，
        // 登录页那边的限速形同虚设。
        if let Some(wait) = self.gw.auth.blocked_for(self.source) {
            let secs = wait.as_secs().max(1);
            return self.me_page(Some(Banner::err(format!(
                "尝试过于频繁，请 {secs} 秒后再试"
            ))));
        }

        let old = form.get("old").unwrap_or("");
        if !crate::config::verify_password(old, &acct.user.hash) {
            self.gw.auth.record_failure(self.source);
            tracing::warn!(user = %self.actor, source = ?self.source, "self password change: wrong current password");
            return self.me_page(Some(Banner::err("当前口令不正确")));
        }

        let pass = form.get("pass").unwrap_or("");
        let pass2 = form.get("pass2").unwrap_or("");
        if pass.chars().count() < MIN_PASSWORD_LEN {
            return self.me_page(Some(Banner::err(format!(
                "新口令至少 {MIN_PASSWORD_LEN} 个字符"
            ))));
        }
        if pass != pass2 {
            return self.me_page(Some(Banner::err("两次输入的新口令不一致")));
        }

        // 来自配置文件的账号：先接管，改口令才有地方落。哈希原样复制，
        // 所以「接管 + 改口令」这个顺序不会出现口令短暂失效的窗口。
        if !acct.source.editable() {
            if let Err(e) = self.gw.store.adopt(&self.actor, &acct.user) {
                return self.me_page(Some(Banner::err(e)));
            }
            tracing::info!(
                actor = %self.actor,
                "config account adopted into the store (self-service password change)"
            );
        }
        if let Err(e) = self.gw.store.set_password(&self.actor, &self.actor, pass) {
            return self.me_page(Some(Banner::err(e)));
        }

        self.gw.auth.clear_source(self.source);
        let killed = self
            .gw
            .auth
            .revoke_user(&self.actor, Some(self.sid.as_str()));
        tracing::info!(
            actor = %self.actor, source = ?self.source,
            sessions_revoked = killed, "password changed"
        );
        let extra = if acct.source.editable() {
            String::new()
        } else {
            "账号已接管进账号库（gateway.toml 里那条现在被覆盖，建议删掉）。".to_string()
        };
        let killed_txt = if killed > 0 {
            format!("其他 {killed} 个会话已失效。")
        } else {
            String::new()
        };
        self.done(
            true,
            &format!("{PREFIX}/me"),
            &format!("口令已修改。{extra}{killed_txt}"),
        )
    }

    // ── 小工具 ──

    fn account(&self, name: &str) -> Option<Account> {
        self.gw.accounts().into_iter().find(|a| a.user.name == name)
    }

    fn not_found_user(&self, name: &str) -> HyperResponse<ProxyBody> {
        self.done(false, &format!("{PREFIX}/"), &format!("账号 {name} 不存在"))
    }

    /// 302 回某页，把提示挂到会话上（下个 GET 取走一次）。
    ///
    /// `ok` 显式给，不靠文案猜。「已保存」和「不能保存」都能被一条
    /// `contains("已")` 判错，而错误提示显示成绿色勾是最糟的反馈。
    fn done(&self, ok: bool, to: &str, msg: &str) -> HyperResponse<ProxyBody> {
        self.gw.auth.set_flash(&self.sid, ok, msg);
        redirect(StatusCode::FOUND, to)
    }

    fn render_form(&self, st: FormState) -> HyperResponse<ProxyBody> {
        let view = UserFormView {
            title: self.gw.title().to_string(),
            actor: self.actor.clone(),
            is_admin: self.admin,
            name: st.name,
            is_new: st.is_new,
            name_locked: st.name_locked,
            admin: st.admin,
            routes: st.routes,
            available: self.gw.cfg.route_names(),
            writable: self.gw.store.writable(),
            banners: st.banners,
        };
        page(StatusCode::OK, web::user_form_page(&view))
    }
}

struct FormState {
    name: String,
    is_new: bool,
    name_locked: bool,
    admin: bool,
    routes: Vec<String>,
    banners: Vec<Banner>,
}

// ─────────────────────────────────────────────────────────────────────────────
// 表单
// ─────────────────────────────────────────────────────────────────────────────

/// 提交上来的账号表单。
struct UserInput {
    name: String,
    pass: String,
    pass2: String,
    all_routes: bool,
    routes: Vec<String>,
    admin: bool,
}

impl UserInput {
    fn from_form(f: &Form) -> Self {
        Self {
            name: f.get("name").unwrap_or("").trim().to_string(),
            pass: f.get("pass").unwrap_or("").to_string(),
            pass2: f.get("pass2").unwrap_or("").to_string(),
            all_routes: f.flag("all_routes"),
            // 多个同名 `route` 字段：这就是为什么不能复用 parse_form
            routes: f.values("route"),
            admin: f.flag("admin"),
        }
    }

    /// 已选项（含 `*`），用于在校验失败时把表单恢复原样。
    fn selected_routes(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.all_routes {
            v.push("*".to_string());
        }
        v.extend(self.routes.iter().filter(|r| *r != "*").cloned());
        v
    }

    /// 校验并返回最终要落库的 routes。
    fn validate(
        &self,
        gw: &Gateway,
        is_new: bool,
        existing: Option<&str>,
    ) -> Result<Vec<String>, String> {
        if is_new {
            if !valid_user_name(&self.name) {
                return Err("账号名不合法：只能用字母、数字、. - _，长度 1..=64".to_string());
            }
            if gw.user(&self.name).is_some() {
                return Err(format!(
                    "账号 {} 已经存在（可能在 gateway.toml 里）",
                    self.name
                ));
            }
        } else {
            // 编辑时账号名来自 URL（输入框是锁死的，表单里不再提交它），
            // 所以空名字是正常的。非空且不一致 = 有人手工构造了请求。
            let target = existing.unwrap_or("");
            if !self.name.is_empty() && self.name != target {
                return Err("账号名不能在这里修改".to_string());
            }
        }

        // 口令：新建必填；编辑时留空 = 不改
        let changing = is_new || !self.pass.is_empty();
        if changing {
            if self.pass.chars().count() < MIN_PASSWORD_LEN {
                return Err(format!("口令至少 {MIN_PASSWORD_LEN} 个字符"));
            }
            if self.pass != self.pass2 {
                return Err("两次输入的口令不一致".to_string());
            }
        }

        // 授权：`*` 优先；否则必须至少选一个且都真实存在
        if self.all_routes {
            return Ok(vec!["*".to_string()]);
        }
        if self.routes.is_empty() {
            return Err("至少要勾一个端口（或勾「全部端口」），否则这个账号什么也访问不了".into());
        }
        let known = gw.cfg.route_names();
        let mut picked = Vec::new();
        for r in &self.routes {
            if r == "*" {
                continue;
            }
            if !known.iter().any(|k| k == r) {
                return Err(format!(
                    "没有名为 {r} 的端口。可选的：{}",
                    if known.is_empty() {
                        "（配置里一个 route 都没有）".to_string()
                    } else {
                        known.join(", ")
                    }
                ));
            }
            if !picked.contains(r) {
                picked.push(r.clone());
            }
        }
        if picked.is_empty() {
            return Err("至少勾一个端口".into());
        }
        Ok(picked)
    }
}

/// 表单字段（保持多值与顺序，`HashMap` 会把重复的 `route` 吃掉）。
struct Form(Vec<(String, String)>);

impl Form {
    fn parse(body: &str) -> Self {
        let mut v = Vec::new();
        for pair in body.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, val) = pair.split_once('=').unwrap_or((pair, ""));
            v.push((crate::proxy::urldecode(k), crate::proxy::urldecode(val)));
        }
        Self(v)
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn values(&self, key: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some(v) if !v.is_empty() && v != "0" && v != "false")
    }
}

/// 读表单体。错误只回一条文案，由调用方决定怎么渲染——把整个 `Response`
/// 塞进 `Err` 会让每条 `Result` 都被撑大好几百字节。
async fn read_form(req: HyperRequest<Incoming>) -> Result<Form, &'static str> {
    let body = Limited::new(req.into_body(), MAX_FORM_BYTES)
        .collect()
        .await
        .map_err(|_| "请求体过大或读取失败")?
        .to_bytes();
    let s = std::str::from_utf8(&body).map_err(|_| "请求体不是合法 UTF-8")?;
    Ok(Form::parse(s))
}

// ─────────────────────────────────────────────────────────────────────────────
// 渲染与路径小工具
// ─────────────────────────────────────────────────────────────────────────────

/// `/__admin/users/alice/delete` → `["users", "alice", "delete"]`
fn segments(path: &str) -> Vec<&str> {
    path.trim_start_matches(PREFIX)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect()
}

/// 面板页面的统一出口：`no-store`（别让浏览器缓存账号列表）+ 禁止被 iframe 套。
fn page(status: StatusCode, body: String) -> HyperResponse<ProxyBody> {
    let mut resp = html(status, body);
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut()
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    resp.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn query_suffix(req: &HyperRequest<Incoming>) -> String {
    match req.uri().query() {
        Some(q) => format!("?{q}"),
        None => String::new(),
    }
}

/// 授权范围的可读形式。
fn routes_label(u: &crate::config::UserRuntime) -> String {
    if u.all_routes {
        return "全部端口".into();
    }
    if u.routes.is_empty() {
        return "—".into();
    }
    u.routes.join(", ")
}

/// 落库用的 routes（含 `*`）。
fn raw_routes(u: &crate::config::UserRuntime) -> Vec<String> {
    let mut v = Vec::new();
    if u.all_routes {
        v.push("*".to_string());
    }
    v.extend(u.routes.iter().cloned());
    v
}

fn store_line(gw: &Gateway) -> String {
    let store = &gw.store;
    let path = store
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "（未配置 users_file）".into());
    let state = if store.writable() {
        format!("可写，{} 个账号", store.count())
    } else {
        "只读".to_string()
    };
    format!("{path} —— {state}。同名账号以账号库为准，配置文件里的那条会被覆盖。")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;
    use std::sync::Arc;

    fn gateway() -> Gateway {
        let raw = r#"
[[user]]
name = "admin"
hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$11111111111111111111111111111111"
routes = ["*"]
admin = true

[[user]]
name = "viewer"
hash = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$11111111111111111111111111111111"
routes = ["panel"]

[[route]]
name = "panel"
listen = "0.0.0.0:2223"
upstream = "127.0.0.1:18787"

[[route]]
name = "secrets"
listen = "0.0.0.0:2224"
upstream = "127.0.0.1:18788"
"#;
        let cfg: GatewayConfig = toml::from_str(raw).unwrap();
        Gateway::new(Arc::new(cfg.build().unwrap()))
    }

    fn input(name: &str, pass: &str, pass2: &str, routes: &[&str]) -> UserInput {
        UserInput {
            name: name.into(),
            pass: pass.into(),
            pass2: pass2.into(),
            all_routes: false,
            routes: routes.iter().map(|r| r.to_string()).collect(),
            admin: false,
        }
    }

    #[test]
    fn segments_splits_the_admin_prefix() {
        assert!(segments("/__admin/").is_empty());
        assert_eq!(segments("/__admin/users"), ["users"]);
        assert_eq!(segments("/__admin/users/alice"), ["users", "alice"]);
        assert_eq!(
            segments("/__admin/users/alice/delete"),
            ["users", "alice", "delete"]
        );
        assert_eq!(
            segments("/__admin/sessions/abc/revoke"),
            ["sessions", "abc", "revoke"]
        );
    }

    #[test]
    fn form_keeps_every_route_value() {
        // HashMap 会把重复的 `route` 吃掉，这正是不能复用 proxy::parse_form 的原因
        let f = Form::parse("name=bob&route=panel&route=secrets&admin=1");
        assert_eq!(f.values("route"), vec!["panel", "secrets"]);
        assert_eq!(f.get("name"), Some("bob"));
        assert!(f.flag("admin"));
        assert!(!f.flag("all_routes"));
    }

    #[test]
    fn form_percent_decodes_and_handles_missing_equals() {
        let f = Form::parse("name=a%2Bb&note&empty=");
        assert_eq!(f.get("name"), Some("a+b"));
        assert_eq!(f.get("note"), Some(""));
        assert_eq!(f.get("empty"), Some(""));
    }

    #[test]
    fn flag_treats_absent_and_false_values_as_off() {
        for body in ["x=1", "admin=0", "admin=false", "admin="] {
            let f = Form::parse(body);
            assert!(!f.flag("admin"), "{body} 不应被当成勾选");
        }
        assert!(Form::parse("admin=1").flag("admin"));
        assert!(Form::parse("admin=on").flag("admin"));
    }

    #[test]
    fn validate_accepts_a_normal_account() {
        let gw = gateway();
        let i = input("bob", "bob-pass-123", "bob-pass-123", &["panel"]);
        let routes = i.validate(&gw, true, None).unwrap();
        assert_eq!(routes, vec!["panel"]);
    }

    #[test]
    fn validate_all_routes_collapses_to_star() {
        let gw = gateway();
        let mut i = input("bob", "bob-pass-123", "bob-pass-123", &["panel"]);
        i.all_routes = true;
        // 勾了「全部端口」时，具体勾选项无关紧要——落库只留 `*`，
        // 这样以后新增的端口也自动包含。
        assert_eq!(i.validate(&gw, true, None).unwrap(), vec!["*"]);
    }

    #[test]
    fn validate_rejects_unknown_route_and_lists_the_valid_ones() {
        let gw = gateway();
        let i = input("bob", "bob-pass-123", "bob-pass-123", &["nope"]);
        let e = i.validate(&gw, true, None).unwrap_err();
        assert!(e.contains("没有名为 nope 的端口"), "{e}");
        assert!(
            e.contains("panel") && e.contains("secrets"),
            "应列出可选值: {e}"
        );
    }

    #[test]
    fn validate_rejects_no_route_at_all() {
        let gw = gateway();
        let i = input("bob", "bob-pass-123", "bob-pass-123", &[]);
        let e = i.validate(&gw, true, None).unwrap_err();
        assert!(e.contains("至少"), "{e}");
    }

    #[test]
    fn validate_rejects_short_password_and_mismatch() {
        let gw = gateway();
        let short = input("bob", "short", "short", &["panel"]);
        assert!(short
            .validate(&gw, true, None)
            .unwrap_err()
            .contains("至少"));

        let mismatch = input("bob", "bob-pass-123", "bob-pass-124", &["panel"]);
        assert!(mismatch
            .validate(&gw, true, None)
            .unwrap_err()
            .contains("不一致"));
    }

    #[test]
    fn validate_rejects_duplicate_and_malformed_names() {
        let gw = gateway();
        let dup = input("admin", "bob-pass-123", "bob-pass-123", &["panel"]);
        let e = dup.validate(&gw, true, None).unwrap_err();
        assert!(e.contains("已经存在"), "{e}");

        for bad in ["", "a b", "a/b"] {
            let i = input(bad, "bob-pass-123", "bob-pass-123", &["panel"]);
            assert!(i.validate(&gw, true, None).is_err(), "{bad:?} 应被拒绝");
        }
    }

    #[test]
    fn edit_allows_empty_password_meaning_keep_it() {
        let gw = gateway();
        let mut i = input("bob", "", "", &["panel"]);
        i.name = "bob".into();
        assert!(
            i.validate(&gw, false, Some("bob")).is_ok(),
            "编辑时留空 = 不改口令"
        );

        // 但空口令在新建时是非法的
        assert!(i.validate(&gw, true, None).is_err());
    }

    #[test]
    fn edit_takes_the_name_from_the_url_not_the_form() {
        let gw = gateway();

        // 真实表单：输入框锁死，`name` 字段根本不提交 —— 必须放行
        let blank = input("", "", "", &["panel"]);
        assert!(blank.validate(&gw, false, Some("bob")).is_ok());

        // 手工构造的改包名请求要挡住
        let forged = input("carol", "", "", &["panel"]);
        let e = forged.validate(&gw, false, Some("bob")).unwrap_err();
        assert!(e.contains("不能在这里修改"), "{e}");
    }

    #[test]
    fn selected_routes_restores_the_form_after_a_validation_error() {
        let mut i = input("bob", "x", "x", &["panel", "secrets"]);
        i.all_routes = true;
        // 勾了「全部」时页面上只回显 `*`
        assert_eq!(i.selected_routes(), vec!["*", "panel", "secrets"]);
    }

    #[test]
    fn routes_label_is_human_readable() {
        let gw = gateway();
        let admin = gw.user("admin").unwrap();
        assert_eq!(routes_label(&admin), "全部端口");
        assert_eq!(raw_routes(&admin), vec!["*".to_string()]);

        let viewer = gw.user("viewer").unwrap();
        assert_eq!(routes_label(&viewer), "panel");
        assert_eq!(raw_routes(&viewer), vec!["panel".to_string()]);
    }

    #[test]
    fn store_line_explains_read_only_mode() {
        // users_file = None（测试里直接 build，没走 load）→ 面板只读
        let gw = gateway();
        let line = store_line(&gw);
        assert!(line.contains("只读"), "{line}");
        assert!(line.contains("未配置"), "{line}");
    }
}
