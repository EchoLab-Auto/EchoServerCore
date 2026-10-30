import { createApp, ref } from 'vue'
import {
  NeumorphismAlert, NeumorphismButton, NeumorphismCard, NeumorphismInput,
  NeumorphismInputNumber, NeumorphismModal, NeumorphismProgress,
  NeumorphismSelect, NeumorphismSwitch, NeumorphismTabs, NeumorphismTag,
  NeumorphismTextarea, NeumorphismTree, createTheme,
} from '@echolab-auto/ui-frame'

export const components = {
  NeumorphismAlert, NeumorphismButton, NeumorphismCard, NeumorphismInput,
  NeumorphismInputNumber, NeumorphismModal, NeumorphismProgress,
  NeumorphismSelect, NeumorphismSwitch, NeumorphismTabs, NeumorphismTag,
  NeumorphismTextarea, NeumorphismTree,
}

export const theme = createTheme({ defaultTheme: 'dark' })
export const flashKind = ref<'success' | 'error'>('success')
export const flashText = ref('')
let flashTimer: ReturnType<typeof setTimeout> | undefined
export function flash(message: string, kind: 'success' | 'error' = 'success') {
  flashText.value = message
  flashKind.value = kind
  clearTimeout(flashTimer)
  flashTimer = setTimeout(() => { flashText.value = '' }, kind === 'error' ? 7000 : 3500)
}

export async function api<T = any>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(path, { cache: 'no-store', ...init })
  const text = await response.text()
  let data: any = null
  try { data = text ? JSON.parse(text) : null } catch { data = { error: text } }
  if (!response.ok) throw new Error(data?.error || `HTTP ${response.status}`)
  return data as T
}

export function json(method: string, path: string, body: unknown = {}) {
  return api(path, { method, headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) })
}

export function bytes(value: unknown, digits = 1): string {
  let n = Number(value)
  if (!Number.isFinite(n)) return '—'
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB']
  let i = 0
  while (Math.abs(n) >= 1024 && i < units.length - 1) { n /= 1024; i++ }
  return n.toFixed(i > 0 && n < 100 ? digits : 0) + ' ' + units[i]
}

export function duration(seconds: unknown): string {
  const n = Math.max(0, Math.floor(Number(seconds) || 0))
  const d = Math.floor(n / 86400), h = Math.floor(n % 86400 / 3600), m = Math.floor(n % 3600 / 60)
  if (d) return `${d}天 ${h}小时`
  if (h) return `${h}小时 ${m}分`
  if (m) return `${m}分 ${n % 60}秒`
  return `${n % 60}秒`
}

export function stateVariant(state: unknown): string {
  return ['ready', 'active', 'running', 'online'].includes(String(state)) ? 'success'
    : ['failed', 'error', 'offline'].includes(String(state)) ? 'error'
    : ['pending', 'connecting', 'draining'].includes(String(state)) ? 'warning' : 'default'
}

export function mountPanel(component: object) {
  const app = createApp(component)
  Object.entries(components).forEach(([name, c]) => app.component(name, c as any))
  app.mount('#app')
}

export const tabItems = (items: [string, string][]) => items.map(([key, label]) => ({ key, label }))
