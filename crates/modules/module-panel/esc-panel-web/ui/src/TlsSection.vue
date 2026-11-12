<script setup lang="ts">

// TLS 终结层（插件 `esc.tls`）。
//
// # 这一页要回答的问题
//
// 「2222 这个端口是谁在听、转发到哪、要不要登录」—— 从前这一层是 nginx 容器，
// 面板完全看不见它，于是这个问题只能在几个页面之间来回切才拼得出来。
// 收进 `esc-host` 之后它和隧道在同一个进程里，这条链路终于能在一屏内讲完：
//
//     :2222 ──TLS──▶ 127.0.0.1:18790 ──隧道──▶ 4090 的 127.0.0.1:8080
//      └─ 本站点 ──┘  └──── 端口映射 ────────┘
//
// 所以这个组件的核心不是"列出站点"，而是**把两段接起来**：`site.target`
// 与某条 mapping 的 `listen` 相等时，那条链路就完整了。
//
// # 为什么现在能改（以前是只读的）
//
// 从前证书与监听端口的改动要重启宿主才生效（见 docs/TLS-MODULE.md），一套只能
// 改内存、活不过重启的输入框比没有更误导，所以这一页只展示不编辑。
// 现在宿主侧的 `tls.config.apply` 会在校验通过后**当场**换掉监听与证书
// （能不变的端口不重绑、正在跑的连接不断），保存这件事才终于说得通。
//
// # 编辑表单必须从 `status.config` 出发
//
// 不能拿上面那些展示字段反推，哪怕它们看起来"就是同一份数据"：
//
//   * `sites[].max_body_bytes` / `hsts` 是**合并默认之后**的生效值。拿它回填，
//     「继承全局」就被悄悄写死成「显式等于这个数字」——数字没变，语义变了：
//     以后改全局默认，这个站点不再跟着动。
//   * `sites[].target` 是把 `upstream` / `redirect` 拼成的一个字符串，反推不回
//     它原本是哪一种。
//   * 私钥路径**只在** `config.cert[]` 里，展示用的 `certs[]` 没有它。

import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

import { api, bytes, duration, flash, json } from './ui'

const tls = ref<any>(null)
const mappings = ref<any[]>([])
const error = ref('')

/** 编辑态。开着时轮询让路（见 onMounted）——只读视图在底下悄悄变，会让人
 *  以为"我还没保存的改动已经生效了"。 */
const editing = ref(false)
const saving = ref(false)
/** 保存失败的原因。后端返回的已经是逐条中文，这里**原样**放上去，不改写不截断。 */
const formError = ref('')
/** 编辑态的那份可丢弃副本。取消时整份扔掉，不必逐字段回滚。 */
const form = ref<any>(emptyForm())

/** 从「端口映射」那边拿到的 mapping.listen → mapping 的索引。 */
const mappingByListen = computed(() => {
  const m = new Map<string, any>()
  for (const x of mappings.value) if (x.listen) m.set(x.listen, x)
  return m
})

async function load(quiet = false) {
  try {
    // 两个接口一起拉：这一页的存在意义就是「把两段接起来」，分两次请求
    // 会让中间有一帧只显示半条链路。
    const [t, m] = await Promise.all([api('/api/tunnel/tls'), api('/api/tunnel/mappings')])
    tls.value = t
    mappings.value = m
    error.value = ''
  } catch (e) {
    if (!quiet) error.value = (e as Error).message
  }
}

// ── 编辑态：表单 ↔ 配置 ─────────────────────────────────────────────────────

/** 数字字段在表单里一律存字符串：输入框清空重打时会经过空串，收成 number
 *  会让光标在 `12` → `` → `3` 的过程中跳一下。 */
function text(v: unknown): string {
  return v === null || v === undefined ? '' : String(v)
}

/** 每行的稳定 key。用下标做 key 的话，删掉中间一行会让后面几行的 DOM 复用错位
 *  （正在输入的框突然对上另一条记录）。`_k` 只活在表单里，不进提交体。 */
let seq = 0
const nextKey = () => `f${++seq}`

function emptyForm() {
  return {
    defaults: { cert: '', tls12: true, max_body: '', handshake: '', hsts: false },
    sites: [] as any[],
    certs: [] as any[],
  }
}

/** `status.config` → 表单。字段名与提交体基本一致，转换只发生在"表达不了的东西"上：
 *  多端口 ↔ 逗号分隔的一行、`upstream`/`redirect` ↔ 一个类型选择、`null` ↔ 空/继承。 */
