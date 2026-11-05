<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import Networks from './Networks.vue'
import PluginTree from './PluginTree.vue'
import { api, bytes, duration, flash, flashKind, flashText, json, stateVariant, tabItems } from './ui'

const tab = ref('overview')
const overview = ref<any>(null)
const containers = ref<any[]>([])
const images = ref<any[]>([])
const jobs = ref<any[]>([])
const networks = ref<string[]>([])
const filter = ref('')
const error = ref('')
const plugins = ref<InstanceType<typeof PluginTree>>()
const networksPanel = ref<InstanceType<typeof Networks>>()
const creating = ref(false)
const pulling = ref(false)
const pullImage = ref('')
const renameFor = ref<any>(null)
const renameValue = ref('')
const restartFor = ref<any>(null)
const restartValue = ref('unless-stopped')
const logsFor = ref<any>(null)
const logs = ref('')
const detailsFor = ref<any>(null)
const details = ref<any>(null)
const createForm = ref<any>({})
let timer: ReturnType<typeof setInterval> | undefined
let jobTimer: ReturnType<typeof setInterval> | undefined

const filteredContainers = computed(() => {
  const q = filter.value.trim().toLowerCase()
  return !q ? containers.value : containers.value.filter(c => [c.name, c.image, c.compose_project].some(x => String(x || '').toLowerCase().includes(q)))
})
const groupedContainers = computed(() => {
  const groups = new Map<string, any[]>()
  for (const c of filteredContainers.value) {
    const name = c.compose_project || '独立容器'
    groups.set(name, [...(groups.get(name) || []), c])
  }
  return [...groups.entries()]
})
const allowActions = computed(() => !!overview.value?.allow_actions)
const allowCreate = computed(() => !!overview.value?.allow_create)
const allowDelete = computed(() => !!overview.value?.allow_delete)

async function loadOverview() { try { overview.value = await api('/api/overview'); error.value = '' } catch (e) { error.value = (e as Error).message } }
async function loadContainers() { try { containers.value = (await api('/api/docker/containers')).containers || [] } catch (e) { flash((e as Error).message, 'error') } }
async function loadImages() { try { images.value = (await api('/api/docker/images')).images || [] } catch (e) { flash((e as Error).message, 'error') } }
async function loadJobs() { try { jobs.value = (await api('/api/docker/jobs')).jobs || [] } catch { /* Docker 不可用时由 banner 说明 */ } }
function pct(n: unknown) { return Number.isFinite(Number(n)) ? Number(n).toFixed(1) + '%' : '—' }
function rate(n: unknown) { return bytes(n) + '/s' }
function progressVariant(n: unknown) { return Number(n) >= 90 ? 'error' : Number(n) >= 75 ? 'warning' : 'primary' }
function since(ts: unknown) { const n = Number(ts); return n ? new Date(n * 1000).toLocaleString() : '—' }
function ports(c: any) { return (c.ports || []).map((p: any) => `${p.ip ? p.ip + ':' : ''}${p.public || ''}→${p.private}/${p.kind}`).join('，') || '—' }

