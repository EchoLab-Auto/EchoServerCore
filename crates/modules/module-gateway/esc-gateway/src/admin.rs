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

use std::net::{IpAddr, SocketAddr};

use axum::http::{header, HeaderValue, Method, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::{Request as HyperRequest, Response as HyperResponse};

use crate::auth::{self, Denied};
use crate::config::valid_user_name;
use crate::proxy::{json_body, json_error, redirect, text, urlencode, Account, Gateway, ProxyBody};
use crate::store::MIN_PASSWORD_LEN;
use serde::Serialize;
use serde_json::json;

use crate::web::{
    self, Banner, MeView, SessionRow, SessionsView, UserFormView, UserRow, UsersView,
};

/// 面板端点的公共前缀。单独提出来是因为它同时出现在 URL 匹配、登录跳转和
/// 文档里，改一处漏一处就会出现「点了跳到 404」。
const PREFIX: &str = "/__admin";

/// 表单体上限。账号表单最多几百字节，8 KB 已经非常宽松。
const MAX_FORM_BYTES: usize = 8 * 1024;

pub async fn handle(
    gw: &std::sync::Arc<Gateway>,
    source: Option<IpAddr>,
    local: SocketAddr,
    req: HyperRequest<Incoming>,
) -> HyperResponse<ProxyBody> {
    let path = req.uri().path().to_string();

    // ① 来源白名单。放在最前面：不在名单里就连「需要登录」都不告诉他。
    if !gw.cfg.admin_source_allowed(source) {
        tracing::warn!(source = ?source, "admin panel source not allowed");
        return json_err(
            StatusCode::FORBIDDEN,
            "当前来源地址不在 admin_allowed_cidrs 允许范围内",
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
    //
    //    `api` 是这些端点的**公共前缀**（见下），不是端点名：判权限要看它后面
    //    那几段。拿带 `api` 的段去比 `[] | ["me"]` 永远不匹配，结果就是普通
    //    用户连自己的改口令页面都进不去。
    // 剥掉 `api` 段再判：权限闸在分发**之前**执行，而分发时用的是剥过 `api`
    // 的那份 segs。两边口径不一致的话，非管理员走 `/__admin/api/me` 会拿着
    // `["api","me"]` 去比 `[] | ["me"]` → 永远不匹配 → 403。
    //
    // `["me", "password"]` 也算自助：改自己的口令不是管理操作。
    let tail: &[&str] = match segs.as_slice() {
        ["api", rest @ ..] => rest,
        other => other,
    };
    let is_self_only = matches!(tail, [] | ["me"] | ["me", "password"]);
    if !session.admin && !is_self_only {
        tracing::warn!(user = %session.user, path = %path, "non-admin tried to open the admin panel");
        return json_err(
            StatusCode::FORBIDDEN,
            &format!("账号 {} 不是管理员", session.user),
        );
    }

    let is_post = req.method() == Method::POST;

    // ④ 所有变更都是 POST，且必须过 Origin 校验（纵深防御，主防线是 SameSite）
    if is_post && !auth::same_origin(req.headers()) {
        tracing::warn!(user = %session.user, source = ?source, path = %path, "cross-origin admin write rejected");
        return json_err(
              StatusCode::FORBIDDEN,
              "跨站请求被拒绝：这个请求不是从本网关的页面发出的",
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
        local,
        host: req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
        flash,
    };

    // 全部管理端点都在 `/__admin/api/` 下：这个前缀以外的请求一律 404，
    // 免得有人把 `/__admin/users` 当成"另一个入口"来试。
    //
    // 为什么用 `/__admin/api/`（而不是裸 `/__admin/users`）：
    // 上游面板未来若长出同名路径，前缀能保证两边永不撞车；而且一眼能看出
    // 这是给前端调的、不是给人点的。
    let Some(rest) = segs.split_first() else {
        return json_err(
            StatusCode::NOT_FOUND,
            "管理接口都在 /__admin/api/ 下",
        );
    };
    if rest.0 != &"api" {
        return json_err(
            StatusCode::NOT_FOUND,
            "管理接口都在 /__admin/api/ 下",
        );
    }
    let segs = rest.1;

    match (is_post, segs) {
        // 我是谁 + 我能去哪些端口（前端壳子启动时拉一次）
        (false, ["overview"]) => ctx.overview(),

        // 账号
        (false, ["users"]) => ctx.users_page(),
        (true, ["users"]) => ctx.user_create(req).await,
        (false, ["users", name]) => ctx.user_form_edit(name),
        (true, ["users", name]) => ctx.user_update(name, req).await,
        (true, ["users", name, "delete"]) => ctx.user_delete(name),
        (true, ["users", name, "adopt"]) => ctx.user_adopt(name),
        (true, ["users", name, "revoke"]) => ctx.user_revoke_sessions(name),

        // 隔离网段：谁在这个网段里、地址能不能用、TLS 该指向哪
        (false, ["segments"]) => ctx.segments_page(),

        // 代理（端口 → 上游）
        (false, ["routes"]) => ctx.routes_page(),
        (true, ["routes"]) => ctx.route_create(req).await,
        (false, ["routes", "new"]) => ctx.route_form_new(),
        (false, ["routes", name]) => ctx.route_form_edit(name),
        (true, ["routes", name]) => ctx.route_update(name, req).await,
        (true, ["routes", name, "delete"]) => ctx.route_delete(name).await,
        (true, ["routes", name, "disable"]) => ctx.route_disable(name).await,
        (true, ["routes", name, "enable"]) => ctx.route_enable(name).await,

        // 会话
        (false, ["sessions"]) => ctx.sessions_page(),
        (true, ["sessions", id, "revoke"]) => ctx.session_revoke(id),

        // 我的账号
        (false, ["me"]) => ctx.me_page(None),
        (true, ["me", "password"]) => ctx.me_change_password(req).await,

        _ => json_err(StatusCode::NOT_FOUND, "没有这个管理端点"),
    }
}

/// 一次请求的上下文，省得每个处理函数都拖一长串参数。
struct Ctx<'a> {
    gw: &'a std::sync::Arc<Gateway>,
    actor: String,
    admin: bool,
    sid: String,
    source: Option<IpAddr>,
    /// 这个请求是从哪个监听端口进来的。删路由时要靠它认出"我自己"。
    local: SocketAddr,
    /// 这个请求的 `Host`（归一化前）。同一个端口可能挂多条按 Host 分流的路由，
    /// 光凭 `local` 认不出"走的是哪一条"——列表里那个「当前」标记要靠它。
    host: Option<String>,
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
    /// 壳子启动时拉一次：我是谁、我有权去哪些端口、当前从哪个端口进来。
    ///
    /// 单独一个端点而不是塞进 `/users`：普通用户（非管理员）也要能拿到它，
    /// 而 `/users` 对他们是 403。
    fn overview(&self) -> HyperResponse<ProxyBody> {
        let routes = self
            .gw
            .effective_routes()
            .into_iter()
            .filter(|r| self.admin || self.can_reach(&r.spec.name))
            .map(|r| {
                json!({
                    "name": r.spec.name,
                    "listen": r.spec.listen,
                    "segment": r.spec.segment,
                    "disabled": r.disabled,
                    "listening": r.disabled == false,
                })
            })
            .collect::<Vec<_>>();
        json_with(
            StatusCode::OK,
            json!({
                "title": self.gw.title(),
                "user": self.actor,
                "admin": self.admin,
                "routes": routes,
                "//": "当前请求从哪个监听端口进来——删端口时要靠它认出「我自己」",
                "current_addr": self.local.to_string(),
                "admin_port": self.gw.cfg.admin_listen.map(|a| a.to_string()),
                "store_writable": self.gw.store.writable(),
                "routes_writable": self.gw.rt_store.writable(),
            }),
        )
    }

    /// 这个账号能不能去某个端口。
    ///
    /// 管理员不看 ACL（和网关转发时的判断保持一致：`Session::can_access`）。
    fn can_reach(&self, route: &str) -> bool {
        self.gw
            .auth
            .list()
            .into_iter()
            .find(|(sid, _)| sid == &self.sid)
            .map(|(_, s)| s.can_access(route))
            .unwrap_or(false)
    }

    /// 隔离网段的状态：声明了什么、里面有哪些路由、地址在本机能不能用。
    ///
    /// 只读端点。网段是**拓扑承诺**（listen 只能在网段里、上游只能是回环），
    /// 改它必须重启——这种改动不该有一个"点一下就生效"的入口。
    fn segments_page(&self) -> HyperResponse<ProxyBody> {
        let gw = self.gw;
        let routes = gw.effective_routes();
        let rows: Vec<serde_json::Value> = gw
            .cfg
            .segments
            .all()
            .iter()
            .map(|seg| {
                let mine: Vec<serde_json::Value> = routes
                    .iter()
                    .filter(|r| r.spec.segment.as_deref() == Some(seg.name.as_str()))
                    .map(|r| {
                        json!({
                            "name": r.spec.name,
                            "listen": r.spec.listen,
                            "upstream": r.spec.upstream,
                            "disabled": r.disabled,
                        })
                    })
                    .collect();
                json!({
                    "name": seg.name,
                    "cidr": seg.cidr(),
                    "kind": seg.kind.label(),
                    "note": seg.note,
                    // 探一下地址还在不在：容器重启过、设备被删掉，
                    // 这里会立刻从 ready 变成 false。
                    "probe_addr": seg.probe_addr().to_string(),
                    "ready": seg.probe().is_ok(),
                    "routes": mine,
                })
            })
            .collect();
        json_with(
            StatusCode::OK,
            json!({
                "segments": rows,
                "//": "网段地址只在本机可达。TLS 终结的上游要指向上面的 listen；\
                       从公网直连这些地址不可能成功——这正是它存在的意义。",
            }),
        )
    }

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
        json_ok(&view)
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

    /// 新建表单的空白初值（`GET /__admin/api/users/new` 用不到——前端自己就能
    /// 画一个空表单；留着是因为创建失败时要回填同一份结构，走 `render_form`）。
    #[allow(dead_code)]
    fn user_form_new(&self) -> HyperResponse<ProxyBody> {
        self.user_form_ok(FormState {
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
        self.user_form_ok(FormState {
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

    // ── 代理（端口 → 上游） ─────────────────────────────────────────────

    fn routes_page(&self) -> HyperResponse<ProxyBody> {
        let gw = self.gw;
        let mut banners = self.flash.banners();
        banners.extend(self.route_store_banners());

        let rows: Vec<web::RouteRow> = gw
            .effective_routes()
            .into_iter()
            .map(|e| {
                let port = e.spec.listen.rsplit(':').next().unwrap_or("").to_string();
                // 「当前」= 既在这个端口上，又是**这次请求真正走的那一条**。
                // 只看端口不够了：一个端口现在可能挂多条按 Host 分流的路由，
                // 那样每一条都会自称"当前"，而下面那些「删它会把连接断掉」的
                // 提示就全错了。
                let here = e.spec.listen.parse::<SocketAddr>().ok() == Some(self.local)
                    && e.spec
                        .host
                        .as_deref()
                        .map(|want| {
                            let got = self
                                .host
                                .as_deref()
                                .map(|h| h.to_ascii_lowercase())
                                .unwrap_or_default();
                            let got = got.split(':').next().unwrap_or("").to_string();
                            got == want
                        })
                        // 兜底那条：只要这次请求没被别的路由精确命中，走的就是它。
                        // 判断「有没有被精确命中」要重查一次表，成本不值当 —— 直接把
                        // 「这个端口上有没有别的精确 host 路由」当作条件。
                        .unwrap_or_else(|| {
                            let cur = self
                                .host
                                .as_deref()
                                .map(|h| h.to_ascii_lowercase())
                                .unwrap_or_default();
                            let cur = cur.split(':').next().unwrap_or("").to_string();
                            !gw.routes
                                .hosts_on(e.spec.listen.parse().unwrap_or(self.local))
                                .iter()
                                .flatten()
                                .any(|h| *h == cur)
                        });
                web::RouteRow {
                    name: e.spec.name.clone(),
                    host: e.spec.host.clone(),
                    source_label: e.source.label().to_string(),
                    disabled: e.disabled,
                    // 只有路由库拥有、且配置文件里没有同名条目的才能真删除
                    deletable: (e.source == crate::routes::RouteSource::Store
                        || e.source == crate::routes::RouteSource::StoreDisabled)
                        && !gw.cfg.routes.contains_key(&e.spec.name),
                    listen: e.spec.listen.clone(),
                    upstream: e.spec.upstream.clone(),
                    detail: route_detail(&e.spec),
                    live: gw.routes.get(&e.spec.name).is_some(),
                    here,
                    port,
                }
            })
            .collect();

        if rows.len() <= 1 {
            banners.push(Banner::warn(
                "现在只有 1 个端口在转发。把最后一条也删掉，就连这个管理面板也进不来了——\
                 删之前先加好新的。",
            ));
        }

        let view = web::RoutesView {
            title: gw.title().to_string(),
            actor: self.actor.clone(),
            is_admin: true,
            rows,
            store_line: route_store_line(gw),
            banners,
            current_port: self.local.port(),
            dedicated_admin: gw.cfg.admin_listen.is_some(),
        };
        json_ok(&view)
    }

    fn route_store_banners(&self) -> Vec<Banner> {
        let store = &self.gw.rt_store;
        let mut v = Vec::new();
        if let Some(note) = store.note() {
            v.push(Banner::warn(note));
        }
        for r in store.rejected() {
            v.push(Banner::err(format!("路由库里有条目被拒绝（已跳过）：{r}")));
        }
        for e in self.gw.effective_routes() {
            if e.source == crate::routes::RouteSource::StoreOverConfig {
                v.push(Banner::warn(format!(
                    "端口 `{}` 在 gateway.toml 里也有一条（只读文件），当前以路由库为准。",
                    e.spec.name
                )));
            }
        }
        // 名字存在于账号 ACL 里、但端口已经不在了——这类"悬空引用"会让那个
        // 账号以为自己在用某个面板，其实已经没了，早点说出来。
        //
        // 已下架的端口也算"不在了"：它不监听，授权给它的人访问不到任何东西。
        // 区分"彻底不存在"和"被下架"没意义，反正都进不去。
        let names = self.gw.route_names();
        for a in self.gw.accounts() {
            let dangling: Vec<&String> = a
                .user
                .routes
                .iter()
                .filter(|r| **r != "*" && !names.iter().any(|n| n == *r))
                .collect();
            if !dangling.is_empty() {
                v.push(Banner::warn(format!(
                    "账号 `{}` 被授权访问已不存在的端口：{}",
                    a.user.name,
                    dangling
                        .iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>()
                        .join("、")
                )));
            }
        }
        v
    }

    fn route_form_new(&self) -> HyperResponse<ProxyBody> {
        self.route_form_ok(RouteFormState {
            is_new: true,
            name_locked: false,
            banners: self.flash.banners(),
            ..Default::default()
        })
    }

    fn route_form_edit(&self, name: &str) -> HyperResponse<ProxyBody> {
        let Some(e) = self.route(name) else {
            return self.not_found_route(name);
        };
        let mut banners = self.flash.banners();
        if e.source == crate::routes::RouteSource::Config {
            banners.push(Banner::warn(format!(
                "端口 {name} 现在定义在 gateway.toml 里（只读文件）。\
                 保存时会自动在路由库生成一条同名条目接管它——不用多点一步。"
            )));
        }
        self.route_form_ok(RouteFormState {
            is_new: false,
            name_locked: true,
            name: e.spec.name.clone(),
            listen: e.spec.listen.clone(),
            upstream: e.spec.upstream.clone(),
            host: e.spec.host.clone().unwrap_or_default(),
            segment: e.spec.segment.clone().unwrap_or_default(),
            strip_prefix: e.spec.strip_prefix.clone().unwrap_or_default(),
            max_body_bytes: e
                .spec
                .max_body_bytes
                .map(|n| n.to_string())
                .unwrap_or_default(),
            allowed_cidrs: e.spec.allowed_cidrs.join("\n"),
            banners,
        })
    }

    async fn route_create(&self, req: HyperRequest<Incoming>) -> HyperResponse<ProxyBody> {
        let form = match read_form(req).await {
            Ok(f) => f,
            Err(e) => return text(StatusCode::BAD_REQUEST, e),
        };
        let input = RouteInput::from_form(&form);
        let spec = match input.validate(self.gw, true, None) {
            Ok(s) => s,
            Err(e) => return self.render_route_form(input.to_state(true, false, e)),
        };
        let known = self.gw.route_names();
        let (pending, action) = match self.gw.rt_store.stage(spec.clone(), true, &known) {
            Ok(v) => v,
            Err(e) => return self.render_route_form(input.to_state(true, false, e)),
        };
        self.apply_route_change(spec, pending, action, &format!("{PREFIX}/routes"), true)
            .await
    }

    async fn route_update(
        &self,
        name: &str,
        req: HyperRequest<Incoming>,
    ) -> HyperResponse<ProxyBody> {
        let Some(e) = self.route(name) else {
            return self.not_found_route(name);
        };
        let from_config = e.source == crate::routes::RouteSource::Config;
        let form = match read_form(req).await {
            Ok(f) => f,
            Err(e) => return text(StatusCode::BAD_REQUEST, e),
        };
        let input = RouteInput::from_form(&form);
        // 端口名来自 URL，表单里不提交，所以这里用它兜底
        let spec = match input.validate(self.gw, false, Some(name)) {
            Ok(s) => s,
            Err(e) => return self.render_route_form(input.to_state(false, true, e)),
        };
        let known = self.gw.route_names();
        let (pending, action) = match self.gw.rt_store.stage(spec.clone(), false, &known) {
            Ok(v) => v,
            Err(e) => return self.render_route_form(input.to_state(false, true, e)),
        };
        let extra = if from_config {
            "（它在 gateway.toml 里那条现在被覆盖了，面板从此管它）".to_string()
        } else {
            String::new()
        };
        self.apply_route_change(
            spec,
            pending,
            format!("{action}{extra}"),
            &format!("{PREFIX}/routes"),
            false,
        )
        .await
    }

    /// **真删除**：从路由库抹掉，不留墓碑。
    ///
    /// 和下架的分工很明确：
    /// * 下架 = 别再听了，但定义留着（可能还要恢复，或者它来自只读的配置文件）
    /// * 删除 = 我不再需要这个端口了
    ///
    /// 只有"路由库自己拥有"的条目能被删除。两种情况必须拒绝，因为它们的
    /// **结果和用户想要的正相反**：
    ///
    /// 1. `gateway.toml` 里也有同名条目 → 删掉库里那条，配置那条立刻复活，
    ///    端口又开起来了。用户以为删了，其实什么都没变（甚至更糟：定义换了一份）。
    /// 2. 条目根本不在库里（只在配置文件里）→ 无从删起。
    async fn route_delete(&self, name: &str) -> HyperResponse<ProxyBody> {
        let back = format!("{PREFIX}/routes");
        let Some(e) = self.route(name) else {
            return self.not_found_route(name);
        };
        // 自己脚下的端口：先拦，错误信息最具体
        if e.spec.listen.parse::<SocketAddr>().ok() == Some(self.local) {
            return self.done(
                false,
                &back,
                &format!(
                    "端口 {name} 就是你现在访问管理面板用的这个（{}）。\
                     删它会把当前连接一起断掉——请从别的端口（或独立管理端口）进来删。",
                    self.local
                ),
            );
        }
        // 配置里也有同名的？删不得（会复活），指向「下架」
        if self.gw.cfg.routes.contains_key(name) {
            return self.done(
                false,
                &back,
                &format!(
                    "端口 {name} 在 gateway.toml 里也有一条（那是只读文件，面板改不了）。\
                     删掉路由库这条只会让它按配置文件里的定义复活，端口照样在听。\
                     想让它停掉请用「下架」；想彻底删掉请编辑 /etc/esc/gateway.toml 里那条。"
                ),
            );
        }
        // 库里没有它（只在配置文件里）→ 无从删起
        let in_store = self.gw.rt_store.get(name).is_some()
            || self
                .gw
                .rt_store
                .all_disabled()
                .iter()
                .any(|r| r.name == name);
        if !in_store {
            return self.done(
                false,
                &back,
                &format!(
                    "端口 {name} 只定义在 gateway.toml 里（只读文件），面板删不掉。\
                     要用「下架」让它停止监听，或者直接编辑那个文件。"
                ),
            );
        }

        // 删到零个业务端口：管理面板挂在业务端口上时会把自己关在外面
        let enabled = self
            .gw
            .effective_routes()
            .iter()
            .filter(|r| !r.disabled)
            .count();
        if !e.disabled && enabled <= 1 && self.gw.cfg.admin_listen.is_none() {
            return self.done(
                false,
                &back,
                "这是最后一个生效的端口，删掉之后连管理面板也进不来了（它就挂在这个端口上）。\
                 先加一个新的，或者配 admin_listen 让管理面板独占一个端口。",
            );
        }

        let (pending, action) = match self.gw.rt_store.stage_delete(name) {
            Ok(v) => v,
            Err(e) => return self.done(false, &back, &e),
        };
        let runtimes = match self.gw.candidate_runtimes(&pending) {
            Ok(r) => r,
            Err(e) => return self.done(false, &back, &format!("删除后配置不合法：{e}")),
        };
        let prepared = match self.gw.listeners.prepare(&runtimes).await {
            Ok(p) => p,
            Err(e) => return self.done(false, &back, &e),
        };
        if let Err(e) = self.gw.rt_store.commit(&self.actor, pending, &action) {
            return self.done(false, &back, &e);
        }
        self.gw.commit_routes(runtimes, prepared);
        tracing::info!(actor = %self.actor, target = %name, source = ?self.source, "route deleted");
        self.done(
            true,
            &back,
            &format!("端口 {name} 已删除（定义也删了，重启不会回来）"),
        )
    }

    /// 下架：停止监听，但定义留着（可「恢复」）。
    async fn route_disable(&self, name: &str) -> HyperResponse<ProxyBody> {
        let back = format!("{PREFIX}/routes");
        let Some(e) = self.route(name) else {
            return self.not_found_route(name);
        };
        // 删掉自己脚下的端口 = 立刻把自己的连接掐了，用户看到的是「页面加载失败」。
        if e.spec.listen.parse::<SocketAddr>().ok() == Some(self.local) {
            return self.done(
                false,
                &back,
                &format!(
                    "端口 {name} 就是你现在访问管理面板用的这个（{}）。\
                     删它会把当前连接一起断掉——请从别的端口进来删。",
                    self.local
                ),
            );
        }
        // 管理面板挂在某个业务端口上时，删光业务端口会把自己关在外面。
        // 配了独立管理端口之后这个顾虑不存在（面板还能进，可以从那里加回来）。
        let enabled = self
            .gw
            .effective_routes()
            .iter()
            .filter(|r| !r.disabled)
            .count();
        if enabled <= 1 && self.gw.cfg.admin_listen.is_none() {
            return self.done(
                false,
                &back,
                "这是最后一个生效的端口，删掉之后连管理面板也进不来了（它就挂在这个端口上）。\
                 要么先加一个新的端口，要么配 admin_listen 让管理面板独占一个端口。",
            );
        }

        let (pending, action) = match self.gw.rt_store.stage_disable(&e.spec) {
            Ok(v) => v,
            Err(e) => return self.done(false, &back, &e),
        };
        // 下架不需要绑新套接字，但**要**换注册表 + 停监听，所以还是走两阶段。
        let runtimes = match self.gw.candidate_runtimes(&pending) {
            Ok(r) => r,
            Err(e) => return self.done(false, &back, &format!("下架后配置不合法：{e}")),
        };
        let prepared = match self.gw.listeners.prepare(&runtimes).await {
            Ok(p) => p,
            Err(e) => return self.done(false, &back, &e),
        };
        if let Err(e) = self.gw.rt_store.commit(&self.actor, pending, &action) {
            // prepared 在这里被 drop：刚绑的套接字全放掉，一切照旧
            return self.done(false, &back, &e);
        }
        self.gw.commit_routes(runtimes, prepared);
        tracing::info!(
            actor = %self.actor, target = %name, source = ?self.source,
            from_config = e.source == crate::routes::RouteSource::Config,
            "route disabled"
        );
        self.done(
            true,
            &back,
            &format!("端口 {name} 已下架，监听已停止。定义还留着——需要时点「恢复」。"),
        )
    }

    async fn route_enable(&self, name: &str) -> HyperResponse<ProxyBody> {
        let back = format!("{PREFIX}/routes");
        let (pending, action) = match self.gw.rt_store.stage_enable(name) {
            Ok(v) => v,
            Err(e) => return self.done(false, &back, &e),
        };
        // 恢复要重新绑端口，走完整的两阶段
        let (runtimes, prepared) = match self.gw.prepare_routes(&pending).await {
            Ok(v) => v,
            Err(e) => return self.done(false, &back, &e),
        };
        if let Err(e) = self.gw.rt_store.commit(&self.actor, pending, &action) {
            return self.done(false, &back, &e);
        }
        self.gw.commit_routes(runtimes, prepared);
        tracing::info!(actor = %self.actor, target = %name, source = ?self.source, "route enabled");
        self.done(true, &back, &format!("端口 {name} 已恢复，正在监听"))
    }

    /// 新建 / 修改端口：两阶段热应用。
    ///
    /// 顺序不能变：**先绑新端口 → 再写盘 → 最后换内存**。
    ///   * 先绑：绑不上就整体放弃，等于什么都没改（而不是写进文件后才发现端口占用）
    ///   * 再写盘：写失败就丢掉刚绑的套接字，内存不动
    ///   * 最后换内存：此时磁盘和内存一致，重启也不会回退
    async fn apply_route_change(
        &self,
        spec: crate::config::RouteConfig,
        pending: crate::routes::Pending,
        action: String,
        back: &str,
        creating: bool,
    ) -> HyperResponse<ProxyBody> {
        let prep = match self.gw.prepare_routes(&pending).await {
            Ok(v) => v,
            Err(e) => {
                // 回填表单比弹一条 302 有用：刚填的东西不该丢，尤其是改错了
                // 一个字符的时候——重填一遍端口和上游地址很烦。
                return self.render_route_form(RouteFormState {
                    is_new: creating,
                    name_locked: !creating,
                    name: spec.name,
                    listen: spec.listen,
                    upstream: spec.upstream,
                    host: spec.host.unwrap_or_default(),
                    segment: spec.segment.unwrap_or_default(),
                    strip_prefix: spec.strip_prefix.unwrap_or_default(),
                    max_body_bytes: spec
                        .max_body_bytes
                        .map(|n| n.to_string())
                        .unwrap_or_default(),
                    allowed_cidrs: spec.allowed_cidrs.join("\n"),
                    banners: vec![Banner::err(e)],
                });
            }
        };
        let (runtimes, prepared) = prep;
        if let Err(e) = self.gw.rt_store.commit(&self.actor, pending, &action) {
            // prepared 在这里被 drop：刚绑的套接字全部释放，一切照旧
            return self.done(false, back, &e);
        }
        self.gw.commit_routes(runtimes, prepared);
        tracing::info!(actor = %self.actor, source = ?self.source, action = %action, "route applied");
        self.done(true, back, &format!("{action}——已生效"))
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
        json_ok(&view)
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
            return json_err(
                StatusCode::FORBIDDEN,
                "你的账号已经被删除，会话会在过期后失效，请退出重新登录",
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
        json_ok(&view)
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

    fn route(&self, name: &str) -> Option<crate::routes::EffectiveRoute> {
        self.gw
            .effective_routes()
            .into_iter()
            .find(|r| r.spec.name == name)
    }

    fn not_found_route(&self, name: &str) -> HyperResponse<ProxyBody> {
        self.done(
            false,
            &format!("{PREFIX}/routes"),
            &format!("端口 {name} 不存在"),
        )
    }

    fn route_form_view(&self, st: RouteFormState) -> web::RouteFormView {
        web::RouteFormView {
            title: self.gw.title().to_string(),
            actor: self.actor.clone(),
            is_admin: self.admin,
            name: st.name,
            is_new: st.is_new,
            name_locked: st.name_locked,
            listen: st.listen,
            upstream: st.upstream,
            host: st.host,
            segment: st.segment,
            segments: self.gw.segment_names(),
            strip_prefix: st.strip_prefix,
            max_body_bytes: st.max_body_bytes,
            allowed_cidrs: st.allowed_cidrs,
            writable: self.gw.rt_store.writable(),
            banners: st.banners,
        }
    }

    /// **提交失败**：400 + `{error, form}`。
    fn render_route_form(&self, st: RouteFormState) -> HyperResponse<ProxyBody> {
        let error = st
            .banners
            .iter()
            .find(|b| b.kind == "err")
            .map(|b| b.text.clone())
            .unwrap_or_else(|| "提交的内容没有通过校验".into());
        let view = self.route_form_view(st);
        json_with(
            StatusCode::BAD_REQUEST,
            json!({ "error": error, "form": view }),
        )
    }

    /// **打开表单**（GET 详情）：200 + 表单本身。
    ///
    /// 状态码必须是 200。前端的 `api()` 把非 2xx 一律当失败抛出，早先这里复用
    /// 了 400 那条路径，于是点「编辑」只会在控制台留下一句
    /// 「提交的内容没有通过校验」，弹窗根本不出现。
    fn route_form_ok(&self, st: RouteFormState) -> HyperResponse<ProxyBody> {
        json_ok(&self.route_form_view(st))
    }

    fn account(&self, name: &str) -> Option<Account> {
        self.gw.accounts().into_iter().find(|a| a.user.name == name)
    }

    fn not_found_user(&self, name: &str) -> HyperResponse<ProxyBody> {
        self.done(false, &format!("{PREFIX}/"), &format!("账号 {name} 不存在"))
    }

    /// 一次操作的结果。
    ///
    /// 从前这里是 `302 → 目标页 + flash 塞进会话`，因为表单提交必须让浏览器
    /// 跳转一次（否则刷新会重复提交）。JSON 客户端不存在这个问题：它自己决定
    /// 要不要重新拉列表，所以直接回结果，让前端就地显示。
    ///
    /// `ok` 显式给，不靠文案猜。「已保存」和「不能保存」都能被一条
    /// `contains("已")` 判错，而错误提示显示成绿色勾是最糟的反馈。
    ///
    /// `_to` 保留着：前端的"该刷新哪个列表"本来就由它自己按当前标签页决定，
    /// 不需要服务端指路。
    fn done(&self, ok: bool, _to: &str, msg: &str) -> HyperResponse<ProxyBody> {
        json_msg(ok, msg)
    }

    /// 表单校验失败：回**结构体本身**（前端要拿它回填），外加一条错误。
    ///
    /// 状态码 400 而不是 200：这不是"成功返回了一个表单"，是"你提交的东西
    /// 没被接受"。前端的 `api()` 据此抛错，错误信息照样能取到。
    fn user_form_view(&self, st: FormState) -> UserFormView {
        UserFormView {
            title: self.gw.title().to_string(),
            actor: self.actor.clone(),
            is_admin: self.admin,
            name: st.name,
            is_new: st.is_new,
            name_locked: st.name_locked,
            admin: st.admin,
            routes: st.routes,
            available: self.gw.route_names(),
            writable: self.gw.store.writable(),
            banners: st.banners,
        }
    }

    /// **提交失败**：400 + `{error, form}`。前端据此在弹窗里显示错误、保留已填内容。
    fn render_form(&self, st: FormState) -> HyperResponse<ProxyBody> {
        let error = st
            .banners
            .iter()
            .find(|b| b.kind == "err")
            .map(|b| b.text.clone())
            .unwrap_or_else(|| "提交的内容没有通过校验".into());
        let view = self.user_form_view(st);
        json_with(StatusCode::BAD_REQUEST, json!({ "error": error, "form": view }))
    }

    /// **打开表单**（GET 详情）：200 + 表单本身。理由同 `route_form_ok`。
    fn user_form_ok(&self, st: FormState) -> HyperResponse<ProxyBody> {
        json_ok(&self.user_form_view(st))
    }
}

/// 新建 / 编辑端口表单的初始值。
#[derive(Default)]
struct RouteFormState {
    is_new: bool,
    name_locked: bool,
    name: String,
    listen: String,
    upstream: String,
    host: String,
    segment: String,
    strip_prefix: String,
    max_body_bytes: String,
    allowed_cidrs: String,
    banners: Vec<Banner>,
}

/// 提交上来的端口表单。
struct RouteInput {
    name: String,
    listen: String,
    upstream: String,
    host: String,
    segment: String,
    strip_prefix: String,
    max_body_bytes: String,
    allowed_cidrs: String,
}

impl RouteInput {
    fn from_form(f: &Form) -> Self {
        Self {
            name: f.get("name").unwrap_or("").trim().to_string(),
            listen: f.get("listen").unwrap_or("").trim().to_string(),
            upstream: f.get("upstream").unwrap_or("").trim().to_string(),
            host: f.get("host").unwrap_or("").trim().to_string(),
            segment: f.get("segment").unwrap_or("").trim().to_string(),
            strip_prefix: f.get("strip_prefix").unwrap_or("").trim().to_string(),
            max_body_bytes: f.get("max_body_bytes").unwrap_or("").trim().to_string(),
            allowed_cidrs: f.get("allowed_cidrs").unwrap_or("").trim().to_string(),
        }
    }

    fn to_state(&self, is_new: bool, name_locked: bool, err: String) -> RouteFormState {
        RouteFormState {
            is_new,
            name_locked,
            name: self.name.clone(),
            listen: self.listen.clone(),
            upstream: self.upstream.clone(),
            host: self.host.clone(),
            segment: self.segment.clone(),
            strip_prefix: self.strip_prefix.clone(),
            max_body_bytes: self.max_body_bytes.clone(),
            allowed_cidrs: self.allowed_cidrs.clone(),
            banners: vec![Banner::err(err)],
        }
    }

    /// 校验并组装。表单层面的检查在这里，语义层面的（端口冲突、自环）交给
    /// `config::validate_route_list`——那一套启动时也在用，不该有两份。
    fn validate(
        &self,
        gw: &Gateway,
        is_new: bool,
        existing: Option<&str>,
    ) -> Result<crate::config::RouteConfig, String> {
        let name = if is_new {
            if !crate::config::valid_name(&self.name) {
                return Err("端口名不合法：只能用字母、数字、. - _，长度 1..=64".into());
            }
            if gw.route_names().iter().any(|n| n == &self.name) {
                return Err(format!("端口名 {} 已经存在", self.name));
            }
            self.name.clone()
        } else {
            existing.unwrap_or("").to_string()
        };

        // 只在"新建"时允许留空上限；编辑时留空 = 回到默认 10 MB
        let max_body_bytes = if self.max_body_bytes.is_empty() {
            None
        } else {
            match self.max_body_bytes.parse::<u64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    return Err(format!(
                        "请求体上限 {:?} 不是数字（单位是字节，留空 = 默认 10 MB）",
                        self.max_body_bytes
                    ))
                }
            }
        };

        // 白名单：一行一个，逗号也行——两种都有人写，认两种比报错友好。
        let allowed_cidrs: Vec<String> = self
            .allowed_cidrs
            .split(['\n', ',', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        // 上游不能指向本网关自己的任何一个监听端口：那会形成跨端口自环
        // （最轻的表现是"二次认证"——401，最坏是打满一个核心）。
        //
        // `config` 层只拦得住 `upstream == listen` 这一种，因为它看不到
        // admin_listen，也没法在单条路由的上下文里判断"这还是我自己"。
        if let Ok(up) = self.upstream.parse::<std::net::SocketAddr>() {
            let mine = gw.routes.all().iter().any(|r| r.listen.port() == up.port())
                || gw.cfg.admin_listen.map(|a| a.port()) == Some(up.port());
            if mine {
                return Err(format!(
                    "上游 {} 是这个网关自己的监听端口——转发过去会形成自环\
                     （表现是莫名其妙的 401 或者 CPU 打满）。上游应该是面板的地址。",
                    self.upstream
                ));
            }
        }

        // 网段归属：表单里填了就听表单的，没填就继承原来的值。
        //
        // 继承是必须的：面板的表单只显示它认识的字段，而一条路由一旦归属某个
        // 网段，listen 就只能在那个网段里。如果编辑时把 segment 丢了，保存出去的
        // 那条路由会变成「网段内地址却没声明归属」——被校验拦下，用户会以为是
        // 面板坏了。要么传上来，要么保留，没有第三种正确做法。
        let inherited_segment = if is_new {
            None
        } else {
            existing
                .and_then(|n| gw.effective_routes().into_iter().find(|r| r.spec.name == n))
                .and_then(|r| r.spec.segment)
        };
        let segment = Some(self.segment.clone())
            .filter(|s| !s.is_empty())
            .or(inherited_segment);

        Ok(crate::config::RouteConfig {
            name,
            listen: self.listen.clone(),
            upstream: self.upstream.clone(),
            // 空串 = 不限定 Host（这个端口的兜底）。后端会做归一化与校验：
            // 带路径、带通配符、有空格的都在那里报错，这里不重复一套规则。
            host: Some(self.host.clone()).filter(|h| !h.is_empty()),
            segment,
            allowed_cidrs,
            strip_prefix: if self.strip_prefix.is_empty() {
                None
            } else {
                Some(self.strip_prefix.clone())
            },
            max_body_bytes,
        })
    }
}

/// 一条路由的附加说明（前缀 / 白名单 / 体积上限），列表里显示成一列。
fn route_detail(spec: &crate::config::RouteConfig) -> String {
    let mut parts = Vec::new();
    if let Some(h) = &spec.host {
        parts.push(format!("Host = {h}"));
    }
    if let Some(s) = &spec.segment {
        parts.push(format!("网段 {s}"));
    }
    if let Some(p) = &spec.strip_prefix {
        parts.push(format!("剥前缀 {p}"));
    }
    if !spec.allowed_cidrs.is_empty() {
        parts.push(format!("来源白名单 {} 条", spec.allowed_cidrs.len()));
    }
    if let Some(n) = spec.max_body_bytes {
        parts.push(format!("体积上限 {}", human_bytes(n)));
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join("，")
    }
}

fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n >= GB && n.is_multiple_of(GB) {
        format!("{} GB", n / GB)
    } else if n >= MB && n.is_multiple_of(MB) {
        format!("{} MB", n / MB)
    } else if n >= KB {
        format!("{} KB", n / KB)
    } else {
        format!("{n} B")
    }
}

fn route_store_line(gw: &Gateway) -> String {
    let store = &gw.rt_store;
    let path = store
        .path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "（未配置 routes_file）".into());
    let state = if store.writable() {
        format!("可写，{} 条", store.count())
    } else {
        "只读".to_string()
    };
    format!("{path} —— {state}。同名端口以路由库为准；保存时立即生效，端口绑不上就整体不生效。")
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
            // 多个同名 `route` 字段：这就是为什么不能复用 parse_form。
            //
            // 两种来源都认：老汉堡表单提交 `route=a&route=b`，面板前端提交
            // `{"routes":["a","b"]}`。只认前者的话，用户在界面上勾了端口也等于没勾。
            routes: {
                let mut picked = f.values("route");
                picked.extend(f.values("routes"));
                picked
            },
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
        let known = gw.route_names();
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

    /// JSON 体 → 同一套键值对。
    ///
    /// 面板前端发的是 `application/json`，而校验逻辑全长在 `Form` 上。与其把
    /// 十几个 `from_form` 各写两遍，不如在入口就把两种编码抹平：
    ///
    /// * 字符串原样；布尔 → `"true"` / `"false"`（`flag()` 认这两个）；数字 → 十进制文本
    ///   （`get()` 拿到的仍是能 `.parse()` 的字符串）
    /// * 数组 → 同名多值：`{"routes":["a","b"]}` 与 `route=a&route=b` 等价
    /// * `null` 与嵌套对象跳过 —— 前者本来就是"没填"，后者在这套扁平表单里没有对应
    fn parse_json(body: &str) -> Result<Self, &'static str> {
        let value: serde_json::Value =
            serde_json::from_str(body).map_err(|_| "请求体不是合法 JSON")?;
        let serde_json::Value::Object(map) = value else {
            return Err("请求体必须是 JSON 对象");
        };
        let mut v = Vec::new();
        for (k, val) in map {
            push_json_field(&mut v, k, val);
        }
        Ok(Self(v))
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

/// 把一个 JSON 值摊成 `(键, 文本)`，数组摊成同名多值。
fn push_json_field(out: &mut Vec<(String, String)>, key: String, value: serde_json::Value) {
    match value {
        serde_json::Value::String(s) => out.push((key, s)),
        serde_json::Value::Bool(b) => out.push((key, b.to_string())),
        serde_json::Value::Number(n) => out.push((key, n.to_string())),
        serde_json::Value::Array(items) => {
            for item in items {
                push_json_field(out, key.clone(), item);
            }
        }
        serde_json::Value::Null | serde_json::Value::Object(_) => {}
    }
}

/// 读请求体并解析成 `Form`。
///
/// **两种编码都认**：前端（合并后的面板）发 `application/json`，命令行与老表单
/// 发 `application/x-www-form-urlencoded`。只看 content-type，不看内容猜测——
/// 一个以 `{` 开头的表单值不该被误当成 JSON。
///
/// 错误只回一条文案，由调用方决定怎么渲染——把整个 `Response` 塞进 `Err`
/// 会让每条 `Result` 都被撑大好几百字节。
async fn read_form(req: HyperRequest<Incoming>) -> Result<Form, &'static str> {
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.trim_start()
                .to_ascii_lowercase()
                .starts_with("application/json")
        })
        .unwrap_or(false);
    let body = Limited::new(req.into_body(), MAX_FORM_BYTES)
        .collect()
        .await
        .map_err(|_| "请求体过大或读取失败")?
        .to_bytes();
    let s = std::str::from_utf8(&body).map_err(|_| "请求体不是合法 UTF-8")?;
    if is_json {
        return Form::parse_json(s);
    }
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

/// 管理 API 的统一出口。
///
/// `no-store` 是必须的：这里返回的是账号列表和在线会话，任何一层缓存把它留下
/// 都是事故。`nosniff` 防止有人把 JSON 当脚本加载。
fn json_out(status: StatusCode, body: String) -> HyperResponse<ProxyBody> {
    let mut resp = json_body(status, body);
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn json_ok<T: Serialize>(v: &T) -> HyperResponse<ProxyBody> {
    match serde_json::to_string(v) {
        Ok(body) => json_out(StatusCode::OK, body),
        Err(e) => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("序列化失败：{e}"),
        ),
    }
}

fn json_with(status: StatusCode, v: serde_json::Value) -> HyperResponse<ProxyBody> {
    json_out(status, v.to_string())
}

/// `{ "ok": bool, "message": "..." }`——写操作的结果。
fn json_msg(ok: bool, msg: &str) -> HyperResponse<ProxyBody> {
    json_with(StatusCode::OK, json!({ "ok": ok, "message": msg }))
}

/// `{ "error": "..." }`。
///
/// 前端把 `error` 当作唯一可信的错误通道（它不看状态码判断成败），所以这里的
/// 文案要**能直接给人看**——不要写 "invalid input" 这种。
fn json_err(status: StatusCode, msg: &str) -> HyperResponse<ProxyBody> {
    json_with(status, json!({ "error": msg }))
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
