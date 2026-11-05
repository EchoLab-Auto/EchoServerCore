<script setup lang="ts">
import { computed, ref } from 'vue'
import { api, flash, json } from './ui'

// 权限由父组件一起传下来：三档闸的开关在 /api/overview 里，这里只管照做。
const props = defineProps<{
  allowCreate: boolean
  allowActions: boolean
  allowDelete: boolean
}>()

const networks = ref<any[]>([])
const loading = ref(false)
const filter = ref('')

const detailFor = ref<any>(null)
const detail = ref<any>(null)
const detailLoading = ref(false)
// 断开一个正在运行的容器时 Docker 默认拒绝（容器会彻底没网），
// 所以这个开关必须由人手动打开，不能默认 true。
const forceDisconnect = ref(false)

const creating = ref(false)
const submitting = ref(false)
const createForm = ref<any>({})

const deleting = ref<any>(null)
const deleteConfirm = ref('')

const connecting = ref<any>(null)
const containers = ref<any[]>([])

const filtered = computed(() => {
  const q = filter.value.trim().toLowerCase()
  if (!q) return networks.value
  return networks.value.filter(n =>
    [n.name, n.driver, n.id, ...(n.subnets || []).map((s: any) => s.subnet)]
      .some(x => String(x || '').toLowerCase().includes(q)))
})

// 分组顺序就是运维关心的顺序：内部网络最容易被误解，放最上面；
// 系统网络基本只看不动，放最后。
const groups = computed(() => [
  {
    key: 'internal',
    title: '内部网络',
    hint: '没有外网出口，端口映射不生效',
    items: filtered.value.filter(n => n.internal),
  },
  {
    key: 'custom',
    title: '自定义网络',
    hint: '',
    items: filtered.value.filter(n => !n.internal && !n.system),
  },
  {
    key: 'system',
    title: '系统网络',
    hint: 'Docker 自带，不可删除',
    items: filtered.value.filter(n => n.system),
  },
].filter(g => g.items.length))

function when(value: unknown) {
  const s = String(value || '')
  if (!s) return '—'
  const d = new Date(s)
  return Number.isNaN(d.getTime()) ? s : d.toLocaleString()
}

function subnets(n: any) {
  return (n.subnets || []).map((s: any) => s.subnet).join('，') || '自动'
}

async function load() {
  loading.value = true
  try {
    networks.value = (await api('/api/docker/networks')).networks || []
  } catch (e) {
    flash((e as Error).message, 'error')
  } finally {
    loading.value = false
  }
}

async function openDetail(n: any) {
  detailFor.value = n
  detail.value = null
  forceDisconnect.value = false
  detailLoading.value = true
  try {
    detail.value = await api(`/api/docker/networks/${encodeURIComponent(n.id)}`)
  } catch (e) {
    flash((e as Error).message, 'error')
  } finally {
    detailLoading.value = false
  }
}

/// 接入/断开之后，列表和详情都要重取：成员变了，列表上的标记也可能变。
async function reloadAfterChange() {
  const id = detailFor.value?.id
  await load()
  if (!id) return
  try {
    detail.value = await api(`/api/docker/networks/${encodeURIComponent(id)}`)
  } catch {
    // 详情读失败不覆盖已经刷新的列表，弹窗里保持上一次的内容即可
  }
}

function openCreate() {
  createForm.value = { name: '', internal: false, subnet: '', gateway: '', ipv6: false }
  creating.value = true
}

async function submitCreate() {
  const f = createForm.value
  submitting.value = true
  try {
    const r: any = await json('POST', '/api/docker/networks', {
      name: f.name,
      internal: f.internal,
      // 留空表示「让 Docker 自己挑网段」，所以传 null 而不是空字符串。
      subnet: String(f.subnet || '').trim() || null,
      gateway: String(f.gateway || '').trim() || null,
      ipv6: f.ipv6,
    })
    creating.value = false
    // Docker 的 warning 不能吞：网段重叠这类问题只会出现在这里。
    flash(r?.warning
      ? `网络 ${f.name} 已创建，但 Docker 提示：${r.warning}`
      : `网络 ${f.name} 已创建`)
    await load()
  } catch (e) {
    flash((e as Error).message, 'error')
  } finally {
    submitting.value = false
  }
}

