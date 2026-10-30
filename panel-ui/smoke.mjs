import { chromium } from '@playwright/test'

const browser = await chromium.launch({ headless: true, args: ['--no-sandbox'] })
const results = []
for (const [name, url, tab] of [
  ['tunnel', 'http://127.0.0.1:18787/', '插件树'],
  ['server', 'http://127.0.0.1:18788/', '插件树'],
]) {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } })
  const errors = []
  page.on('pageerror', error => errors.push(String(error)))
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()) })
  await page.goto(url, { waitUntil: 'networkidle' })
  await page.locator('.nm-card:visible').first().waitFor()
  await page.screenshot({ path: `/tmp/esc-${name}-ui.png`, fullPage: true })
  await page.getByRole('tab', { name: tab, exact: true }).click()
  await page.locator('.nm-tree-node').first().waitFor()
  const plugins = await page.locator('.nm-tree-node').count()
  if (plugins < 3) throw new Error(`${name}: 插件节点只有 ${plugins}`)
  await page.screenshot({ path: `/tmp/esc-${name}-plugins.png`, fullPage: true })
  results.push({ name, cards: await page.locator('.nm-card').count(), plugins, errors })
  if (name === 'server') {
    await page.getByRole('tab', { name: '容器', exact: true }).click()
    await page.getByText('echo-cad-toolbox', { exact: true }).first().waitFor()
    results.at(-1).containers = await page.locator('.esc-container').count()
    await page.getByRole('tab', { name: '主机概览', exact: true }).click()
    await page.getByRole('heading', { name: 'CPU', exact: true }).waitFor()
  }
  if (name === 'tunnel') {
    await page.getByRole('tab', { name: '隧道管理', exact: true }).click()
    await page.getByText('端口映射', { exact: true }).waitFor()
    await page.getByRole('button', { name: '＋ 新建映射', exact: true }).click()
    await page.locator('.nm-modal').waitFor()
    await page.getByRole('button', { name: '取消', exact: true }).click()
  }
  if (errors.length) throw new Error(`${name}: 浏览器错误 ${errors.join('; ')}`)
  await page.close()
}
console.log(JSON.stringify(results, null, 2))
await browser.close()
