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
    // 网络页：三个分组按 内部 → 自定义 → 系统 排，系统网络必须标出来
    await page.getByRole('tab', { name: '网络', exact: true }).click()
    await page.getByText('系统网络', { exact: true }).first().waitFor()
    const groups = await page.locator('.esc-section-title h3').allInnerTexts()
    for (const want of ['系统网络']) {
      if (!groups.includes(want)) throw new Error(`server: 网络页缺少「${want}」分组（实际 ${groups}）`)
    }
    // bridge/host/none 三个系统网络在表里必须存在，且带「系统」徽章
    for (const net of ['bridge', 'host', 'none']) {
      const row = page.locator('tr', { hasText: net }).first()
      await row.waitFor()
    }
    if (await page.locator('.esc-tagcell .nm-tag', { hasText: '系统' }).count() < 3) {
      throw new Error('server: 系统网络没有被标记')
    }
    results.at(-1).networkGroups = groups
    results.at(-1).networks = await page.locator('table.esc-table:visible tbody tr').count()
    await page.screenshot({ path: '/tmp/esc-server-networks.png', fullPage: true })
    // 详情弹窗能打开、能读到成员区
    await page.locator('tr', { hasText: 'bridge' }).first.getByRole('button', { name: '详情' }).click()
    await page.locator('.nm-modal:visible').getByText('成员容器').waitFor()
    await page.locator('.nm-modal:visible').getByText('原始 JSON').waitFor()
    await page.screenshot({ path: '/tmp/esc-server-network-detail.png', fullPage: true })
    await page.locator('.nm-modal:visible .nm-modal__close').first().click()
    await page.getByRole('tab', { name: '主机概览', exact: true }).click()
    await page.getByRole('heading', { name: 'CPU', exact: true }).waitFor()
  }
  if (name === 'tunnel') {
    await page.getByRole('tab', { name: '隧道管理', exact: true }).click()
    await page.getByText('端口映射', { exact: true }).waitFor()
    await page.getByRole('button', { name: '＋ 新建映射', exact: true }).click()
    await page.locator('.nm-modal').waitFor()
    await page.getByRole('button', { name: '取消', exact: true }).click()
    await page.getByRole('button', { name: '＋ 新建路由', exact: true }).click()
    const edgeModal = page.locator('.nm-modal:visible')
    await edgeModal.getByLabel('域名前缀或完整域名').fill('cad.example.com')
    await edgeModal.getByText('只匹配该域名，优先于前缀', { exact: false }).waitFor()
    await page.screenshot({ path: '/tmp/esc-edge-create.png', fullPage: true })
    await edgeModal.getByRole('button', { name: '取消', exact: true }).click()
    const cadRow = page.locator('tr', { has: page.locator('td.mono', { hasText: /^cad$/ }) })
    await cadRow.getByRole('button', { name: '编辑', exact: true }).click()
    await edgeModal.getByRole('heading', { name: '编辑边缘路由', exact: true }).waitFor()
    if (await edgeModal.getByLabel('域名前缀或完整域名').inputValue() !== 'cad') throw new Error('编辑没有回填路由键')
    if (await edgeModal.getByLabel('转发到本机端口').inputValue() !== '2228') throw new Error('编辑没有回填端口')
    await page.screenshot({ path: '/tmp/esc-edge-edit.png', fullPage: true })
    await edgeModal.getByRole('button', { name: '取消', exact: true }).click()
    results.at(-1).edgeEditor = true
console.log(JSON.stringify(results, null, 2))
await browser.close()