function formFromConfig(cfg: any) {
  const d = cfg?.defaults || {}
  return {
    defaults: {
      // 空串 = 不指定，后端会退回"第一张证书"。留着一个不存在的名字会直接报错。
      cert: typeof d.cert === 'string' ? d.cert : '',
      tls12: d.tls12 !== false,
      max_body: text(d.max_body_bytes),
      handshake: text(d.handshake_timeout_secs),
      hsts: !!d.hsts,
    },
    sites: (cfg?.site || []).map((s: any) => ({
      _k: nextKey(),
      name: text(s.name),
      // 多端口在表单里是逗号分隔的一行：一个站点挂 443 + 2224 是常态，做成
      // 动态行会让"改其中一个端口"要点三次。
      listen: (s.listen || []).join(', '),
      // 两个互斥字段里哪个非空就是哪种类型；都空时先按转发展示，保存前的
      // 自查会把它拦下来（后端也会拦）。
      kind: s.redirect ? 'redirect' : 'proxy',
      upstream: text(s.upstream),
      redirect: text(s.redirect),
      websocket: !!s.websocket,
      log_query: !!s.log_query,
      read_timeout: text(s.read_timeout_secs ?? 300),
      // 空串 = 继承全局默认（三态的第一态）。写成 0 意思就完全变了。
      max_body: s.max_body_bytes === null || s.max_body_bytes === undefined ? '' : text(s.max_body_bytes),
      hsts: s.hsts === null || s.hsts === undefined ? 'inherit' : s.hsts ? 'on' : 'off',
    })),
    certs: (cfg?.cert || []).map((c: any) => ({
      _k: nextKey(),
      name: text(c.name),
      cert: text(c.cert),
      key: text(c.key),
      sni: (c.sni || []).join(', '),
    })),
  }
}

/** 后端没给 `status.config` 就**不要**开编辑：从 `sites` 反推会把"继承"写成
 *  "显式"，保存一次就悄悄改掉了语义。这种情况只可能是宿主版本旧。 */
const canEdit = computed(() => !!tls.value?.config)

function startEdit() {
  // 不需要深拷贝：formFromConfig 是逐字段构造一个新的普通对象。
  // （`tls.value.config` 是响应式代理，structuredClone 碰到 Proxy 会抛
  //  DataCloneError —— 这也是这里手写构造而不是整体克隆的原因之一。）
  form.value = formFromConfig(tls.value?.config)
  formError.value = ''
  editing.value = true
}

async function cancel() {
  editing.value = false
  formError.value = ''
  // 「取消」= 丢弃改动并**重新读一遍**：这期间别人可能改过，回到打开编辑时的
  // 旧快照会让人以为那些改动是自己的。
  await load()
  tickRate()
}

/** 类型切换。`upstream` 与 `redirect` 在配置里互斥，两边都带后端会直接拒，
 *  切过去就把另一边清掉——别留一个看不见的旧值等着下次被提交。
 *  （下拉先发 `update:modelValue` 再发 `change`，所以走到这里 `kind` 已是新值。） */
function onKindChange(site: any) {
  if (site.kind === 'proxy') site.redirect = ''
  else site.upstream = ''
}

function addSite() {
  form.value.sites.push({
    _k: nextKey(),
    // 默认值对齐配置里的缺省：`log_query = false`（承载令牌的端口靠它保证
    // 查询串不落盘）、`websocket = false`、读超时 300 秒。
    name: '', listen: '0.0.0.0:', kind: 'proxy', upstream: '127.0.0.1:',
    redirect: '', websocket: false, log_query: false, read_timeout: '300',
    max_body: '', hsts: 'inherit',
  })
}

/** 删一行站点。**没有**"删全部"：把表清空等于关掉这一层，那种操作该走配置文件
 *  的 review，不该是一个按钮的副作用。 */
function removeSite(i: number) {
  form.value.sites.splice(i, 1)
}

function addCert() {
  form.value.certs.push({ _k: nextKey(), name: '', cert: '', key: '', sni: '' })
}

function removeCert(i: number) {
  const gone = form.value.certs[i]?.name?.trim()
  form.value.certs.splice(i, 1)
  // 删掉的正好是"默认证书"时顺手把引用清掉，否则下一次保存必然被
  // 「defaults.cert 没有对应证书」挡住一次。
  if (gone && form.value.defaults.cert === gone) form.value.defaults.cert = ''
}

/** 逗号（中英文都认）/ 空白分隔 → 数组。端口和 SNI 共用：从文档里粘一行
 *  `0.0.0.0:443, 0.0.0.0:2224` 是常态，不该逼人逐个输入框地填。 */
function splitList(raw: string): string[] {
  return String(raw || '').split(/[,，\s]+/).map(s => s.trim()).filter(Boolean)
}