async function containerAction(c: any, action: string) {
  try { await json('POST', `/api/docker/containers/${encodeURIComponent(c.id)}/${action}`); flash(`容器 ${c.name}：${action} 已执行`); await Promise.all([loadContainers(), loadOverview()]) }
  catch (e) { flash((e as Error).message, 'error') }
}
async function removeContainer(c: any) {
  if (!confirm(`确定删除容器「${c.name}」？运行中的容器会先被强制停止，操作不可撤销。`)) return
  try { await json('DELETE', `/api/docker/containers/${encodeURIComponent(c.id)}`); flash('容器已删除'); await loadContainers() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function groupAction(group: any[], action: string) {
  for (const c of group) await containerAction(c, action)
}
async function openLogs(c: any) {
  logsFor.value = c; logs.value = '读取中…'
  try { logs.value = (await api(`/api/docker/containers/${encodeURIComponent(c.id)}/logs?tail=300`)).text }
  catch (e) { logs.value = (e as Error).message }
}
async function openDetails(c: any) {
  detailsFor.value = c; details.value = null
  try { details.value = await api(`/api/docker/containers/${encodeURIComponent(c.id)}/detail`) }
  catch (e) { flash((e as Error).message, 'error') }
}
async function renameContainer() {
  try { await json('POST', `/api/docker/containers/${encodeURIComponent(renameFor.value.id)}/rename`, { name: renameValue.value }); renameFor.value = null; flash('容器已改名'); await loadContainers() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function setRestart() {
  try { await json('POST', `/api/docker/containers/${encodeURIComponent(restartFor.value.id)}/restart-policy`, { policy: restartValue.value }); restartFor.value = null; flash('重启策略已修改') }
  catch (e) { flash((e as Error).message, 'error') }
}
async function openCreate() {
  createForm.value = { image: 'nginx:alpine', name: '', cmd: '', env: '', ports: '', binds: '', restart: 'unless-stopped', network: '', auto_start: true }
  creating.value = true
  try { networks.value = (await api('/api/docker/networks')).names || [] } catch { networks.value = [] }
}
async function createContainer() {
  const f = createForm.value
  const lines = (v: string) => v.split('\n').map(x => x.trim()).filter(Boolean)
  try {
    await json('POST', '/api/docker/containers', { image: f.image, name: f.name || null, cmd: f.cmd.trim() ? f.cmd.trim().split(/\s+/) : [], env: lines(f.env), ports: lines(f.ports), binds: lines(f.binds), restart: f.restart, network: f.network || null, auto_start: f.auto_start })
    creating.value = false; flash('容器已创建'); await loadContainers()
  } catch (e) { flash((e as Error).message, 'error') }
}
async function pull() {
  try { await json('POST', '/api/docker/images/pull', { image: pullImage.value.trim() }); pulling.value = false; flash('拉取任务已开始，可在下方查看进度'); await loadJobs() }
  catch (e) { flash((e as Error).message, 'error') }
}
async function removeImage(name: string) {
  if (!confirm(`确定删除镜像「${name}」？此操作不可撤销。`)) return
  try { await json('DELETE', `/api/docker/images/${encodeURIComponent(name)}`); flash('镜像已删除'); await loadImages() }
  catch (e) { flash((e as Error).message, 'error') }
}
watch(tab, value => {
  if (value === 'containers') loadContainers()
  if (value === 'images') { loadImages(); loadJobs() }
  if (value === 'networks') networksPanel.value?.load()
  if (value === 'plugins') plugins.value?.load()
})
onMounted(() => {
  loadOverview()
  timer = setInterval(() => { if (tab.value === 'overview') loadOverview() }, 5000)
  jobTimer = setInterval(() => { if (tab.value === 'images' && jobs.value.some(j => j.state === 'running')) loadJobs() }, 2000)
})
onBeforeUnmount(() => { clearInterval(timer); clearInterval(jobTimer) })
</script>

<template>
  <div class="esc-app">
    <header class="esc-header"><h1>服务器面板</h1><NeumorphismTag v-if="overview" :variant="overview.docker?.state === 'ok' ? 'success' : 'warning'">Docker {{ overview.docker?.state }}</NeumorphismTag><NeumorphismTag v-if="overview && !allowActions && !allowCreate && !allowDelete">只读</NeumorphismTag><span class="esc-spacer" /><NeumorphismButton size="small" @click="loadOverview">刷新</NeumorphismButton><span class="subtitle">主机指标与 Docker 管理 · ui-frame</span></header>
    <NeumorphismTabs v-model="tab" class="esc-tabs" :tabs="tabItems([['overview','主机概览'],['containers','容器'],['images','镜像'],['networks','网络'],['plugins','插件树']])" />
    <NeumorphismAlert v-if="error" type="error" title="面板连接失败" :message="error" />
    <Networks
      v-show="tab === 'networks'"
      ref="networksPanel"
      :allow-create="allowCreate"
      :allow-actions="allowActions"
      :allow-delete="allowDelete"
    />
    <PluginTree v-show="tab === 'plugins'" ref="plugins" />

    <main v-if="overview" v-show="tab === 'overview'">
      <div class="esc-grid">
        <NeumorphismCard><div class="esc-section-title"><h2>CPU</h2><NeumorphismTag>{{ overview.cpu?.total_ticks || overview.host?.cpu_count }} 核</NeumorphismTag></div><div class="esc-metric">{{ pct(overview.cpu?.usage_pct) }}</div><NeumorphismProgress :model-value="overview.cpu?.usage_pct || 0" :variant="progressVariant(overview.cpu?.usage_pct)" /><dl class="esc-kv"><dt>负载 1/5/15</dt><dd>{{ overview.cpu?.load1?.toFixed(2) }} / {{ overview.cpu?.load5?.toFixed(2) }} / {{ overview.cpu?.load15?.toFixed(2) }}</dd><dt>每核使用率</dt><dd>{{ (overview.cpu?.per_core || []).map((n: number) => n.toFixed(0) + '%').join(' · ') }}</dd></dl></NeumorphismCard>
        <NeumorphismCard><div class="esc-section-title"><h2>内存</h2><NeumorphismTag>{{ bytes(overview.mem?.total_bytes) }}</NeumorphismTag></div><div class="esc-metric">{{ pct(overview.mem?.usage_pct) }}</div><NeumorphismProgress :model-value="overview.mem?.usage_pct || 0" :variant="progressVariant(overview.mem?.usage_pct)" /><dl class="esc-kv"><dt>已用 / 可用</dt><dd>{{ bytes(overview.mem?.used_bytes) }} / {{ bytes(overview.mem?.available_bytes) }}</dd><dt>缓存 / 缓冲</dt><dd>{{ bytes(overview.mem?.cached_bytes) }} / {{ bytes(overview.mem?.buffers_bytes) }}</dd><dt>Swap</dt><dd>{{ overview.mem?.swap_total_bytes ? bytes(overview.mem.swap_used_bytes) + ' / ' + bytes(overview.mem.swap_total_bytes) : '未启用' }}</dd></dl></NeumorphismCard>
        <NeumorphismCard><div class="esc-section-title"><h2>网络</h2></div><div class="esc-metric"><small>↓</small> {{ rate(overview.net?.rx_per_sec) }}<br><small>↑</small> {{ rate(overview.net?.tx_per_sec) }}</div><dl class="esc-kv"><dt>累计收 / 发</dt><dd>{{ bytes(overview.net?.rx_total_bytes) }} / {{ bytes(overview.net?.tx_total_bytes) }}</dd><template v-for="n in overview.net?.interfaces || []" :key="n.name"><dt>{{ n.name }}</dt><dd>↓{{ rate(n.rx_per_sec) }} ↑{{ rate(n.tx_per_sec) }}</dd></template></dl></NeumorphismCard>
        <NeumorphismCard><div class="esc-section-title"><h2>系统</h2><NeumorphismTag>{{ overview.in_container ? '容器内' : '物理/虚机' }}</NeumorphismTag></div><dl class="esc-kv"><dt>主机名</dt><dd>{{ overview.host?.hostname }}</dd><dt>操作系统</dt><dd>{{ overview.host?.os }}</dd><dt>内核</dt><dd>{{ overview.host?.kernel }}</dd><dt>架构</dt><dd>{{ overview.host?.arch }}</dd><dt>运行时长</dt><dd>{{ duration(overview.host?.uptime_secs) }}</dd><dt>面板版本</dt><dd>v{{ overview.version }}</dd></dl></NeumorphismCard>
      </div>
      <div class="esc-section-title"><h2>磁盘</h2><NeumorphismTag>{{ overview.disks?.length || 0 }} 个挂载</NeumorphismTag></div>
      <NeumorphismCard><div class="esc-table-wrap"><table class="esc-table"><thead><tr><th>挂载点</th><th>设备 / 类型</th><th>使用率</th><th>已用 / 总量</th></tr></thead><tbody><tr v-for="d in overview.disks" :key="d.mount"><td class="mono">{{ d.mount }}</td><td class="mono">{{ d.device }} · {{ d.fs }}</td><td><NeumorphismProgress :model-value="d.usage_pct" :variant="progressVariant(d.usage_pct)" /></td><td class="mono">{{ bytes(d.used_bytes) }} / {{ bytes(d.total_bytes) }}</td></tr></tbody></table></div></NeumorphismCard>
      <div class="esc-section-title"><h2>进程 Top 20</h2><span class="esc-hint">按 CPU 降序</span></div>
      <NeumorphismCard><div class="esc-table-wrap"><table class="esc-table"><thead><tr><th>PID</th><th>名称</th><th>CPU</th><th>内存</th><th>线程</th><th>用户</th><th>命令行</th></tr></thead><tbody><tr v-for="p in overview.procs" :key="p.pid"><td class="mono">{{ p.pid }}</td><td>{{ p.name }}</td><td>{{ pct(p.cpu_pct) }}</td><td>{{ bytes(p.rss_bytes) }}</td><td>{{ p.threads }}</td><td>{{ p.user }}</td><td class="mono">{{ p.cmdline || '[' + p.name + ']' }}</td></tr></tbody></table></div></NeumorphismCard>
    </main>

    <main v-show="tab === 'containers'">
      <div class="esc-section-title"><h2>Docker 容器</h2><NeumorphismTag>{{ containers.length }}</NeumorphismTag><span class="esc-spacer" /><NeumorphismButton v-if="allowCreate" size="small" variant="primary" @click="openCreate">＋ 创建容器</NeumorphismButton><NeumorphismButton size="small" @click="loadContainers">刷新</NeumorphismButton></div>
      <NeumorphismAlert v-if="overview?.docker?.state !== 'ok'" type="warning" title="Docker 不可用" :message="overview?.docker?.message || '未检测到 Docker socket'" />
      <NeumorphismCard><div class="esc-filter"><NeumorphismInput v-model="filter" placeholder="过滤名称 / 镜像 / Compose 项目" /></div>
        <div v-if="!filteredContainers.length" class="esc-empty">没有匹配的容器</div>
        <template v-for="[project, group] in groupedContainers" :key="project">
          <div class="esc-section-title"><h2>{{ project }}</h2><NeumorphismTag size="small">{{ group.length }}</NeumorphismTag><span v-if="project !== '独立容器'" class="esc-spacer" /><div v-if="project !== '独立容器' && allowActions" class="esc-actions"><NeumorphismButton size="small" @click="groupAction(group, 'start')">整栈启动</NeumorphismButton><NeumorphismButton size="small" @click="groupAction(group, 'stop')">整栈停止</NeumorphismButton></div></div>
          <div v-for="c in group" :key="c.id" class="esc-container"><div class="esc-container-head"><NeumorphismTag :variant="stateVariant(c.state)" size="small">{{ c.state }}</NeumorphismTag><span class="esc-container-title">{{ c.name }}</span><span class="mono esc-hint">{{ c.id.slice(0,12) }}</span></div><div class="esc-container-meta">{{ c.image }} · {{ c.status }}<br>{{ ports(c) }}</div><div class="esc-actions"><template v-if="allowActions"><NeumorphismButton v-if="c.state !== 'running'" size="small" @click="containerAction(c,'start')">启动</NeumorphismButton><NeumorphismButton v-else size="small" @click="containerAction(c,'stop')">停止</NeumorphismButton><NeumorphismButton size="small" @click="containerAction(c,'restart')">重启</NeumorphismButton><NeumorphismButton size="small" @click="containerAction(c,c.state === 'paused' ? 'unpause' : 'pause')">{{ c.state === 'paused' ? '恢复' : '暂停' }}</NeumorphismButton><NeumorphismButton size="small" @click="renameFor=c;renameValue=c.name">改名</NeumorphismButton><NeumorphismButton size="small" @click="restartFor=c;restartValue='unless-stopped'">重启策略</NeumorphismButton></template><NeumorphismButton size="small" @click="openLogs(c)">日志</NeumorphismButton><NeumorphismButton size="small" @click="openDetails(c)">详情</NeumorphismButton><NeumorphismButton v-if="allowDelete" size="small" @click="removeContainer(c)">删除</NeumorphismButton></div></div>
        </template>
      </NeumorphismCard>
    </main>

    <main v-show="tab === 'images'">
      <div class="esc-section-title"><h2>Docker 镜像</h2><NeumorphismTag>{{ images.length }}</NeumorphismTag><span class="esc-spacer" /><NeumorphismButton v-if="allowCreate" size="small" variant="primary" @click="pulling=true">＋ 拉取镜像</NeumorphismButton><NeumorphismButton size="small" @click="loadImages">刷新</NeumorphismButton></div>
      <NeumorphismCard v-if="jobs.length"><div class="esc-section-title"><h2>后台任务</h2></div><div v-for="j in jobs" :key="j.id" class="esc-container"><div class="esc-container-head"><span>{{ j.kind }}</span><NeumorphismTag :variant="j.state === 'done' ? 'success' : j.state === 'failed' ? 'error' : 'warning'">{{ j.state }}</NeumorphismTag><span class="mono esc-hint">{{ j.id }}</span></div><div v-if="j.message" class="esc-hint">{{ j.message }}</div></div></NeumorphismCard>
      <NeumorphismCard><div v-if="!images.length" class="esc-empty">没有镜像</div><div v-else class="esc-table-wrap"><table class="esc-table"><thead><tr><th>标签</th><th>ID</th><th>大小</th><th>创建时间</th><th v-if="allowDelete"></th></tr></thead><tbody><tr v-for="im in images" :key="im.id"><td>{{ (im.tags || []).join('、') || '&lt;none&gt;' }}</td><td class="mono">{{ im.id }}</td><td>{{ bytes(im.size) }}</td><td>{{ since(im.created) }}</td><td v-if="allowDelete"><NeumorphismButton size="small" @click="removeImage(im.tags?.[0] || im.id)">删除</NeumorphismButton></td></tr></tbody></table></div></NeumorphismCard>
    </main>

    <NeumorphismModal v-model="creating" title="创建容器" :size="720" :footer="false"><div class="esc-form"><div class="esc-form-row"><NeumorphismInput v-model="createForm.image" label="镜像" required /><NeumorphismInput v-model="createForm.name" label="容器名（可选）" /></div><NeumorphismInput v-model="createForm.cmd" label="覆盖 CMD（可选）" /><div class="esc-form-row"><NeumorphismSelect v-model="createForm.restart" label="重启策略" :options="['no','always','unless-stopped','on-failure:3'].map(value=>({value,label:value}))" /><NeumorphismSelect v-model="createForm.network" label="网络" :options="networks.map(value=>({value,label:value}))" clearable /></div><NeumorphismTextarea v-model="createForm.env" label="环境变量（每行 KEY=VALUE）" :rows="3" /><NeumorphismTextarea v-model="createForm.ports" label="端口（每行 8080:80）" :rows="3" /><NeumorphismTextarea v-model="createForm.binds" label="挂载（每行 /host:/container[:ro]）" :rows="3" /><div class="esc-check"><NeumorphismSwitch v-model="createForm.auto_start" />创建后立即启动</div><div class="esc-actions"><NeumorphismButton variant="primary" @click="createContainer">创建</NeumorphismButton><NeumorphismButton @click="creating=false">取消</NeumorphismButton></div></div></NeumorphismModal>
    <NeumorphismModal v-model="pulling" title="拉取镜像" :footer="false"><div class="esc-form"><NeumorphismInput v-model="pullImage" label="镜像" placeholder="nginx:alpine" /><div class="esc-actions"><NeumorphismButton variant="primary" @click="pull">开始拉取</NeumorphismButton><NeumorphismButton @click="pulling=false">取消</NeumorphismButton></div></div></NeumorphismModal>
    <NeumorphismModal :model-value="!!renameFor" title="修改容器名" :footer="false" @update:model-value="renameFor=null"><div class="esc-form"><NeumorphismInput v-model="renameValue" label="新名称" /><div class="esc-actions"><NeumorphismButton variant="primary" @click="renameContainer">保存</NeumorphismButton><NeumorphismButton @click="renameFor=null">取消</NeumorphismButton></div></div></NeumorphismModal>
    <NeumorphismModal :model-value="!!restartFor" title="修改重启策略" :footer="false" @update:model-value="restartFor=null"><div class="esc-form"><NeumorphismSelect v-model="restartValue" label="策略" :options="['no','always','unless-stopped','on-failure:3'].map(value=>({value,label:value}))" /><div class="esc-actions"><NeumorphismButton variant="primary" @click="setRestart">保存</NeumorphismButton><NeumorphismButton @click="restartFor=null">取消</NeumorphismButton></div></div></NeumorphismModal>
    <NeumorphismModal :model-value="!!logsFor" :title="`容器日志 · ${logsFor?.name || ''}`" :size="900" :footer="false" @update:model-value="logsFor=null"><pre class="esc-code">{{ logs }}</pre></NeumorphismModal>
    <NeumorphismModal :model-value="!!detailsFor" :title="`容器详情 · ${detailsFor?.name || ''}`" :size="900" :footer="false" @update:model-value="detailsFor=null"><pre class="esc-code">{{ details ? JSON.stringify(details,null,2) : '读取中…' }}</pre></NeumorphismModal>
    <NeumorphismAlert v-if="flashText" class="esc-flash" :type="flashKind" :message="flashText" />
  </div>
</template>
