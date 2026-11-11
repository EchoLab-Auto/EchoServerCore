<script setup lang="ts">
// 导航页：把配置里的 [[nav]] 条目摆成一屏可以点进去的入口。
//
// 纯展示组件——数据由 ServerPanel 拉（和其它 loadXxx 一致），这里只负责画。
// 唯一的行为是「点开」（整卡是 <a>，新标签页打开）：导航页不该有副作用。
import { computed } from 'vue'

const props = defineProps<{ groups: any[]; stale?: boolean }>()

/** 把 URL 压成「主机[:端口]」，比整条链接好读，也不至于挤爆卡片。 */
function shortUrl(raw: unknown): string {
  try {
    const url = new URL(String(raw))
    // 默认端口不显示：https://x:443/ 写成 x 就够了
    const port = url.port && !((url.protocol === 'https:' && url.port === '443') || (url.protocol === 'http:' && url.port === '80'))
      ? ':' + url.port
      : ''
    return url.hostname + port
  } catch {
    return String(raw ?? '')
  }
}

const total = computed(() => props.groups.reduce((sum, group) => sum + (group.items?.length || 0), 0))
const downCount = computed(() =>
  props.groups.reduce((sum, group) => sum + (group.items || []).filter((i: any) => i.status === 'down').length, 0),
)
</script>

<template>
  <div v-if="!total" class="esc-empty">
    <p><strong>还没有配置导航条目。</strong></p>
    <p class="esc-hint">
      在 <code>server-panel.toml</code> 里加一条就会出现在这里：
      <code class="esc-code">[[nav]]
name  = "隧道面板"
url   = "https://example.com:2223/"
desc  = "转发隧道与会话"
group = "面板"</code>
    </p>
  </div>

  <template v-else>
    <div class="esc-nav-summary">
      <span>{{ total }} 个入口</span>
      <span v-if="downCount" class="esc-nav-down">其中 {{ downCount }} 个连不上</span>
      <span v-else class="esc-nav-ok">全部可达</span>
      <span v-if="stale" class="esc-nav-stale">刷新失败，显示的是上一次的结果</span>
    </div>

    <section v-for="group in groups" :key="group.name">
      <div class="esc-section-title">
        <h2>{{ group.name }}</h2>
        <NeumorphismTag>{{ group.items.length }}</NeumorphismTag>
      </div>
      <div class="esc-nav-grid">
        <a
          v-for="item in group.items"
          :key="item.url + item.name"
          class="esc-nav-card"
          :class="'is-' + item.status"
          :href="item.url"
          target="_blank"
          rel="noreferrer noopener"
          :title="item.detail ? `${item.url}\n${item.detail}` : item.url"
        >
          <div class="esc-nav-head">
            <span class="esc-nav-dot" :class="'is-' + item.status" />
            <span class="esc-nav-name">{{ item.name }}</span>
          </div>
          <p v-if="item.desc" class="esc-nav-desc">{{ item.desc }}</p>
          <div v-if="item.tags?.length" class="esc-nav-tags">
            <NeumorphismTag v-for="tag in item.tags" :key="tag" size="small">{{ tag }}</NeumorphismTag>
          </div>
          <div class="esc-nav-foot">
            <span class="esc-nav-host">{{ shortUrl(item.url) }}</span>
            <!-- 探测结果只是提示：down 的卡片仍然可以点（探测从服务器视角出发，
                 可能因为自签证书或回环地址而误判，用户自己点一下才是真相）。 -->
            <span v-if="item.status === 'up'" class="esc-nav-latency">{{ item.ms }}ms</span>
            <span v-else-if="item.status === 'down'" class="esc-nav-error">{{ item.detail || '连不上' }}</span>
            <span v-else class="esc-nav-latency">未探测</span>
          </div>
        </a>
      </div>
    </section>
  </template>
</template>
