<script setup lang="ts">
// 网关管理：账号 / 端口 / 会话 / 我的账号。
//
// 这个页面从前是 esc-gateway 里服务端渲染的 HTML（admin.rs + web.rs 一千多行）。
// 现在它是壳子里的一个 section，数据来自 `/__admin/api/*`——那条路径由**网关**
// 直接应答（同源），不经过面板。
//
// # 两个必须知道的约定
//
// 1. **未登录是 401，不是 302。** 从前服务端会把你重定向到登录页；合并之后
//    跳走会把这个标签页的状态一起丢掉，所以改成 401，由外壳弹登录框。
//
// 2. **写操作回 200 + `{ok, message}`**，校验失败回 400 + `{error, form}`。
//    `form` 是用来回填的——用户填了十个字段只错一个，不该让他重填一遍。
import { computed, onMounted, ref } from 'vue'
import { api, flash, json } from './ui'

const tab = ref('users')
const error = ref('')
const loading = ref(false)

const overview = ref<any>(null)
const users = ref<any>(null)
const routes = ref<any>(null)
const sessions = ref<any>(null)
const me = ref<any>(null)

/** 编辑中的对象。`null` = 不在编辑。 */
const editingUser = ref<any>(null)
const editingRoute = ref<any>(null)
const formError = ref('')
const saving = ref(false)

/** 编辑框里「可访问端口」的文本形式（逗号分隔）。Input 只吃字符串。 */
const routesText = ref('')

/** 我现在的口令（改口令用）。 */
const password = ref({ old: '', pass: '', pass2: '' })

const tabs = computed(() => {
  const base: [string, string][] = [['users', '账号'], ['routes', '端口'], ['sessions', '会话']]
  // 「我的账号」所有人都有；其余三个标签页只有管理员能进（后端还会再判一次，
  // 这里只是别把必然 403 的入口摆给普通用户）
  base.push(['me', '我的账号'])
  return overview.value?.admin ? base : [['me', '我的账号']]
})

const isAdmin = computed(() => !!overview.value?.admin)

async function guard<T>(fn: () => Promise<T>): Promise<T | null> {
  loading.value = true
  try {
    const r = await fn()
    error.value = ''
    return r
  } catch (e) {
    error.value = (e as Error).message
    return null
  } finally {
    loading.value = false
  }
}

async function loadOverview() { overview.value = await guard(() => api('/__admin/api/overview')) }
async function loadUsers() { users.value = await guard(() => api('/__admin/api/users')) }
async function loadRoutes() { routes.value = await guard(() => api('/__admin/api/routes')) }
async function loadSessions() { sessions.value = await guard(() => api('/__admin/api/sessions')) }
async function loadMe() { me.value = await guard(() => api('/__admin/api/me')) }

async function loadAll() {
  await loadOverview()
  if (isAdmin.value) await Promise.all([loadUsers(), loadRoutes(), loadSessions()])
  await loadMe()
}

function openNewUser() {
  formError.value = ''
  routesText.value = '*'
  editingUser.value = { is_new: true, name: '', admin: false, routes: ['*'] }
}
function openEditUser(row: any) {
  formError.value = ''
  editingUser.value = { is_new: false, name: row.name, admin: row.admin, routes: [], routesLabel: row.routes_label, available: null }
  // 详情要单独拉：列表里的 `routes_label` 是给人看的字符串，不能反解
  guard(() => api(`/__admin/api/users/${encodeURIComponent(row.name)}`)).then((detail: any) => {
    if (detail) {
      editingUser.value = { ...detail, is_new: false, routesLabel: row.routes_label }
      routesText.value = (detail.routes || []).join(', ')
    }
  })
}
function openNewRoute() {
  formError.value = ''
  editingRoute.value = {
    is_new: true, name: '', listen: '0.0.0.0:', upstream: '127.0.0.1:',
    host: '', segment: '', strip_prefix: '', max_body_bytes: '', allowed_cidrs: '',
  }
}
function openEditRoute(row: any) {
  formError.value = ''
  guard(() => api(`/__admin/api/routes/${encodeURIComponent(row.name)}`)).then(detail => {
    if (detail) editingRoute.value = { ...detail, is_new: false }
  })
}