/** 表单 → 提交体（形状 = `tls.config.apply` 的入参）。 */
function payload() {
  const f = form.value
  const n = (v: string) => Number(v.trim())
  return {
    defaults: {
      // 空 = 不指定（后端退回第一张证书）。别把空串发过去：那不是"没有默认证书"，
      // 那是一个名字为空的证书引用。
      cert: f.defaults.cert || null,
      tls12: !!f.defaults.tls12,
      max_body_bytes: n(f.defaults.max_body),
      handshake_timeout_secs: n(f.defaults.handshake),
      hsts: !!f.defaults.hsts,
    },
    cert: f.certs.map((c: any) => ({
      name: c.name.trim(),
      cert: c.cert.trim(),
      key: c.key.trim(),
      sni: splitList(c.sni),
    })),
    site: f.sites.map((s: any) => ({
      name: s.name.trim(),
      listen: splitList(s.listen),
      // 互斥的两个字段只带生效的那一半：两边都带后端会报"只能配一个"。
      upstream: s.kind === 'proxy' ? s.upstream.trim() : null,
      redirect: s.kind === 'redirect' ? s.redirect.trim() : null,
      // 301 不会升级连接，后端会拒绝"redirect 站点开 websocket"，这里直接
      // 不提交它；切换类型时表单里的开关值仍然留着，切回转发就还在。
      websocket: s.kind === 'proxy' && !!s.websocket,
      log_query: !!s.log_query,
      read_timeout_secs: n(s.read_timeout),
      // `null` = 继承全局默认。这两种"没填"必须原样传成 null：写成 0 / false
      // 就把"跟着默认走"变成了"显式钉死"。
      max_body_bytes: s.max_body.trim() === '' ? null : n(s.max_body),
      hsts: s.hsts === 'inherit' ? null : s.hsts === 'on',
    })),
  }
}

// ── 提交前自查 ──────────────────────────────────────────────────────────────

/** `IP:端口`。只认 IP 字面量是故意的：后端把 `listen` / `upstream` 解析成
 *  `SocketAddr`，**不查 DNS**，写主机名一定失败。 */
const ADDR_RE = /^(\d{1,3}(?:\.\d{1,3}){3}|\[[0-9a-fA-F:]+\]):(\d{1,5})$/

/** 整数框解析。空 / 非整数一律 `null`，由调用方给中文提示。 */
function intOf(raw: string): number | null {
  const s = String(raw ?? '').trim()
  if (!/^\d+$/.test(s)) return null
  const n = Number(s)
  return Number.isSafeInteger(n) ? n : null
}

/** 一个 `IP:端口` 哪儿不对；`null` = 能用。 */
function addrProblem(raw: string): string | null {
  if (!ADDR_RE.test(raw)) return `要写成 IP:端口（例如 0.0.0.0:2222）；后端把它解析成 SocketAddr，不查 DNS，主机名一定失败`
  const port = Number(raw.slice(raw.lastIndexOf(':') + 1))
  if (!(port >= 1 && port <= 65535)) return `端口 ${port} 超出 1–65535`
  return null
}

/** 提交前自查。后端才是权威（它还要查证书文件读不读得到、端口能不能绑），
 *  这里只挡"一眼就能看出错"的那几条，省一次往返。 */
