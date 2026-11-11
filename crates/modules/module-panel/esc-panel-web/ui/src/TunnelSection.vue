<script setup lang="ts">

// 隧道管理：端口映射 / Spoke / 边缘路由。
//
// 从前这是一个独立页面（TunnelPanel.vue，跑在 esc-host 的 18787 端口上）。
// 现在它是壳子里的一个 section：没有自己的 header、没有自己的标签栏，
// API 路径统一走 `/api/tunnel/*`（由服务器面板转发到 127.0.0.1:18787）。
//
// 为什么把 API 集中转发而不是让浏览器直连 18787：那个端口只监听回环，而且
// **不做认证**——它靠"只有本机能连"来保命。把它暴露到浏览器等于把这道防线
// 拆掉。
//
// 导出配置那部分仍然要选角色（hub/spoke），因为导出的是一份完整的部署配置，
// 和当前进程的角色不一定一致。

import { onBeforeUnmount, onMounted, ref, watch } from 'vue'

import { api, bytes, duration, flash, flashKind, flashText, json, stateVariant } from './ui'
const status = ref<any>(null)
const mappings = ref<any[]>([])
const edge = ref<any>(null)
const edgeUnavailable = ref('')
const error = ref('')
const editing = ref(false)
const saving = ref(false)
const exporting = ref(false)
const exportRole = ref('hub')
const exportData = ref<any>(null)
const editingEdge = ref(false)
const form = ref<any>({})
const edgeForm = ref({ prefix: '', port: 2228 })
const edgeOriginalPrefix = ref('')
const savingEdge = ref(false)
let timer: ReturnType<typeof setInterval> | undefined
async function loadTunnel(quiet = false) {
  try {
    const [s, m] = await Promise.all([api('/api/tunnel/status'), api('/api/tunnel/mappings')])
    status.value = s; mappings.value = m; error.value = ''
  } catch (e) { if (!quiet) error.value = (e as Error).message }
}
async function loadEdge() {
  try { edge.value = await api('/api/tunnel/edge'); edgeUnavailable.value = '' }
  catch (e) { edge.value = null; edgeUnavailable.value = (e as Error).message }
}
async function refresh() { await Promise.all([loadTunnel(), loadEdge()]) }
function openMapping(m: any = null) {
  form.value = m ? { ...m.mapping } : { name: '', listen: '0.0.0.0:', local: '127.0.0.1:', spoke: '', enabled: true, note: '' }
  editing.value = true
}
async function saveMapping() {
  saving.value = true
  try {
    const id = form.value.id
    await json('POST', id ? `/api/tunnel/mappings/${encodeURIComponent(id)}` : '/api/tunnel/mappings', form.value)
    editing.value = false; flash('映射已保存'); await loadTunnel()
  } catch (e) { flash((e as Error).message, 'error') } finally { saving.value = false }
}
async function toggleMapping(m: any) {
  try { await json('POST', `/api/tunnel/mappings/${encodeURIComponent(m.id)}/enabled`, { enabled: !m.enabled }); await loadTunnel() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function removeMapping(m: any) {
  if (!confirm(`确定删除映射「${m.name}」？`)) return
  try { await json('DELETE', `/api/tunnel/mappings/${encodeURIComponent(m.id)}`); flash('映射已删除'); await loadTunnel() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function reload() {
  try { await json('POST', '/api/tunnel/reload'); flash('已从磁盘重新加载映射表'); await loadTunnel() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function kick(id?: string) {
  if (id && !confirm(`确定踢掉 spoke「${id}」？`)) return
  try { await json('POST', '/api/tunnel/spokes/kick', { id }); flash('已断开 spoke，客户端将自动重连'); await loadTunnel() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function loadExport() {
  exporting.value = true
  try { exportData.value = await api(`/api/tunnel/export?role=${exportRole.value}`) }
  catch (e) { flash((e as Error).message, 'error') } finally { exporting.value = false }
}
function downloadExport() {
  if (!exportData.value) return
  const url = URL.createObjectURL(new Blob([exportData.value.toml], { type: 'text/plain' }))
  const a = document.createElement('a'); a.href = url; a.download = exportData.value.filename; a.click()
  URL.revokeObjectURL(url)
}
function openEdge(route: any = null) {
  edgeOriginalPrefix.value = route?.prefix || ''
  edgeForm.value = { prefix: route?.prefix || '', port: route?.port || 2228 }
  editingEdge.value = true
}
function edgeAddress(prefix: string) {
  return prefix.includes('.') ? prefix : `${prefix}.<任意域名>`
}
async function saveEdge() {
  if (savingEdge.value) return
  savingEdge.value = true
  try {
    const old = edgeOriginalPrefix.value
    await json('POST', old ? `/api/tunnel/edge/routes/${encodeURIComponent(old)}` : '/api/tunnel/edge/routes', {
      prefix: edgeForm.value.prefix.trim(), port: Number(edgeForm.value.port),
    })
    editingEdge.value = false; edgeOriginalPrefix.value = ''; flash('边缘路由已保存'); await loadEdge()
  } catch (e) { flash((e as Error).message, 'error') } finally { savingEdge.value = false }
}
async function removeEdge(prefix: string) {
  if (!confirm(`确定删除边缘路由「${prefix}」？`)) return
  try { await json('DELETE', `/api/tunnel/edge/routes/${encodeURIComponent(prefix)}`); flash('边缘路由已删除'); await loadEdge() }
  catch (e) { flash((e as Error).message, 'error') }
}
watch(exportRole, loadExport)
onMounted(() => {
  refresh()
  // 弹性轮询：有对话框打开时让路（它会与用户正在改的数据打架）。
  // 不再看 tab —— 外壳用 v-show 挂着，切走只是隐藏，轮询继续也几乎没有成本
  // （两个 JSON 端点、5 秒一次），而"切回来看到的是旧数据"更让人困惑。
  timer = setInterval(() => { if (!editing.value && !editingEdge.value) refresh() }, 5000)
})
onBeforeUnmount(() => clearInterval(timer))
</script>

<template>
<div>
      <NeumorphismAlert v-if="error" type="error" title="无法读取隧道状态" :message="error" />
      <div class="esc-grid">
        <NeumorphismCard>
          <div class="esc-section-title"><h2>引擎状态</h2><NeumorphismTag :variant="status?.running ? 'success' : 'error'">{{ status?.running ? '运行中' : '不可用' }}</NeumorphismTag></div>
          <dl class="esc-kv">
            <dt>角色</dt><dd>{{ status?.role === 'hub' ? '公网 Hub' : status?.role === 'spoke' ? '内网 Spoke' : '—' }}</dd>
            <dt>版本</dt><dd>v{{ status?.version || '—' }}</dd>
            <dt>运行时长</dt><dd>{{ duration(status?.uptime_secs) }}</dd>
            <dt>控制监听</dt><dd class="mono">{{ status?.control_listen || '—' }}</dd>
          </dl>
        </NeumorphismCard>
        <NeumorphismCard>
          <div class="esc-section-title"><h2>连接与映射</h2></div>
          <div class="esc-metric">{{ mappings.filter(m => m.state === 'active').length }} <small>/ {{ mappings.length }} 条映射活跃</small></div>
          <dl class="esc-kv"><dt>已连接 Spoke</dt><dd>{{ status?.spokes?.length || 0 }}</dd><dt>Hub 连接</dt><dd>{{ status?.hub_connected === undefined ? '—' : status.hub_connected ? '已连接' : '未连接' }}</dd></dl>
        </NeumorphismCard>
      </div>

      <div class="esc-section-title">
        <h2>端口映射</h2><NeumorphismTag>{{ mappings.length }}</NeumorphismTag><span class="esc-spacer" />
        <NeumorphismButton size="small" @click="reload">重新加载</NeumorphismButton>
        <NeumorphismButton size="small" @click="exporting = true; loadExport()">导出配置</NeumorphismButton>
        <NeumorphismButton size="small" variant="primary" @click="openMapping()">＋ 新建映射</NeumorphismButton>
      </div>
      <NeumorphismCard>
        <div v-if="!mappings.length" class="esc-empty">还没有端口映射</div>
        <div v-else class="esc-table-wrap"><table class="esc-table"><thead><tr><th>服务名</th><th>公网监听</th><th>转发到</th><th>状态</th><th>提供者</th><th>连接 / 流量</th><th>操作</th></tr></thead><tbody>
          <tr v-for="m in mappings" :key="m.id">
            <td><strong>{{ m.name }}</strong><div class="esc-hint">{{ m.note }}</div></td>
            <td class="mono">{{ m.listen || '—' }}</td><td class="mono">{{ m.local || '—' }}</td>
            <td><NeumorphismTag size="small" :variant="stateVariant(m.state)">{{ m.state }}</NeumorphismTag><div v-if="m.error" class="esc-hint">{{ m.error }}</div></td>
            <td class="mono">{{ (m.providers || []).join(', ') || '—' }}</td>
            <td class="mono">{{ m.stats?.active_connections ?? 0 }} / {{ m.stats?.total_connections ?? 0 }}<div class="esc-hint">↓{{ bytes(m.stats?.bytes_down) }} ↑{{ bytes(m.stats?.bytes_up) }}</div></td>
            <td><div class="esc-actions"><NeumorphismButton size="small" @click="toggleMapping(m)">{{ m.enabled ? '停用' : '启用' }}</NeumorphismButton><NeumorphismButton size="small" @click="openMapping(m)">编辑</NeumorphismButton><NeumorphismButton size="small" @click="removeMapping(m)">删除</NeumorphismButton></div></td>
          </tr>
        </tbody></table></div>
      </NeumorphismCard>

      <div class="esc-section-title"><h2>边缘路由</h2><NeumorphismTag v-if="edge" variant="success">已启用</NeumorphismTag><span class="esc-spacer" /><NeumorphismButton v-if="edge" size="small" variant="primary" @click="openEdge()">＋ 新建路由</NeumorphismButton></div>
      <NeumorphismAlert v-if="edgeUnavailable" type="info" title="边缘路由未启用" :message="edgeUnavailable" />
      <NeumorphismCard v-if="edge">
        <p class="esc-hint">支持域名前缀或完整域名，完整域名优先。监听：{{ (edge.listens || []).join('、') }}。改动自动持久化，重启后仍有效。</p>
        <div v-if="!edge.routes?.length" class="esc-empty">还没有边缘路由</div>
        <div v-else class="esc-table-wrap"><table class="esc-table"><thead><tr><th>匹配类型</th><th>域名 / 前缀</th><th>匹配示例</th><th>转发到</th><th>操作</th></tr></thead><tbody><tr v-for="r in edge.routes" :key="r.prefix"><td><NeumorphismTag size="small">{{ r.prefix.includes('.') ? '完整域名' : '前缀' }}</NeumorphismTag></td><td class="mono">{{ r.prefix }}</td><td class="mono">{{ edgeAddress(r.prefix) }}</td><td class="mono">127.0.0.1:{{ r.port }}</td><td><div class="esc-actions"><NeumorphismButton size="small" @click="openEdge(r)">编辑</NeumorphismButton><NeumorphismButton size="small" @click="removeEdge(r.prefix)">删除</NeumorphismButton></div></td></tr></tbody></table></div>
        <dl class="esc-kv"><dt>请求 / 代理</dt><dd>{{ edge.counters?.requests || 0 }} / {{ edge.counters?.proxied || 0 }}</dd><dt>未路由 / 上游错误</dt><dd>{{ edge.counters?.unrouted || 0 }} / {{ edge.counters?.upstream_errors || 0 }}</dd><dt>Upgrade</dt><dd>{{ edge.counters?.upgraded || 0 }}</dd></dl>
      </NeumorphismCard>

      <template v-if="status?.role === 'hub'">
        <div class="esc-section-title"><h2>已连接的 Spoke</h2></div>
        <NeumorphismCard><div v-if="!status.spokes?.length" class="esc-empty">暂无 Spoke 连接</div><div v-else class="esc-table-wrap"><table class="esc-table"><thead><tr><th>ID</th><th>对端地址</th><th>版本</th><th>已授权服务</th><th>在线时长</th><th></th></tr></thead><tbody><tr v-for="s in status.spokes" :key="s.id"><td class="mono">{{ s.id }}</td><td class="mono">{{ s.remote_addr }}</td><td>{{ s.version }}</td><td>{{ (s.services || []).join('、') }}</td><td>{{ duration(s.connected_secs) }}</td><td><NeumorphismButton size="small" @click="kick(s.id)">踢下线</NeumorphismButton></td></tr></tbody></table></div></NeumorphismCard>
      </template>
    </div></template>