/** 保存。失败时把 `form` 回填进编辑框，`error` 显示在弹窗里。 */
async function saveUser() {
  const form = editingUser.value
  saving.value = true
  formError.value = ''
  // 文本 → 端口列表。`*` 单独走 all_routes，混在列表里会被后端当成"端口名叫 *"。
  const picked = routesText.value.split(/[,，\s]+/).map(s => s.trim()).filter(Boolean)
  const body = {
    name: form.name,
    pass: form.pass || '',
    pass2: form.pass2 || '',
    // 端口框留空 = **留空**，不是"全部"。以前这里写 `|| picked.length === 0`，
    // 于是清空输入框就静默拿到全部端口 —— 一个手滑就是越权账号。
    // 空的正确结局是后端那条「至少要勾一个端口（或勾「全部端口」）」。
    all_routes: picked.includes('*'),
    routes: picked.filter((r: string) => r !== '*'),
    admin: !!form.admin,
  }
  try {
    const path = form.is_new ? '/__admin/api/users' : `/__admin/api/users/${encodeURIComponent(form.name)}`
    const r: any = await json('POST', path, body)
    flash(r.message || '已保存', r.ok === false ? 'error' : 'success')
    if (r.ok !== false) { editingUser.value = null; await Promise.all([loadUsers(), loadOverview()]) }
    else formError.value = r.message
  } catch (e) {
    formError.value = (e as Error).message
  } finally { saving.value = false }
}

async function saveRoute() {
  const form = editingRoute.value
  saving.value = true
  formError.value = ''
  const body = {
    name: form.name, listen: form.listen, upstream: form.upstream,
    // 空串 = 这个端口的兜底（不限 Host）。后端把它归一化并校验；
    // 带路径/通配符的写法在那里报错，前端不重复一套规则。
    host: form.host || '',
    // 空串 = 不归属任何网段（行为与加网段功能之前一致）。归属之后 listen 必须
    // 落在网段里、upstream 必须是本机回环——那三条由网关校验，前端不复制规则。
    segment: form.segment || '',
    strip_prefix: form.strip_prefix || '', max_body_bytes: String(form.max_body_bytes ?? ''),
    allowed_cidrs: form.allowed_cidrs || '',
  }
  try {
    const path = form.is_new ? '/__admin/api/routes' : `/__admin/api/routes/${encodeURIComponent(form.name)}`
    const r: any = await json('POST', path, body)
    flash(r.message || '已保存', r.ok === false ? 'error' : 'success')
    if (r.ok !== false) { editingRoute.value = null; await loadRoutes() }
    else formError.value = r.message
  } catch (e) {
    formError.value = (e as Error).message
  } finally { saving.value = false }
}

/** 账号/端口上的一次性动作（删除、接管、踢下线、上下架）。 */
async function act(path: string, okText: string, confirmText?: string) {
  if (confirmText && !confirm(confirmText)) return
  try {
    const r: any = await json('POST', path, {})
    flash(r.message || okText, r.ok === false ? 'error' : 'success')
    await Promise.all([loadUsers(), loadRoutes(), loadSessions()])
  } catch (e) {
    flash((e as Error).message, 'error')
  }
}

async function changePassword() {
  formError.value = ''
  if (password.value.pass !== password.value.pass2) { formError.value = '两次输入的新口令不一致'; return }
  try {
    const r: any = await json('POST', '/__admin/api/me/password', password.value)
    flash(r.message || '口令已修改', r.ok === false ? 'error' : 'success')
    if (r.ok !== false) { password.value = { old: '', pass: '', pass2: '' }; await loadMe() }
    else formError.value = r.message
  } catch (e) {
    formError.value = (e as Error).message
  }
}

/** 退出：清 cookie 后整页刷新（壳子会再撞一次 401 并弹登录框）。 */
async function logout() {
  await fetch('/__auth/logout', { credentials: 'same-origin' })
  location.reload()
}

onMounted(loadAll)
</script>

