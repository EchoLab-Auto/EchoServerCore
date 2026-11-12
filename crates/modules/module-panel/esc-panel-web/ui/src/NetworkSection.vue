<script setup lang="ts">

// 「网络」这一块：Docker 网络 / 端口映射 / 网关。
//
// # 为什么这三件事放在同一个标签页
//
// 它们回答的是同一个问题——**这台机器上的流量从哪进、到哪去、谁能进**：
//
//   Docker 网络   容器之间的东西向内网
//   端口映射      经隧道进出的南北向流量（公网 2222 → 本机 8080）
//   网关          每个入口后面的认证与按端口的 ACL
//
// 从前它们是三个平级标签页，于是「2222 这个端口是谁在听、要不要登录」这个问题
// 要在三个页面之间来回切才能拼出来。放一起之后，一条链路在一屏之内就能看完。
//
// # 子页面各自探测可用性
//
// 没配隧道就没有「端口映射」，网关不可达就没有「网关」——**不显示比显示一个
// 必然报错的入口好**（后者每次点进去都要读一遍错误才能确认这台机器没这功能）。
// 探测在 ServerPanel 里做（它本来就要探），这里只按传进来的布尔值决定渲染。
//
// Docker 网络始终在：它不需要外部上游，Docker 挂了也是「没有网络」这种空态，
// 那是本页自己的事。

import { computed, onMounted, ref, watch } from 'vue'
import GatewaySection from './GatewaySection.vue'
import TlsSection from './TlsSection.vue'
import Networks from './Networks.vue'
import TunnelSection from './TunnelSection.vue'

const props = defineProps<{
  allowCreate: boolean
  allowActions: boolean
  allowDelete: boolean
  tunnelReady: boolean
  gatewayReady: boolean
  tlsReady: boolean
}>()

type Sub = 'docker' | 'tls' | 'tunnel' | 'gateway'

const sub = ref<Sub>('docker')

const tabs = computed<[Sub, string][]>(() => {
  const out: [Sub, string][] = [['docker', 'Docker 网络']]
  if (props.tlsReady) out.push(['tls', 'TLS 终结'])
  if (props.tunnelReady) out.push(['tunnel', '端口映射'])
  if (props.gatewayReady) out.push(['gateway', '网关'])
  return out
})

/** 探测结果可能在页面开着的时候变（隧道进程重启、网关被下架）。 */
watch(tabs, list => {
  if (!list.some(([key]) => key === sub.value)) sub.value = 'docker'
})

// 延迟挂载：没点开过的子页面不拉数据。
//
// 挂过之后用 `v-show` 保留，而不是切走就卸载 —— 这样表单里填了一半的内容、
// 表格的滚动位置还在，切回来也不用等一次重新加载。隧道那一页的 5 秒轮询同理
// （见 TunnelSection 里的说明：两个 JSON 端点，代价可以忽略）。
const visited = ref(new Set<Sub>(['docker']))
watch(sub, value => {
  if (!visited.value.has(value)) visited.value = new Set(visited.value).add(value)
}, { immediate: true })

const showing = (key: Sub) => sub.value === key

// 壳子切到「网络」标签页时会调 `networksPanel.load()`（见 ServerPanel 的 watch）。
// 这一层是新增的中间层，得把那次调用转给真正有 `load` 的 Docker 网络组件。
// 顺带在首次进入 Docker 子页时也拉一次 —— 用户可能直接点进来，没经过壳子的 watch。
const docker = ref<InstanceType<typeof Networks>>()
onMounted(() => { if (showing('docker')) docker.value?.load() })

defineExpose({ load: () => docker.value?.load() })
</script>

<template>
  <div>
    <!-- 只有一个子页面时不画这条栏：一个标签的标签栏只是占地方 -->
    <NeumorphismTabs
      v-if="tabs.length > 1"
      v-model="sub"
      class="esc-tabs esc-subtabs"
      size="small"
      :tabs="tabs.map(([key, label]) => ({ key, label }))"
    />

    <!--
      显示/隐藏套在**普通 div** 上，不直接写在组件上。
      组件上的 `v-show` 会和组件根节点自己的 `:style` 打架 —— NeumorphismCard 的根
      div 带 `:style="rootStyle"`，于是隐藏指令设下的 `display:none` 在下一次重渲染时
      被样式补丁覆盖回去，实测「切到端口映射后 Docker 网络那张卡还在」。套一层 div
      之后指令作用于这个 div，与组件内部的样式互不干扰。
    -->
    <div v-if="visited.has('docker')">
      <div v-show="showing('docker')">
        <Networks
          ref="docker"
          :allow-create="props.allowCreate"
          :allow-actions="props.allowActions"
          :allow-delete="props.allowDelete"
        />
      </div>
    </div>

    <div v-if="props.tlsReady && visited.has('tls')">
      <div v-show="showing('tls')"><TlsSection /></div>
    </div>

    <div v-if="props.tunnelReady && visited.has('tunnel')">
      <div v-show="showing('tunnel')"><TunnelSection /></div>
    </div>

    <div v-if="props.gatewayReady && visited.has('gateway')">
      <div v-show="showing('gateway')"><GatewaySection /></div>
    </div>
  </div>
</template>