function validate(): string[] {
  const bad: string[] = []
  const f = form.value

  const body = intOf(f.defaults.max_body)
  if (body === null || body < 1) bad.push('全局默认：请求体上限要填一个 ≥ 1 的整数（字节）；留空不会变成"不限制"')
  const hs = intOf(f.defaults.handshake)
  if (hs === null || hs < 1) bad.push('全局默认：握手超时要填一个 ≥ 1 的整数（秒）')

  // 证书
  const certNames = new Set<string>()
  f.certs.forEach((c: any, i: number) => {
    const name = c.name.trim()
    const where = name ? `证书「${name}」` : `第 ${i + 1} 张证书`
    if (!name) bad.push(`第 ${i + 1} 张证书：引用名不能为空（默认证书与 SNI 都靠它指认）`)
    else if (certNames.has(name)) bad.push(`证书名「${name}」重复了`)
    else certNames.add(name)
    if (!c.cert.trim()) bad.push(`${where}：证书链路径不能为空`)
    if (!c.key.trim()) bad.push(`${where}：私钥路径不能为空`)
  })
  // 默认证书必须指向表里存在的一张，否则后端一定会拒。
  if (f.defaults.cert && !certNames.has(f.defaults.cert)) {
    bad.push(`默认证书「${f.defaults.cert}」不在证书表里，请重新选一张，或先在下面把它加回来`)
  }

  // 站点
  const siteNames = new Set<string>()
  const portOwner = new Map<string, string>() // 监听地址 → 哪个站点
  f.sites.forEach((s: any, i: number) => {
    const name = s.name.trim()
    const where = name ? `站点「${name}」` : `第 ${i + 1} 个站点`
    if (!name) bad.push(`第 ${i + 1} 个站点：name 不能为空（日志里用它标识站点——端口会变，它不变）`)
    else if (siteNames.has(name)) bad.push(`站点名「${name}」重复了`)
    else siteNames.add(name)

    const addrs = splitList(s.listen)
    if (!addrs.length) bad.push(`${where}：listen 至少要有一个监听地址，例如 0.0.0.0:2222`)
    const seen = new Set<string>()
    for (const a of addrs) {
      if (seen.has(a)) { bad.push(`${where}：监听地址「${a}」写了两遍`); continue }
      seen.add(a)
      const p = addrProblem(a)
      if (p) { bad.push(`${where}：监听地址「${a}」${p}`); continue }
      const owner = portOwner.get(a)
      if (owner && owner !== where) bad.push(`监听地址「${a}」同时配在「${owner}」和「${where}」上，一个地址只能挂一个站点`)
      else if (!owner) portOwner.set(a, where)
    }

    if (s.kind === 'proxy') {
      const up = s.upstream.trim()
      if (!up) bad.push(`${where}：类型是"转发到上游"，upstream 不能为空（形如 127.0.0.1:18790）`)
      else {
        const p = addrProblem(up)
        if (p) bad.push(`${where}：upstream「${up}」${p}`)
        else if (addrs.includes(up)) bad.push(`${where}：upstream 就是它自己的监听地址「${up}」，转给自己会兜圈子`)
      }
    } else {
      const r = s.redirect.trim()
      if (!r) bad.push(`${where}：类型是"301 跳转"，redirect 模板不能为空（形如 https://{hostname}{uri}）`)
      else if (!/^https?:\/\//.test(r)) bad.push(`${where}：redirect 要以 http:// 或 https:// 开头`)
      // 跳到**另一个端口**时 `{host}` 会把自己带的端口一起带过去，拼出
      // `https://x:2223:2224/…`。后端不拦（模板是它的自由文本），
      // docs/TLS-MODULE.md 里专门标了这条 ❌，所以只能在这里挡。
      else if (/\{host\}:\d/.test(r)) bad.push(`${where}：redirect 里的「{host}:端口」拼不出合法 URL —— {host} 自带端口，跳到别的端口要用 {hostname}`)
    }

    const rt = intOf(s.read_timeout)
    if (rt === null || rt < 1) bad.push(`${where}：上游超时要填一个 ≥ 1 的整数（秒）`)
    const mb = s.max_body.trim()
    if (mb !== '') {
      const m = intOf(mb)
      if (m === null || m < 1) bad.push(`${where}：请求体上限要么留空（继承全局默认），要么填一个 ≥ 1 的整数（字节）`)
    }
  })

  return bad
}

/** 保存。成功 → 用回执里的 status 刷新 + 退出编辑态；失败 → 原样显示后端的话。 */
async function save() {
  if (saving.value) return
  const bad = validate()
  if (bad.length) {
    // 和后端同一个排版（`·` 打头的逐条），两条信息看起来才是一回事。
    formError.value = '提交前自查发现问题：\n' + bad.map(t => `  · ${t}`).join('\n')
    return
  }
  saving.value = true
  formError.value = ''
  try {
    const out: any = await json('POST', '/api/tunnel/tls/config', payload())
    // 用回执里的 status 刷新，**不再**发一次 GET：这份就是应用之后的实际状态，
    // 再查一次只会把"两次请求之间又被改了一版"的结果混进来。
    if (out?.status) tls.value = out.status
    // 计数基准清掉：status 里带着新的 counts，留着旧基准会算出负速率或一个
    // 巨大的瞬时值（见 tickRate）。
    prev.value = null
    rate.value = null
    tickRate()
    editing.value = false
    flash(outcomeText(out), out?.warn ? 'error' : 'success')
  } catch (e) {
    // 原样显示：后端返回的已经是逐条中文（`{"error": "…\n  · …"}`），
    // 在这里改写或截断只会丢掉"哪一条不合法"。
    formError.value = (e as Error).message
  } finally {
    saving.value = false
  }
}

/** 保存成功后的一句话结论。
 *
 *  `store` 为空必须说出来：它意味着**只改了内存**，重启回到 host.toml 的基线 ——
 *  「保存成功」和「重启后变回去」之间的落差要当场交代，不能等到重启那天。
 *  `warn`（后端已定义，可选）同理，原样接在后面。 */
function outcomeText(out: any): string {
  const n = (out?.rebound || []).length
  const m = (out?.released || []).length
  const k = out?.unchanged ?? 0
  const where = out?.store
    ? `已写入 ${out.store}`
    : '只在内存生效，重启后回到 host.toml 的基线'
  const head = `已生效：重绑 ${n} 个端口、释放 ${m} 个、${k} 个未受影响；${where}`
  return out?.warn ? `${head}。${out.warn}` : head
}

// ── 编辑态用到的选项与提示 ──────────────────────────────────────────────────

/** 默认证书的下拉项取**表单里**的证书表，不是 `tls.certs`：编辑中可能刚加了
 *  一张还没保存的证书，下拉里得能选到它。 */
const certOptions = computed(() =>
  (form.value.certs || []).map((c: any) => ({
    label: c.name.trim() || '（未命名）',
    value: c.name.trim(),
  })),
)

const kindOptions = [
  { label: '转发到上游', value: 'proxy' },
  { label: '301 跳转', value: 'redirect' },
]

/** HSTS 是三态：继承 / 显式开 / 显式关。开/关两态用开关表达不了第三态，
 *  所以这里用下拉，并把"继承到的是什么"直接写进选项里。 */
const triOptions = computed(() => [
  { label: `继承全局（当前：${form.value.defaults.hsts ? '开启' : '关闭'}）`, value: 'inherit' },
  { label: '显式开启', value: 'on' },
  { label: '显式关闭', value: 'off' },
])

/** 站点请求体上限留空 = 继承，而"继承"到底是多少只有全局默认知道。 */
const bodyPlaceholder = computed(() => `继承（${bytes(form.value.defaults.max_body)}）`)

// ── 派生视图（只读） ────────────────────────────────────────────────────────

/** 端口 → 该端口后面的隧道（没有就是 `undefined`，表示"本机某个服务"）。 */
const chains = computed(() =>
  (tls.value?.sites || []).map((s: any) => {
    const m = s.kind === 'proxy' ? mappingByListen.value.get(s.target) : undefined
    return {
      site: s,
      mapping: m,
      // 完整链路才谈得上"通到哪"
      // hub 侧的 mapping 记录里 `local` 是空的 —— 目标地址在 spoke 那台机器上，
      // 握手时不上报。所以这里不能编一个地址出来，只能说明"终点由对端决定"。
      // 这本身就是个该被看见的事实：面板能看到的链路终点**止于隧道**。
      endpoint: m ? (m.local || `由 ${(m.providers || [])[0] || '对端'} 决定`) : null,
      provider: m ? (m.providers || []).join('、') : null,
      state: m ? m.state : null,
    }
  }),
)

/** 按监听端口排序：读起来像「从公网往里走」的顺序。 */
const sortedChains = computed(() =>
  [...chains.value].sort((a, b) => portOf(a.site.listen) - portOf(b.site.listen)),
)

function portOf(addr: string): number {
  const i = String(addr || '').lastIndexOf(':')
  return i < 0 ? 0 : Number(addr.slice(i + 1)) || 0
}

/** 站点在本机监听的是 `0.0.0.0` / `::` 时，对外就是"所有地址"。 */
function isPublic(addr: string): boolean {
  const host = String(addr || '').slice(0, String(addr || '').lastIndexOf(':'))
  return host === '0.0.0.0' || host === '::' || host === '[::]'
}

const certNames = computed(() => (tls.value?.certs || []).map((c: any) => c.name).join('、') || '—')

const counters = computed(() => tls.value?.counters || {})

// 计数是累计值。存上一次采样算速率，比后端维护时间窗口简单，也不会因为
// 面板关着而"数据断了"。
const prev = ref<{ t: number; req: number; fail: number } | null>(null)
const rate = ref<number | null>(null)

function tickRate() {
  const now = Date.now()
  const req = Number(counters.value.requests_total || 0)
  const fail = Number(counters.value.handshake_failures || 0)
  if (prev.value) {
    const dt = (now - prev.value.t) / 1000
    if (dt > 0.5) {
      rate.value = Math.max(0, (req - prev.value.req) / dt)
      prev.value = { t: now, req, fail }
    }
  } else {
    prev.value = { t: now, req, fail }
  }
}

let timer: ReturnType<typeof setInterval> | undefined
onMounted(() => {
  load()
  timer = setInterval(() => {
    if (document.hidden) return
    // 编辑中不刷新：表单是打开那一刻的快照，刷新也不会同步过来，而只读视图
    // 在底下变了只会让"我看到的值"和"我要提交的值"分家。
    if (editing.value) return
    load(true)
    tickRate()
  }, 5000)
})
onBeforeUnmount(() => clearInterval(timer))
</script>

<template>
<div>
  <NeumorphismAlert v-if="error" type="error" title="无法读取 TLS 状态" :message="error" />

  <template v-if="tls">
    <!-- ── 顶部动作条 ──────────────────────────────────────────────────────
         编辑入口放最上面：这一页其余部分都是"现在长什么样"，改配置是唯一的
         写操作，得先让人找得到它。 -->
    <div class="esc-topbar">
      <span class="esc-hint">
        <template v-if="canEdit">
          证书路径、监听端口、上游与 301 都在这里改：保存后由 esc-host 的 TLS 层热切换，
          不用重启宿主。
        </template>
        <template v-else>
          当前宿主没有返回可编辑的配置（<code>status.config</code>），只能看不能改；
          升级 esc-host 之后这里才会出现输入框。
        </template>
      </span>
      <NeumorphismTag v-if="editing" variant="warning">编辑中 · 未保存</NeumorphismTag>
      <NeumorphismButton v-else size="small" variant="primary" :disabled="!canEdit" @click="startEdit">编辑配置</NeumorphismButton>
    </div>

    <!-- ── 编辑态 ────────────────────────────────────────────────────────
         用折叠区域而不是弹窗：这一页要看的东西（链路、计数）得有参照，弹窗一盖
         就只剩输入框；而且站点/证书是表状的一长列，弹窗里滚动反而更难对照。 -->
    <div v-if="editing" class="esc-editor">
      <!-- 保存失败的原因贴在最上面：放在底部动作条旁边时会被三张表挤到屏幕外，
           而"为什么没保存成功"是此刻唯一该看的东西。 -->
      <div v-if="formError" class="esc-form-error">
        <NeumorphismAlert type="error" title="配置没有生效（改动仍在下面，可以继续修）" :message="formError" :closable="false" />
      </div>

      <!-- 全局默认 -->
      <div class="esc-section-title"><h2>全局默认</h2></div>
      <NeumorphismCard>
        <div class="esc-form">
          <div class="esc-form-row">
            <NeumorphismSelect
              v-model="form.defaults.cert"
              label="默认证书（SNI 没命中时用这张）"
              :options="certOptions"
              placeholder="（证书表是空的）"
            />
            <NeumorphismInput v-model="form.defaults.max_body" label="请求体上限（字节）" inputmode="numeric" />
          </div>
          <div class="esc-form-row">
            <NeumorphismInput v-model="form.defaults.handshake" label="握手超时（秒）" inputmode="numeric" />
            <div class="esc-check"><NeumorphismSwitch v-model="form.defaults.tls12" />允许 TLS 1.2（关掉 = 只留 TLS 1.3）</div>
          </div>
          <div class="esc-check"><NeumorphismSwitch v-model="form.defaults.hsts" />默认发 HSTS（每个站点可以单独覆盖）</div>
        </div>
        <p class="esc-hint">
          请求体现在是 {{ bytes(form.defaults.max_body) }}；超了回 413，并计入「请求体超限」。
          握手超时挡 slowloris，单位是秒。
        </p>
      </NeumorphismCard>

      <!-- 站点 -->
      <div class="esc-section-title">
        <h2>站点</h2>
        <NeumorphismTag>{{ form.sites.length }}</NeumorphismTag>
        <span class="esc-spacer" />
        <NeumorphismButton size="small" variant="primary" @click="addSite">＋ 新增站点</NeumorphismButton>
      </div>
      <NeumorphismCard>
        <div v-if="!form.sites.length" class="esc-empty">
          没有站点：这一层现在只提供证书，不监听任何端口
        </div>
        <div v-else class="esc-edit-list">
          <div v-for="(s, i) in form.sites" :key="s._k" class="esc-edit-row">
            <div class="esc-edit-head">
              <NeumorphismTag size="small">{{ i + 1 }}</NeumorphismTag>
              <div class="esc-edit-name">
                <NeumorphismSelect
                  v-model="s.kind"
                  :options="kindOptions"
                  label="类型"
                  :size="36"
                  @change="onKindChange(s)"
                />
              </div>
              <span class="esc-spacer" />
              <NeumorphismButton size="small" class="esc-danger" @click="removeSite(i)">删除</NeumorphismButton>
            </div>
            <div class="esc-edit-grid">
              <NeumorphismInput
                v-model="s.listen"
                label="监听端口（逗号分隔）"
                placeholder="0.0.0.0:2222, 0.0.0.0:443"
                :size="36"
              />
              <NeumorphismInput
                v-if="s.kind === 'proxy'"
                v-model="s.upstream"
                label="转发到（IP:端口）"
                placeholder="127.0.0.1:18790"
                :size="36"
              />
              <NeumorphismInput
                v-else
                v-model="s.redirect"
                label="301 目标模板"
                placeholder="https://{hostname}:2224{uri}"
                :size="36"
              />
              <NeumorphismInput v-model="s.read_timeout" label="上游超时（秒）" inputmode="numeric" :size="36" />
              <NeumorphismInput
                v-model="s.max_body"
                label="请求体上限（字节）"
                :placeholder="bodyPlaceholder"
                inputmode="numeric"
                :size="36"
              />
              <NeumorphismSelect v-model="s.hsts" :options="triOptions" label="HSTS" :size="36" />
            </div>
            <div class="esc-edit-foot">
              <div class="esc-check">
                <NeumorphismSwitch v-model="s.websocket" :size="24" :disabled="s.kind === 'redirect'" />透传 WebSocket
              </div>
              <div class="esc-check">
                <NeumorphismSwitch v-model="s.log_query" :size="24" />把查询串写进访问日志
              </div>
              <span v-if="s.kind === 'redirect'" class="esc-hint">301 不升级连接，websocket 固定关闭</span>
            </div>
          </div>
        </div>
        <p class="esc-hint">
          301 模板的占位符：<code>{host}</code> 是<b>带端口</b>的 Host、<code>{hostname}</code> 去掉端口、
          <code>{uri}</code> 路径 + 查询串、<code>{path}</code> 只有路径。跳到<b>另一个端口</b>时必须用
          <code>{hostname}</code> —— <code>{host}</code> 自带端口，会拼出 <code>https://x:2223:2224/…</code>。
        </p>
        <p class="esc-hint">
          「请求体上限」与「HSTS」留空 / 选继承，表示跟着上面的全局默认走；填了就是把这一项钉死在
          这个端口上，以后改全局默认不会影响它。
        </p>
      </NeumorphismCard>

      <!-- 证书 -->
      <div class="esc-section-title">
        <h2>证书</h2>
        <NeumorphismTag>{{ form.certs.length }}</NeumorphismTag>
        <span class="esc-spacer" />
        <NeumorphismButton size="small" variant="primary" @click="addCert">＋ 新增证书</NeumorphismButton>
      </div>
      <NeumorphismCard>
        <div v-if="!form.certs.length" class="esc-empty">没有证书：TLS 握手会失败</div>
        <div v-else class="esc-edit-list">
          <div v-for="(c, i) in form.certs" :key="c._k" class="esc-edit-row">
            <div class="esc-edit-head">
              <NeumorphismTag size="small">{{ i + 1 }}</NeumorphismTag>
              <div class="esc-edit-name">
                <NeumorphismInput v-model="c.name" label="引用名" placeholder="self" :size="36" />
              </div>
              <span class="esc-spacer" />
              <NeumorphismButton size="small" class="esc-danger" @click="removeCert(i)">删除</NeumorphismButton>
            </div>
            <div class="esc-edit-grid">
              <NeumorphismInput v-model="c.cert" label="证书链（PEM，叶子在前）" placeholder="/etc/esc/pki/self.crt" :size="36" />
              <NeumorphismInput v-model="c.key" label="私钥（PEM）" placeholder="/etc/esc/pki/self.key" :size="36" />
              <NeumorphismInput
                v-model="c.sni"
                label="服务的 SNI（逗号分隔，留空 = 只作默认证书用）"
                placeholder="cloud.example.com"
                :size="36"
              />
            </div>
          </div>
        </div>
        <p class="esc-hint">
          路径是 esc-host 进程视角的绝对路径；保存时会立刻读盘，读不到就整份拒绝（旧配置继续跑）。
          引用名要唯一：默认证书和 SNI 都靠它互相指认。
        </p>
      </NeumorphismCard>

      <!-- 动作条 -->
      <NeumorphismCard>
        <div class="esc-actions">
          <NeumorphismButton variant="primary" :loading="saving" @click="save">保存</NeumorphismButton>
          <NeumorphismButton :disabled="saving" @click="cancel">取消</NeumorphismButton>
          <span class="esc-hint">保存后立即生效：上游与 301 改动不中断连接，改监听端口会重绑该端口</span>
        </div>
      </NeumorphismCard>
    </div>

    <!-- ── 概览 ─────────────────────────────────────────────────────────── -->
    <div class="esc-grid">
      <NeumorphismCard>
        <div class="esc-section-title">
          <h2>终结层</h2>
          <NeumorphismTag :variant="tls.sites?.length ? 'success' : 'error'">
            {{ tls.sites?.length || 0 }} 个端口
          </NeumorphismTag>
        </div>
        <dl class="esc-kv">
          <dt>协议</dt><dd>{{ tls.tls12 ? 'TLS 1.2 + 1.3' : '仅 TLS 1.3' }}</dd>
          <dt>证书</dt><dd>{{ certNames }}</dd>
          <dt>默认证书</dt><dd>{{ tls.default_cert || '—' }}</dd>
        </dl>
        <p class="esc-hint">
          证书按 SNI 选：名字命中 <code>[[tls.config.cert]].sni</code> 的用对应证书，
          其余落回默认那张。改动走上面的「编辑配置」，保存即生效。
        </p>
      </NeumorphismCard>

      <NeumorphismCard>
        <div class="esc-section-title"><h2>流量</h2></div>
        <div class="esc-metric">
          {{ counters.requests_total ?? 0 }}
          <small>个请求{{ rate === null ? '' : ` · ${rate.toFixed(1)}/s` }}</small>
        </div>
        <dl class="esc-kv">
          <dt>握手失败</dt><dd>{{ counters.handshake_failures ?? 0 }}</dd>
          <dt>上游错误</dt><dd>{{ counters.upstream_errors ?? 0 }}</dd>
          <dt>请求体超限</dt><dd>{{ counters.body_too_large ?? 0 }}</dd>
        </dl>
        <p class="esc-hint">
          握手失败零星的几条是端口扫描；<b>持续增长</b>才是问题（证书过期、端口被防火墙打断）。
        </p>
      </NeumorphismCard>
    </div>

    <!-- ── 链路 ─────────────────────────────────────────────────────────── -->
    <div class="esc-section-title">
      <h2>TLS 链路</h2>
      <NeumorphismTag>{{ sortedChains.length }}</NeumorphismTag>
      <span class="esc-spacer" />
      <NeumorphismButton size="small" @click="load(); tickRate()">刷新</NeumorphismButton>
    </div>

    <NeumorphismCard>
      <div v-if="!sortedChains.length" class="esc-empty">没有配置任何 TLS 站点</div>
      <div v-else class="esc-table-wrap">
        <table class="esc-table">
          <thead>
            <tr>
              <th>监听</th><th>站点</th><th>终结之后</th><th>隧道</th><th>链路终点</th><th>状态</th><th>选项</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="c in sortedChains" :key="c.site.name + c.site.listen">
              <td class="mono">
                <strong>{{ c.site.listen }}</strong>
                <div class="esc-hint">{{ isPublic(c.site.listen) ? '所有网卡' : '仅本机' }}</div>
              </td>
              <td>
                {{ c.site.name }}
                <div class="esc-hint">{{ c.site.kind === 'redirect' ? '301 跳转' : '转发' }}</div>
              </td>
              <td class="mono">
                {{ c.site.target }}
                <!--
                  这条箭头是本页的重点：TLS 与隧道从前是两个世界，
                  现在 target 就是 mapping.listen，指向同一条链路。
                -->
                <div v-if="c.mapping" class="esc-hint">↳ 就是这条映射的监听地址</div>
              </td>
              <td>
                <template v-if="c.mapping">
                  <strong>{{ c.mapping.name }}</strong>
                  <div class="esc-hint">{{ c.provider || '无提供者' }}</div>
                </template>
                <span v-else class="esc-hint">—（不经隧道）</span>
              </td>
              <td class="mono">
                <template v-if="c.endpoint">{{ c.endpoint }}</template>
                <span v-else class="esc-hint">本机服务</span>
              </td>
              <td>
                <NeumorphismTag
                  v-if="c.state"
                  size="small"
                  :variant="c.state === 'active' ? 'success' : c.state === 'error' ? 'error' : 'default'"
                >{{ c.state }}</NeumorphismTag>
                <NeumorphismTag v-else size="small">—</NeumorphismTag>
                <div v-if="c.mapping?.error" class="esc-hint">{{ c.mapping.error }}</div>
              </td>
              <td>
                <div class="esc-actions">
                  <NeumorphismTag v-if="c.site.websocket" size="small" variant="primary">WS</NeumorphismTag>
                  <NeumorphismTag v-if="c.site.hsts" size="small">HSTS</NeumorphismTag>
                  <!--
                    `log_query = false` 是承载令牌的端口用来保证凭据不落盘的设置。
                    它应该被看见，而不是藏在配置里 —— 所以这里明确标出来。
                  -->
                  <NeumorphismTag v-if="!c.site.log_query" size="small" variant="success">不记录查询串</NeumorphismTag>
                  <NeumorphismTag v-else size="small" variant="warning">记录查询串</NeumorphismTag>
                </div>
                <div class="esc-hint">
                  超时 {{ duration(c.site.read_timeout_secs) }} · 体上限 {{ bytes(c.site.max_body_bytes) }}
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </NeumorphismCard>

    <!-- ── 证书 ─────────────────────────────────────────────────────────── -->
    <div class="esc-section-title">
      <h2>证书</h2>
      <NeumorphismTag>{{ tls.certs?.length || 0 }}</NeumorphismTag>
    </div>
    <NeumorphismCard>
      <div class="esc-table-wrap">
        <table class="esc-table">
          <thead><tr><th>引用名</th><th>文件</th><th>服务的 SNI</th><th>角色</th></tr></thead>
          <tbody>
            <tr v-for="c in tls.certs || []" :key="c.name">
              <td><strong>{{ c.name }}</strong></td>
              <td class="mono">{{ c.cert }}</td>
              <td class="mono">
                <template v-if="c.sni?.length">{{ c.sni.join('、') }}</template>
                <span v-else class="esc-hint">（只作为默认证书）</span>
              </td>
              <td>
                <NeumorphismTag v-if="c.default" size="small" variant="primary">默认</NeumorphismTag>
                <span v-else class="esc-hint">—</span>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <p class="esc-hint">
        续期后把新证书复制到这里的路径，然后在「编辑配置」里保存一次（哪怕什么都没改）——
        保存会重新读盘并换掉进程里那张。具体命令见 <code>docs/TLS-MODULE.md</code>。
      </p>
    </NeumorphismCard>
  </template>
</div>
</template>

<style scoped>
/* 顶部动作条：一句话说明 + 编辑入口。 */
.esc-topbar { display: flex; flex-wrap: wrap; align-items: center; gap: 12px; margin: 0 0 16px; }
.esc-topbar .esc-hint { flex: 1 1 320px; }

/* 编辑区与下面那几块只读视图的边界：一条左侧色带 + 留白。
   新拟态的卡片本身没有明显的边界，不加这个的话"哪几块是能改的"要靠读标题。 */
.esc-editor {
  border-left: 3px solid var(--nm-primary-color, #2563eb);
  padding-left: 14px;
  margin-bottom: 8px;
}

/* 后端/自查的错误是逐条多行的，Alert 默认把换行折叠成空格 —— 那样"哪一条"
   就看不出来了。pre-line 保留换行、仍然自动折行。 */
.esc-form-error { margin-bottom: 14px; }
.esc-form-error :deep(.nm-alert__message) { white-space: pre-line; }

/* 一个站点/一张证书 = 一块。做成行块而不是九列表格：九个可编辑字段挤进一张表，
   每列不到 150px，输入框里连 `0.0.0.0:443` 都显示不全，也没地方放字段名。 */
.esc-edit-row { padding: 14px 0; border-top: 1px solid var(--nm-color-border); }
.esc-edit-row:first-child { border-top: 0; padding-top: 2px; }
.esc-edit-head { display: flex; flex-wrap: wrap; align-items: flex-end; gap: 10px; }
.esc-edit-name { flex: 1 1 200px; min-width: 160px; }
.esc-edit-kind { flex: 0 1 180px; min-width: 150px; }
.esc-edit-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
  gap: 10px 12px;
  margin-top: 10px;
}
.esc-edit-foot { display: flex; flex-wrap: wrap; align-items: center; gap: 8px 20px; margin-top: 10px; }
</style>