<template>
  <div>
    <div class="esc-nav-summary">
      <span v-if="overview">
        以 <strong>{{ overview.user }}</strong> 身份登录
        <template v-if="overview.admin">（管理员）</template>
        <template v-else>（普通账号，只能改自己的口令）</template>
      </span>
      <span v-if="overview && !overview.store_writable" class="esc-nav-down">账号库只读</span>
      <span v-if="overview && !overview.routes_writable" class="esc-nav-down">路由库只读</span>
      <span class="esc-spacer" />
      <NeumorphismButton size="small" @click="loadAll">刷新</NeumorphismButton>
      <NeumorphismButton size="small" @click="logout">退出登录</NeumorphismButton>
    </div>

    <NeumorphismAlert v-if="error" type="error" title="网关接口调用失败" :message="error" />
    <NeumorphismAlert
      v-if="!isAdmin && overview"
      type="info"
      title="普通账号"
      message="你不是管理员，只能管理自己的账号。端口与账号列表需要管理员权限。"
    />

    <NeumorphismTabs v-model="tab" class="esc-tabs" :tabs="tabs.map(([key, label]) => ({ key, label }))" />

    <!-- ── 账号 ─────────────────────────────────────────────────────────── -->
    <main v-if="tab === 'users' && users">
      <div class="esc-actions" style="margin-bottom: 12px">
        <NeumorphismButton @click="openNewUser">＋ 新建账号</NeumorphismButton>
        <span class="esc-hint">{{ users.store_line }}</span>
      </div>
      <NeumorphismAlert
        v-for="(b, i) in users.banners || []" :key="i"
        :type="b.kind === 'err' ? 'error' : b.kind === 'warn' ? 'warning' : 'success'"
        :message="b.text"
      />
      <div class="esc-table-wrap">
        <table class="esc-table">
          <thead><tr><th>账号</th><th>来源</th><th>权限</th><th>可访问端口</th><th>在线</th><th>操作</th></tr></thead>
          <tbody>
            <tr v-for="row in users.rows" :key="row.name">
              <td class="mono">{{ row.name }}</td>
              <td>{{ row.source_label }}</td>
              <td><NeumorphismTag :variant="row.admin ? 'primary' : 'default'">{{ row.admin ? '管理员' : '普通' }}</NeumorphismTag></td>
              <td class="mono">{{ row.routes_label }}</td>
              <td>{{ row.sessions }}</td>
              <td>
                <div class="esc-actions">
                  <NeumorphismButton v-if="row.editable" size="small" @click="openEditUser(row)">编辑</NeumorphismButton>
                  <NeumorphismButton v-else size="small" variant="primary"
                    @click="act(`/__admin/api/users/${encodeURIComponent(row.name)}/adopt`, '已接管')">接管</NeumorphismButton>
                  <NeumorphismButton size="small"
                    @click="act(`/__admin/api/users/${encodeURIComponent(row.name)}/revoke`, '已踢下线')">踢下线</NeumorphismButton>
                  <NeumorphismButton size="small" class="esc-danger"
                    @click="act(`/__admin/api/users/${encodeURIComponent(row.name)}/delete`, '已删除', `确定删除账号「${row.name}」？该账号的在线会话会立刻失效。`)">删除</NeumorphismButton>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <details class="esc-details">
        <summary>最近操作（{{ (users.recent || []).length }} 条）</summary>
        <div class="esc-table-wrap">
          <table class="esc-table">
            <thead><tr><th>时间</th><th>动作</th><th>对象</th><th>操作者</th></tr></thead>
            <tbody>
              <tr v-for="(r, i) in users.recent" :key="i">
                <td>{{ r[0] }}</td><td>{{ r[1] }}</td><td class="mono">{{ r[2] }}</td><td class="mono">{{ r[3] }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </details>
    </main>

    <!-- ── 端口 ─────────────────────────────────────────────────────────── -->
    <main v-if="tab === 'routes' && routes">
      <div class="esc-actions" style="margin-bottom: 12px">
        <NeumorphismButton @click="openNewRoute">＋ 新建端口</NeumorphismButton>
        <span class="esc-hint">{{ routes.store_line }}</span>
      </div>
      <NeumorphismAlert
        v-for="(b, i) in routes.banners || []" :key="i"
        :type="b.kind === 'err' ? 'error' : b.kind === 'warn' ? 'warning' : 'success'"
        :message="b.text"
      />
      <div class="esc-table-wrap">
        <table class="esc-table">
          <thead><tr><th>名字</th><th>Host</th><th>监听</th><th>上游</th><th>状态</th><th>来源</th><th>操作</th></tr></thead>
          <tbody>
            <tr v-for="row in routes.rows" :key="row.name">
              <td class="mono">{{ row.name }}<NeumorphismTag v-if="row.here" size="small" variant="primary">当前</NeumorphismTag></td>
              <!--
                一个监听地址现在可以挂多条路由，靠 Host 区分。空 = 兜底：
                没命中任何显式 Host 的请求都归它。必须显示出来，否则同一端口
                出现两三行会看着像配错了。
              -->
              <td class="mono">
                <template v-if="row.host">{{ row.host }}</template>
                <span v-else class="esc-hint">（兜底：任意 Host）</span>
              </td>
              <td class="mono">
                {{ row.listen }}
                <!--
                  网段标记必须和地址贴在一起看：同一个端口号，在网段里和在公网口上
                  是两件完全不同的事 —— 前者绕不过 TLS，后者能。分两列显示反而
                  容易看漏（多一列就少一分注意力）。
                -->
                <div v-if="row.segment" style="margin-top: 4px">
                  <NeumorphismTag size="small" variant="primary">网段 {{ row.segment }}</NeumorphismTag>
                </div>
              </td>
              <td class="mono">{{ row.upstream }}</td>
              <td>
                <NeumorphismTag :variant="row.disabled ? 'default' : row.live ? 'success' : 'error'">
                  {{ row.disabled ? '已下架' : row.live ? '监听中' : '未监听' }}
                </NeumorphismTag>
              </td>
              <td>{{ row.source_label }}</td>
              <td>
                <div class="esc-actions">
                  <NeumorphismButton size="small" @click="openEditRoute(row)">编辑</NeumorphismButton>
                  <NeumorphismButton v-if="!row.disabled" size="small"
                    @click="act(`/__admin/api/routes/${encodeURIComponent(row.name)}/disable`, '已下架', `下架「${row.name}」会停止监听，但定义保留。`)">下架</NeumorphismButton>
                  <NeumorphismButton v-else size="small" variant="primary"
                    @click="act(`/__admin/api/routes/${encodeURIComponent(row.name)}/enable`, '已恢复')">恢复</NeumorphismButton>
                  <NeumorphismButton size="small" class="esc-danger"
                    @click="act(`/__admin/api/routes/${encodeURIComponent(row.name)}/delete`, '已删除', `彻底删除端口「${row.name}」？配置文件里的那一份会让它复活，届时只能下架。`)">删除</NeumorphismButton>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </main>

    <!-- ── 会话 ─────────────────────────────────────────────────────────── -->
    <main v-if="tab === 'sessions' && sessions">
      <NeumorphismAlert
        v-for="(b, i) in sessions.banners || []" :key="i"
        :type="b.kind === 'err' ? 'error' : b.kind === 'warn' ? 'warning' : 'success'"
        :message="b.text"
      />
      <div class="esc-table-wrap">
        <table class="esc-table">
          <thead><tr><th>会话</th><th>账号</th><th>来源</th><th>建立</th><th>最近</th><th>操作</th></tr></thead>
          <tbody>
            <tr v-for="row in sessions.rows" :key="row.short">
              <td class="mono">{{ row.short }}</td>
              <td class="mono">{{ row.user }}<NeumorphismTag v-if="row.admin" size="small" variant="primary">管理员</NeumorphismTag></td>
              <td class="mono">{{ row.source }}</td>
              <td>{{ row.created }}</td>
              <td>{{ row.seen }}</td>
              <td>
                <NeumorphismButton size="small" class="esc-danger"
                  @click="act(`/__admin/api/sessions/${encodeURIComponent(row.short)}/revoke`, '已踢下线')">踢下线</NeumorphismButton>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </main>

    <!-- ── 我的账号 ─────────────────────────────────────────────────────── -->
    <main v-if="tab === 'me' && me">
      <div class="esc-grid">
        <NeumorphismCard>
          <div class="esc-section-title"><h2>我的账号</h2>
            <NeumorphismTag :variant="me.admin ? 'primary' : 'default'">{{ me.admin ? '管理员' : '普通' }}</NeumorphismTag>
          </div>
          <dl class="esc-kv">
            <dt>账号</dt><dd class="mono">{{ me.name }}</dd>
            <dt>来源</dt><dd>{{ me.source_label }}</dd>
            <dt>可访问</dt><dd class="mono">{{ me.routes_label }}</dd>
          </dl>
        </NeumorphismCard>
        <NeumorphismCard>
          <div class="esc-section-title"><h2>修改口令</h2></div>
          <div v-if="!me.writable" class="esc-note">
            账号库是只读的，这里改不了口令。请让 <code>users_file</code> 指向可写路径。
          </div>
          <div class="esc-form">
            <NeumorphismInput v-model="password.old" label="当前口令" type="password" />
            <NeumorphismInput v-model="password.pass" label="新口令" type="password" />
            <NeumorphismInput v-model="password.pass2" label="再输一次" type="password" />
            <NeumorphismAlert v-if="formError" type="error" :message="formError" />
            <NeumorphismButton :disabled="!me.writable" @click="changePassword">保存新口令</NeumorphismButton>
            <p class="esc-hint">
              改完其他设备的会话会立刻失效。如果这个账号原本来自 <code>gateway.toml</code>，
              改口令会顺手把它接管进账号库（否则下次重启就被配置文件覆盖回去）。
            </p>
          </div>
        </NeumorphismCard>
      </div>
    </main>

    <!-- ── 账号弹窗 ─────────────────────────────────────────────────────── -->
    <!--
      绑定方式必须是「`:model-value="!!x"` + `@update:model-value`」，不能用
      `:model-value="true"` + `@close`：
      弹窗组件关闭时走的是 `emit('update:modelValue', false)`，父组件不接这条的话
      prop 永远是 true，于是对话框**关不掉** —— X、遮罩、ESC、以及组件自带的那个
      「取消」全部失效，只有下面 `.esc-actions` 里自己写的「取消」有用（它直接改 ref）。
      这正是「取消无法退出」的来源。

      `:footer="false"`：这个表单自己有保存/取消，组件默认页脚会再渲染一对
      「取消/确定」—— 其中「确定」没有接任何保存逻辑，点了像是保存成功、实际丢改动。 -->
    <NeumorphismModal
      :model-value="!!editingUser"
      title="账号"
      :footer="false"
      @update:model-value="editingUser = null"
    >
      <div class="esc-form">
        <NeumorphismInput v-model="editingUser.name" label="账号名" :disabled="!editingUser.is_new" />
        <NeumorphismInput v-model="editingUser.pass" label="口令（留空 = 不改）" type="password" />
        <NeumorphismInput v-model="editingUser.pass2" label="再输一次" type="password" />
        <NeumorphismSwitch v-model="editingUser.admin" label="管理权限" />
        <NeumorphismInput
          v-model="routesText" label="可访问端口（逗号分隔，* = 全部）"
          placeholder="*"
        />
        <p class="esc-hint">当前可用端口：{{ (overview?.routes || []).map((r: any) => r.name).join('、') || '（没有）' }}</p>
        <NeumorphismAlert v-if="formError" type="error" :message="formError" />
        <div class="esc-actions">
          <NeumorphismButton :disabled="saving" @click="saveUser">{{ saving ? '保存中…' : '保存' }}</NeumorphismButton>
          <NeumorphismButton variant="pressed" @click="editingUser = null">取消</NeumorphismButton>
        </div>
      </div>
    </NeumorphismModal>

    <!-- ── 端口弹窗 ─────────────────────────────────────────────────────── -->
    <NeumorphismModal
      :model-value="!!editingRoute"
      title="端口"
      :footer="false"
      @update:model-value="editingRoute = null"
    >
      <div class="esc-form">
        <NeumorphismInput v-model="editingRoute.name" label="端口名" :disabled="!editingRoute.is_new" />
        <NeumorphismInput v-model="editingRoute.listen" label="监听地址（host:port）" />
        <NeumorphismInput v-model="editingRoute.upstream" label="上游（host:port）" />
        <!--
          网段只在 gateway.toml 里声明，这里只做归属（不可创建）。改网段本身是
          改拓扑，必须重启 —— 所以没有"新建网段"的入口。
        -->
        <NeumorphismSelect
          v-if="(overview?.segments || []).length"
          v-model="editingRoute.segment"
          label="隔离网段（可选）"
          placeholder="不归属（这个地址直接对外）"
          clearable
          :options="(overview?.segments || []).map((s: string) => ({ value: s, label: s }))"
        />
        <span v-if="(overview?.segments || []).length" class="esc-hint">
          归属网段后：监听必须是网段内的具体地址（不能 0.0.0.0），上游必须是本机回环。
        </span>
        <NeumorphismInput v-model="editingRoute.strip_prefix" label="剥离前缀（可选）" />
        <NeumorphismInput v-model="editingRoute.max_body_bytes" label="请求体上限（字节，留空 = 10 MB）" />
        <NeumorphismInput v-model="editingRoute.allowed_cidrs" label="来源白名单（一行一个或逗号分隔）" />
        <NeumorphismAlert v-if="formError" type="error" :message="formError" />
        <div class="esc-actions">
          <NeumorphismButton :disabled="saving" @click="saveRoute">{{ saving ? '保存中…' : '保存' }}</NeumorphismButton>
          <NeumorphismButton variant="pressed" @click="editingRoute = null">取消</NeumorphismButton>
        </div>
      </div>
    </NeumorphismModal>
  </div>
</template>
