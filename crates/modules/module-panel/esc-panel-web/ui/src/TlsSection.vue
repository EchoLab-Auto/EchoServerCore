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
// # 为什么是只读
//
// 证书与监听端口的改动要重启宿主才生效（见 docs/TLS-MODULE.md）。给一套只能
// 改内存、活不过重启的输入框，比没有更误导。

import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

import { api, bytes, duration, flash } from './ui'

const tls = ref<any>(null)
const mappings = ref<any[]>([])
const error = ref('')
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

// ── 派生视图 ────────────────────────────────────────────────────────────────

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
          其余落回默认那张。改动要重启 esc-host 才生效。
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
        续期后要把新证书**复制到这里的路径**再重启 esc-host —— 本模块不做运行期热加载，
        「证书什么时候换的」必须可追溯。具体命令见 <code>docs/TLS-MODULE.md</code>。
      </p>
    </NeumorphismCard>
  </template>
</div>
</template>
