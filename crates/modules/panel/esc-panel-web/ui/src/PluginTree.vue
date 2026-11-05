<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { api, stateVariant } from './ui'

const tree = ref<any>(null)
const error = ref('')
const selectedKey = ref<string[]>([])
const expandedKeys = ref<string[]>([])

function flatten(nodes: any[], out: any[] = []): any[] {
  for (const n of nodes || []) { out.push(n); flatten(n.children, out) }
  return out
}
const nodes = computed(() => flatten(tree.value?.plugins || []))
const selected = computed(() => nodes.value.find(n => n.id === selectedKey.value[0]))
const count = computed(() => nodes.value.length)

async function load() {
  error.value = ''
  try {
    tree.value = await api('/api/plugins')
    tree.value.plugins = (tree.value.plugins || []).map((n: any) => ({ ...n, key: n.id, label: n.name || n.id }))
    const keys = nodes.value.map(n => n.id)
    expandedKeys.value = keys
    if (!selectedKey.value.length && keys.length) selectedKey.value = [keys[0]]
  } catch (e) { error.value = (e as Error).message }
}

function select(key: string) { selectedKey.value = [key] }

onMounted(load)
defineExpose({ load })
</script>

<template>
  <NeumorphismCard>
    <div class="esc-section-title">
      <h2>服务器核心插件树</h2>
      <NeumorphismTag v-if="tree" variant="primary">{{ count }} 个插件</NeumorphismTag>
      <span class="esc-spacer" />
      <NeumorphismButton size="small" @click="load">刷新</NeumorphismButton>
    </div>
    <p class="esc-hint">数据来自 esc-host 的 esc.plugins/tree。缩进是框架的父子作用域，依赖是 Inject 声明的服务键。</p>
    <NeumorphismAlert v-if="error" class="esc-error" type="error" title="插件树不可用" :message="error" />
    <template v-if="tree">
      <div class="esc-plugin-services mono">
        <div><span>宿主：</span>{{ tree.host }} · {{ tree.framework }}</div>
        <div><span>Ctx 服务：</span>{{ (tree.context_services || []).join('、') || '（空）' }}</div>
        <div><span>HostCall 服务：</span>{{ (tree.host_services || []).join('、') || '（空）' }}</div>
      </div>
      <NeumorphismTree
        :data="tree.plugins"
        :expanded-keys="expandedKeys"
        :selected-keys="selectedKey"
        @node-select="select"
      />
      <NeumorphismCard v-if="selected" class="esc-plugin-selected" :elevation="-1">
        <div class="esc-section-title">
          <h2>{{ selected.name || selected.id }}</h2>
          <NeumorphismTag :variant="stateVariant(selected.state)">{{ selected.state }}</NeumorphismTag>
        </div>
        <dl class="esc-kv">
          <dt>插件 ID</dt><dd class="mono">{{ selected.id }}</dd>
          <dt>版本 / 类型</dt><dd>v{{ selected.version }} · {{ selected.kind }}</dd>
          <dt>运行形态</dt><dd>{{ selected.runtime === 'subprocess' ? '独立子进程' : '宿主进程内' }}</dd>
          <dt>已挂载</dt><dd>{{ selected.mounted ? '是' : '否' }}</dd>
          <template v-if="selected.runtime === 'subprocess'">
            <dt>重启 / 代际</dt><dd>{{ selected.restarts }} 次 / {{ selected.generation }}</dd>
            <dt>启动命令</dt><dd class="mono">{{ selected.command || '—' }}</dd>
          </template>
          <dt>说明</dt><dd>{{ selected.description || '—' }}</dd>
        </dl>
        <div v-if="selected.required_services?.length" class="esc-deps">
          <span class="esc-hint">硬依赖</span>
          <NeumorphismTag v-for="s in selected.required_services" :key="s" variant="warning" size="small">{{ s }}</NeumorphismTag>
        </div>
        <div v-if="selected.optional_services?.length" class="esc-deps">
          <span class="esc-hint">软依赖</span>
          <NeumorphismTag v-for="s in selected.optional_services" :key="s" size="small">{{ s }}</NeumorphismTag>
        </div>
        <div v-if="selected.pending_services?.length" class="esc-deps">
          <span class="esc-hint">当前缺失</span>
          <NeumorphismTag v-for="s in selected.pending_services" :key="s" variant="error" size="small">{{ s }}</NeumorphismTag>
        </div>
        <NeumorphismAlert v-if="selected.error" type="error" title="运行错误" :message="selected.error" />
      </NeumorphismCard>
    </template>
    <div v-else-if="!error" class="esc-empty">正在读取插件树…</div>
  </NeumorphismCard>
</template>