function openDelete(n: any) {
  deleting.value = n
  deleteConfirm.value = ''
}

async function submitDelete() {
  const n = deleting.value
  if (!n || deleteConfirm.value !== n.name) return
  try {
    await json('DELETE', `/api/docker/networks/${encodeURIComponent(n.id)}`)
    deleting.value = null
    flash(`网络 ${n.name} 已删除`)
    await load()
  } catch (e) {
    flash((e as Error).message, 'error')
  }
}

async function openConnect(n: any) {
  connecting.value = { network: n, container: '', ipv4: '' }
  containers.value = []
  try {
    containers.value = (await api('/api/docker/containers')).containers || []
  } catch (e) {
    flash((e as Error).message, 'error')
  }
}

async function submitConnect() {
  const c = connecting.value
  if (!c) return
  if (!c.container) {
    flash('先选一个容器', 'error')
    return
  }
  try {
    await json('POST', `/api/docker/networks/${encodeURIComponent(c.network.id)}/connect`, {
      container: c.container,
      ipv4: String(c.ipv4 || '').trim() || null,
    })
    connecting.value = null
    flash(`已把 ${c.container} 接入网络 ${c.network.name}`)
    await reloadAfterChange()
  } catch (e) {
    flash((e as Error).message, 'error')
  }
}

async function disconnect(member: any) {
  const net = detailFor.value
  if (!net) return
  try {
    await json('POST', `/api/docker/networks/${encodeURIComponent(net.id)}/disconnect`, {
      container: member.id,
      force: forceDisconnect.value,
    })
    flash(`已把 ${member.name} 从 ${net.name} 断开`)
    await reloadAfterChange()
  } catch (e) {
    flash((e as Error).message, 'error')
  }
}

// 父组件切到「网络」标签页时调它（懒加载，与其它标签页一致）。
defineExpose({ load })
</script>

