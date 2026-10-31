<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import PluginTree from './PluginTree.vue'
import { api, bytes, duration, flash, flashKind, flashText, json, stateVariant, tabItems } from './ui'

const tab = ref('tunnel')
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
const plugins = ref<InstanceType<typeof PluginTree>>()
let timer: ReturnType<typeof setInterval> | undefined
async function loadTunnel(quiet = false) {
  try {
    const [s, m] = await Promise.all([api('/api/status'), api('/api/mappings')])
    status.value = s; mappings.value = m; error.value = ''
  } catch (e) { if (!quiet) error.value = (e as Error).message }
}
async function loadEdge() {
  try { edge.value = await api('/api/edge'); edgeUnavailable.value = '' }
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
    await json('POST', id ? `/api/mappings/${encodeURIComponent(id)}` : '/api/mappings', form.value)
    editing.value = false; flash('映射已保存'); await loadTunnel()
  } catch (e) { flash((e as Error).message, 'error') } finally { saving.value = false }
}
async function toggleMapping(m: any) {
  try { await json('POST', `/api/mappings/${encodeURIComponent(m.id)}/enabled`, { enabled: !m.enabled }); await loadTunnel() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function removeMapping(m: any) {
  if (!confirm(`确定删除映射「${m.name}」？`)) return
  try { await json('DELETE', `/api/mappings/${encodeURIComponent(m.id)}`); flash('映射已删除'); await loadTunnel() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function reload() {
  try { await json('POST', '/api/reload'); flash('已从磁盘重新加载映射表'); await loadTunnel() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function kick(id?: string) {
  if (id && !confirm(`确定踢掉 spoke「${id}」？`)) return
  try { await json('POST', '/api/spokes/kick', { id }); flash('已断开 spoke，客户端将自动重连'); await loadTunnel() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function loadExport() {
  exporting.value = true
  try { exportData.value = await api(`/api/export?role=${exportRole.value}`) }
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
    await json('POST', old ? `/api/edge/routes/${encodeURIComponent(old)}` : '/api/edge/routes', {
      prefix: edgeForm.value.prefix.trim(), port: Number(edgeForm.value.port),
    })
    editingEdge.value = false; edgeOriginalPrefix.value = ''; flash('边缘路由已保存'); await loadEdge()
  } catch (e) { flash((e as Error).message, 'error') } finally { savingEdge.value = false }
}
async function removeEdge(prefix: string) {
  if (!confirm(`确定删除边缘路由「${prefix}」？`)) return
  try { await json('DELETE', `/api/edge/routes/${encodeURIComponent(prefix)}`); flash('边缘路由已删除'); await loadEdge() }
  catch (e) { flash((e as Error).message, 'error') }
}
watch(exportRole, loadExport)
watch(tab, value => { if (value === 'plugins') plugins.value?.load() })
onMounted(() => { refresh(); timer = setInterval(() => { if (!editing.value && !editingEdge.value && tab.value === 'tunnel') refresh() }, 5000) })
onBeforeUnmount(() => clearInterval(timer))
</script>

<template>
  <div class="esc-app">
    <header class="esc-header">
      <h1>服务器核心</h1>
      <NeumorphismTag variant="primary">隧道与插件</NeumorphismTag>
      <span class="esc-spacer" />
      <NeumorphismButton size="small" @click="refresh">刷新</NeumorphismButton>
      <span class="subtitle">Rust 隧道内核 · ui-frame 管理界面</span>
    </header>
    <NeumorphismTabs v-model="tab" class="esc-tabs" :tabs="tabItems([['tunnel', '隧道管理'], ['plugins', '插件树']])" />
    <PluginTree v-show="tab === 'plugins'" ref="plugins" />
    <main v-show="tab === 'tunnel'">
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
    </main>

    <NeumorphismModal v-model="editing" :title="form.id ? '编辑映射' : '新建映射'" :footer="false">
      <div class="esc-form"><NeumorphismInput v-model="form.name" label="服务名" placeholder="hub 与 spoke 两侧一致" required /><div class="esc-form-row"><NeumorphismInput v-model="form.listen" label="公网监听（Hub）" placeholder="0.0.0.0:2222" /><NeumorphismInput v-model="form.local" label="本地目标（Spoke）" placeholder="127.0.0.1:8080" /></div><NeumorphismInput v-model="form.spoke" label="固定 Spoke（可选）" /><NeumorphismInput v-model="form.note" label="备注" /><div class="esc-check"><NeumorphismSwitch v-model="form.enabled" />启用此映射</div><div class="esc-actions"><NeumorphismButton variant="primary" :loading="saving" @click="saveMapping">保存</NeumorphismButton><NeumorphismButton @click="editing = false">取消</NeumorphismButton></div></div>
    </NeumorphismModal>
    <NeumorphismModal v-model="editingEdge" :title="edgeOriginalPrefix ? '编辑边缘路由' : '新建边缘路由'" :footer="false"><div class="esc-form"><NeumorphismInput v-model="edgeForm.prefix" label="域名前缀或完整域名" placeholder="cad 或 cad.example.com" required /><p class="esc-hint">前缀 <code>cad</code> 匹配 <code>cad.任意域名</code>；完整域名 <code>cad.example.com</code> 只匹配该域名，优先于前缀。不要填写协议、端口或路径。</p><NeumorphismInputNumber v-model="edgeForm.port" label="转发到本机端口" :min="1" :max="65535" /><div class="esc-actions"><NeumorphismButton variant="primary" :loading="savingEdge" @click="saveEdge">保存</NeumorphismButton><NeumorphismButton :disabled="savingEdge" @click="editingEdge = false">取消</NeumorphismButton></div></div></NeumorphismModal>
    <NeumorphismModal v-model="exporting" title="导出静态配置" :footer="false"><div class="esc-form"><NeumorphismSelect v-model="exportRole" label="角色" :options="[{label:'Hub',value:'hub'},{label:'Spoke',value:'spoke'}]" /><pre class="esc-code">{{ exportData?.toml || '读取中…' }}</pre><div class="esc-actions"><NeumorphismButton :disabled="!exportData" @click="downloadExport">下载 TOML</NeumorphismButton><NeumorphismButton @click="exporting = false">关闭</NeumorphismButton></div></div></NeumorphismModal>
    <NeumorphismAlert v-if="flashText" class="esc-flash" :type="flashKind" :message="flashText" />
  </div>
</template>