<template>
  <NeumorphismCard>
    <div class="esc-section-title">
      <h2>Docker 网络</h2>
      <NeumorphismTag size="small" variant="primary">{{ networks.length }}</NeumorphismTag>
      <span class="esc-spacer" />
      <NeumorphismButton v-if="props.allowCreate" variant="primary" size="small" @click="openCreate">新建网络</NeumorphismButton>
      <NeumorphismButton size="small" @click="load">刷新</NeumorphismButton>
    </div>

    <div class="esc-filter">
      <NeumorphismInput v-model="filter" placeholder="过滤名称 / 驱动 / 网段" />
    </div>

    <div v-if="loading && !networks.length" class="esc-empty">读取中…</div>
    <div v-else-if="!networks.length" class="esc-empty">没有网络</div>
    <div v-else-if="!filtered.length" class="esc-empty">没有匹配的网络</div>

    <template v-for="g in groups" :key="g.key">
      <div class="esc-section-title">
        <h3>{{ g.title }}</h3>
        <NeumorphismTag size="small">{{ g.items.length }}</NeumorphismTag>
        <span v-if="g.hint" class="esc-hint">{{ g.hint }}</span>
      </div>
      <div class="esc-table-wrap">
        <table class="esc-table">
          <thead>
            <tr><th>名称</th><th>驱动</th><th>网段</th><th>属性</th><th>创建时间</th><th /></tr>
          </thead>
          <tbody>
            <tr v-for="n in g.items" :key="n.id">
              <td>
                <strong>{{ n.name }}</strong><br>
                <span class="mono esc-hint">{{ n.id.slice(0, 12) }}</span>
              </td>
              <td>{{ n.driver }}</td>
              <td>
                <div v-if="n.subnets.length" v-for="s in n.subnets" :key="s.subnet" class="mono">
                  {{ s.subnet }}<span v-if="s.gateway" class="esc-hint"> → {{ s.gateway }}</span>
                </div>
                <span v-else class="esc-hint">自动</span>
              </td>
              <td class="esc-tagcell">
                <NeumorphismTag v-if="n.internal" size="small" variant="warning">内部</NeumorphismTag>
                <NeumorphismTag v-if="n.system" size="small">系统</NeumorphismTag>
                <NeumorphismTag v-if="n.ipv6" size="small">IPv6</NeumorphismTag>
                <NeumorphismTag v-if="n.attachable" size="small">可附着</NeumorphismTag>
              </td>
              <td class="esc-hint">{{ when(n.created) }}</td>
              <td>
                <div class="esc-actions">
                  <NeumorphismButton size="small" @click="openDetail(n)">详情</NeumorphismButton>
                  <NeumorphismButton v-if="props.allowActions && !n.system" size="small" @click="openConnect(n)">接入</NeumorphismButton>
                  <NeumorphismButton v-if="props.allowDelete && !n.system" size="small" @click="openDelete(n)">删除</NeumorphismButton>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </template>
  </NeumorphismCard>

  <!-- 详情 -->
  <NeumorphismModal
    :model-value="!!detailFor"
    :title="`网络详情 · ${detailFor?.name || ''}`"
    :size="880"
    :footer="false"
    @update:model-value="detailFor = null"
  >
    <div v-if="detailLoading" class="esc-empty">读取中…</div>
    <div v-else-if="detail">
      <dl class="esc-kv">
        <dt>名称</dt><dd>{{ detail.network.name }}</dd>
        <dt>ID</dt><dd class="mono">{{ detail.network.id }}</dd>
        <dt>驱动 / 作用域</dt><dd>{{ detail.network.driver }} / {{ detail.network.scope }}</dd>
        <dt>网段</dt><dd class="mono">{{ subnets(detail.network) }}</dd>
        <dt>属性</dt><dd>{{ detail.network.internal ? '内部网络' : '普通网络' }}{{ detail.network.ipv6 ? ' · IPv6' : '' }}{{ detail.network.system ? ' · 系统网络' : '' }}</dd>
        <dt>创建时间</dt><dd>{{ when(detail.network.created) }}</dd>
        <template v-if="detail.network.labels.length">
          <dt>Labels</dt>
          <dd>
            <div v-for="[k, v] in detail.network.labels" :key="k" class="mono">{{ k }}={{ v }}</div>
          </dd>
        </template>
      </dl>

      <p v-if="detail.network.internal" class="esc-note">
        这是<b>内部网络</b>：容器没有通往宿主外部的路由，端口映射（<code>-p</code>）不生效，
        只能与本网络内其它成员通信。要让容器重新上网，得把它接到别的网络上。
      </p>

      <div class="esc-section-title">
        <h3>成员容器</h3>
        <NeumorphismTag size="small">{{ detail.containers.length }}</NeumorphismTag>
        <span class="esc-spacer" />
        <label v-if="props.allowActions && detail.containers.length" class="esc-check">
          <NeumorphismSwitch v-model="forceDisconnect" />
          强制断开（运行中也照断）
        </label>
      </div>

      <div v-if="!detail.containers.length" class="esc-empty">
        暂无容器接入<br>
        <span class="esc-hint">已停止的容器不会显示在这里——Docker 只给运行中的容器保留 endpoint</span>
      </div>
      <div v-else class="esc-table-wrap">
        <table class="esc-table">
          <thead><tr><th>容器</th><th>IPv4</th><th>MAC</th><th /></tr></thead>
          <tbody>
            <tr v-for="c in detail.containers" :key="c.id">
              <td>
                {{ c.name }}<br>
                <span class="mono esc-hint">{{ c.id.slice(0, 12) }}</span>
              </td>
              <td class="mono">{{ c.ipv4 || '—' }}</td>
              <td class="mono">{{ c.mac || '—' }}</td>
              <td>
                <NeumorphismButton v-if="props.allowActions" size="small" @click="disconnect(c)">断开</NeumorphismButton>
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <details class="esc-details">
        <summary class="esc-hint">原始 JSON</summary>
        <pre class="esc-code">{{ JSON.stringify(detail, null, 2) }}</pre>
      </details>
    </div>
  </NeumorphismModal>

  <!-- 新建 -->
  <NeumorphismModal v-model="creating" title="新建网络" :size="660" :footer="false">
    <div class="esc-form">
      <NeumorphismInput v-model="createForm.name" label="网络名" required placeholder="my-net" />
      <label class="esc-check">
        <NeumorphismSwitch v-model="createForm.internal" />
        内部网络（无外网出口，端口映射不生效）
      </label>
      <div class="esc-form-row">
        <NeumorphismInput v-model="createForm.subnet" label="子网（留空自动分配）" placeholder="172.30.0.0/24" />
        <NeumorphismInput v-model="createForm.gateway" label="网关（可留空）" placeholder="172.30.0.1" />
      </div>
      <label class="esc-check">
        <NeumorphismSwitch v-model="createForm.ipv6" />
        启用 IPv6
      </label>
      <span class="esc-hint">
        驱动固定 bridge。子网限 /24–/29：太大一次吃掉整个地址池，太小装不下几个容器；
        留空时由 Docker 挑一个不冲突的网段（最省事）。
      </span>
      <div class="esc-actions">
        <NeumorphismButton variant="primary" :loading="submitting" @click="submitCreate">创建</NeumorphismButton>
        <NeumorphismButton @click="creating = false">取消</NeumorphismButton>
      </div>
    </div>
  </NeumorphismModal>

  <!-- 删除：不可撤销，要求手打网络名 -->
  <NeumorphismModal
    :model-value="!!deleting"
    :title="`删除网络 · ${deleting?.name || ''}`"
    :size="580"
    :footer="false"
    @update:model-value="deleting = null"
  >
    <div class="esc-form">
      <p>
        删除<strong>不可撤销</strong>。接在这个网络上的容器会立刻失去这块网络
        ——Docker 不允许删除还有成员的网络，所以多数情况下要先断开它们。
      </p>
      <p>
        确认请输入网络名 <code>{{ deleting?.name }}</code>：
      </p>
      <NeumorphismInput v-model="deleteConfirm" label="网络名" :placeholder="deleting?.name" />
      <div class="esc-actions">
        <NeumorphismButton
          variant="pressed"
          :disabled="deleteConfirm !== deleting?.name"
          @click="submitDelete"
        >永久删除</NeumorphismButton>
        <NeumorphismButton @click="deleting = null">取消</NeumorphismButton>
      </div>
    </div>
  </NeumorphismModal>

  <!-- 接入容器 -->
  <NeumorphismModal
    :model-value="!!connecting"
    :title="`接入容器 · ${connecting?.network?.name || ''}`"
    :size="620"
    :footer="false"
    @update:model-value="connecting = null"
  >
    <div class="esc-form">
      <NeumorphismSelect
        v-model="connecting.container"
        label="容器"
        placeholder="选一个容器"
        :options="containers.map((c: any) => ({ value: c.id, label: `${c.name}（${c.state}）` }))"
      />
      <NeumorphismInput v-model="connecting.ipv4" label="固定 IPv4（可留空）" placeholder="172.30.0.10" />
      <span class="esc-hint">
        接入会立即生效，容器不会重启；但它的网络栈会短暂中断一下。
        已停止的容器接进来后不会出现在成员列表里，启动之后才会。
      </span>
      <div class="esc-actions">
        <NeumorphismButton variant="primary" @click="submitConnect">接入</NeumorphismButton>
        <NeumorphismButton @click="connecting = null">取消</NeumorphismButton>
      </div>
    </div>
  </NeumorphismModal>
</template>
