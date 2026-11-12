import { chromium } from '@playwright/test'

// `CHROME_PATH` 可选：有些机器上 playwright 的浏览器版本和 `node_modules` 里的
// 版本对不上（报 "Executable doesn't exist at .../chromium_headless_shell-XXXX"），
// 这时把已有的 chrome 指过来就能跑，不必为一个烟测重下几百兆浏览器。
const browser = await chromium.launch({
  headless: true,
  args: ['--no-sandbox'],
  ...(process.env.CHROME_PATH ? { executablePath: process.env.CHROME_PATH } : {}),
})
const results = []
for (const [name, url, tab] of [
  ['tunnel', 'http://127.0.0.1:18787/', '插件树'],
  ['server', 'http://127.0.0.1:18788/', '插件树'],
]) {
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } })
  // 只认**最上层**那个弹窗（DOM 里最后挂载的）。不要用 `.nm-modal:visible` 的
  // 第一个：上一个弹窗还在关闭过渡里时，会短暂同时存在两个可见对话框，
  // 选择器直接撞上 strict mode（实测报 resolved to 2 elements）。
  const topModal = () => page.locator('.nm-modal').last()
  const errors = []
  page.on('pageerror', error => errors.push('JS: ' + String(error)))
  // 控制台里的 "Failed to load resource: ... 401/404" 不单独记：它只是网络层的回声，
  // 下面按 URL 断言更准确。留下它会让「面板探测一个本机没配的上游」这种**设计内**的
  // 404 变成烟测失败 —— 那正是 `tunnelReady / gatewayReady` 探测在做的事。
  page.on('console', message => {
    if (message.type() !== 'error') return
    if (/Failed to load resource/.test(message.text())) return
    errors.push(message.text())
  })
  // 允许清单：网关探测（没配网关时按设计回 404 / 未登录回 401）、favicon。
  page.on('response', response => {
    const status = response.status()
    if (status < 400) return
    const url = response.url()
    if (/[\/]__admin\//.test(url)) return
    if (/favicon\.ico$/.test(url)) return
    errors.push(`HTTP ${status} ${url}`)
  })
  await page.goto(url, { waitUntil: 'networkidle' })
  let navCards
  if (name === 'server') {
    // 服务器面板配了 [[nav]] 时默认落在「导航」，没配时直接是主机概览。
    // 两种形态都要能跑，所以先探测一下再决定要不要点。
    const navTab = page.getByRole('tab', { name: '导航', exact: true })
    if (await navTab.count()) {
      await page.locator('.esc-nav-card').first().waitFor()
      navCards = await page.locator('.esc-nav-card').count()
      await page.screenshot({ path: '/tmp/esc-server-nav.png', fullPage: true })
      // 导航页上每张卡片都必须有状态点：数量对不上说明探测结果没渲染出来
      if (await page.locator('.esc-nav-dot').count() !== navCards) {
        throw new Error('server: 导航卡片的存活状态点数量对不上')
      }
      await page.getByRole('tab', { name: '主机概览', exact: true }).click()
    }
  }
  await page.locator('.nm-card:visible').first().waitFor()
  await page.screenshot({ path: `/tmp/esc-${name}-ui.png`, fullPage: true })
  await page.getByRole('tab', { name: tab, exact: true }).click()
  await page.locator('.nm-tree-node').first().waitFor()
  const plugins = await page.locator('.nm-tree-node').count()
  if (plugins < 3) throw new Error(`${name}: 插件节点只有 ${plugins}`)
  await page.screenshot({ path: `/tmp/esc-${name}-plugins.png`, fullPage: true })
  results.push({ name, cards: await page.locator('.nm-card').count(), plugins, navCards, errors })
  if (name === 'server') {
    await page.getByRole('tab', { name: '容器', exact: true }).click()
    await page.getByText('echo-cad-toolbox', { exact: true }).first().waitFor()
    results.at(-1).containers = await page.locator('.esc-container').count()
    await page.getByRole('tab', { name: '网络', exact: true }).click()
    await page.getByText('系统网络', { exact: true }).first().waitFor()
    for (const net of ['bridge', 'host', 'none']) {
      await page.locator('tr', { hasText: net }).first().waitFor()
    }
    if (await page.locator('.esc-tagcell .nm-tag', { hasText: '系统' }).count() < 3) {
      throw new Error('server: 系统网络没有被标记')
    }
    results.at(-1).networks = await page.locator('table.esc-table:visible tbody tr').count()
    await page.locator('tr', { hasText: 'bridge' }).first().getByRole('button', { name: '详情' }).click()
    await page.locator('.nm-modal:visible').getByText('成员容器').waitFor()
    await page.locator('.nm-modal:visible .nm-modal__close').first().click()

    // ── 「网络」里的子页与它们的弹窗 ──
    //
    // 这一段是补的：合并三个面板那次，`TunnelSection.vue` 的 `<script setup>` 把
    // 新建/编辑映射、边缘路由、导出配置的全套逻辑都搬过来了，**模板里的三个弹窗却没搬**
    // ——于是那四个按钮点了什么都不发生（改的是一个没人渲染的 ref）。
    // 当时的烟测没抓到，因为它只走 18787 上那个旧独立页（TunnelPanel.vue）。
    //
    // 现在把「按钮必须能开出弹窗、弹窗必须能关掉」逐条钉在这里。
    const subTabs = await page.locator('.esc-subtabs').getByRole('tab').allInnerTexts()
    results.at(-1).networkSubtabs = subTabs

    const openAndClose = async (open, label) => {
      await open()
      const modal = topModal()
      await modal.waitFor({ timeout: 5000 })
      const title = await modal.locator('.nm-modal__title').innerText()
      await modal.getByRole('button', { name: /取消|关闭/ }).first().click()
      await modal.waitFor({ state: 'detached', timeout: 5000 })
      return title
    }

    // TLS 终结子页：把「:2222 ──TLS──▶ 18790 ──隧道──▶ 4090:8080」接起来的那一页。
    // 它存在的意义就是让"某个端口是谁在听、转发到哪"能在一屏里看到。
    if (subTabs.includes('TLS 终结')) {
      await page.getByRole('tab', { name: 'TLS 终结', exact: true }).click()
      await page.getByRole('heading', { name: '终结层', exact: true }).waitFor()
      const chain = page.locator('table.esc-table:visible').filter({ hasText: '链路终点' }).first()
      await chain.waitFor()
      const rows = await chain.locator('tbody tr').count()
      if (rows < 1) throw new Error('server: TLS 链路表是空的')
      // "↳ 就是这条映射的监听地址" 是 TLS 与隧道被接上的标志。
      // 没配隧道映射的部署不会有它，所以只在有映射时才要求。
      const hasTunnelLink = await page.getByText('就是这条映射的监听地址').count()
      results.at(-1).tlsSites = rows
      results.at(-1).tlsChainLinked = hasTunnelLink > 0
      await page.screenshot({ path: '/tmp/esc-tls-chain.png', fullPage: true })
      await page.getByRole('tab', { name: 'Docker 网络', exact: true }).click()
    }

    if (subTabs.includes('端口映射')) {
      await page.getByRole('tab', { name: '端口映射', exact: true }).click()
      await page.getByText('端口映射', { exact: true }).first().waitFor()
      const titles = []
      titles.push(await openAndClose(() => page.getByRole('button', { name: '＋ 新建映射' }).click(), '新建映射'))
      titles.push(await openAndClose(() => page.getByRole('button', { name: '导出配置' }).click(), '导出配置'))
      if (await page.getByRole('button', { name: '＋ 新建路由' }).count()) {
        titles.push(await openAndClose(() => page.getByRole('button', { name: '＋ 新建路由' }).click(), '新建路由'))
      }
      // 有映射时「编辑」必须回填，而不是弹一个空的新建表单
      const editRow = page
        .locator('table.esc-table:visible tbody tr')
        .filter({ has: page.getByRole('button', { name: '编辑', exact: true }) })
        .first()
      if (await editRow.count()) {
        await editRow.getByRole('button', { name: '编辑', exact: true }).click()
        const modal = topModal()
        await modal.waitFor({ timeout: 5000 })
        const t = await modal.locator('.nm-modal__title').innerText()
        const filled = await modal.locator('input').first().inputValue()
        if (!t.includes('编辑')) throw new Error(`server: 映射「编辑」弹出的是「${t}」`)
        if (!filled) throw new Error('server: 映射「编辑」没有回填服务名')
        await modal.getByRole('button', { name: '取消', exact: true }).click()
        await modal.waitFor({ state: 'detached', timeout: 5000 })
        titles.push(t)
      }
      results.at(-1).tunnelModals = titles
      await page.getByRole('tab', { name: 'Docker 网络', exact: true }).click()
    }
    await page.getByRole('tab', { name: '主机概览', exact: true }).click()
    await page.getByRole('heading', { name: 'CPU', exact: true }).waitFor()
  }
  if (name === 'tunnel') {
    await page.getByRole('tab', { name: '隧道管理', exact: true }).click()
    await page.getByText('端口映射', { exact: true }).waitFor()
    await page.getByRole('button', { name: '＋ 新建映射', exact: true }).click()
    const mapModal = topModal()
    await mapModal.waitFor()
    await mapModal.getByRole('button', { name: '取消', exact: true }).click()
    await mapModal.waitFor({ state: 'detached', timeout: 5000 })
    await page.getByRole('button', { name: '＋ 新建路由', exact: true }).click()
    const edgeModal = topModal()
    await edgeModal.getByLabel('域名前缀或完整域名').fill('cad.example.com')
    await edgeModal.getByText('只匹配该域名，优先于前缀', { exact: false }).waitFor()
    await page.screenshot({ path: '/tmp/esc-edge-create.png', fullPage: true })
    await edgeModal.getByRole('button', { name: '取消', exact: true }).click()
    // 「编辑」的回填检查挑第一行现有路由来做，不写死 `cad`。
    // 写死的时候这份烟测只对「开发机上恰好有一条 cad 路由」的部署有效，
    // 换台机器（cloud2 上是 cad.evencewu.asia / echolab.evencewu.asia 这类完整域名）
    // 就会以「找不到 cad 行」失败，看起来像回归、其实只是环境不同。
    const anyRouteRow = page
      .locator('table.esc-table tbody tr')
      .filter({ has: page.getByRole('button', { name: '编辑', exact: true }) })
      .last()
    if (await anyRouteRow.count()) {
      const key = (await anyRouteRow.locator('td.mono').first().innerText()).trim()
      const port = (await anyRouteRow.locator('td.mono').last().innerText()).trim().split(':').pop()
      await anyRouteRow.getByRole('button', { name: '编辑', exact: true }).click()
      await edgeModal.getByRole('heading', { name: '编辑边缘路由', exact: true }).waitFor()
      if ((await edgeModal.getByLabel('域名前缀或完整域名').inputValue()) !== key) throw new Error('编辑没有回填路由键')
      if ((await edgeModal.locator('.nm-input-number__input').inputValue()) !== port) throw new Error('编辑没有回填端口')
      await page.screenshot({ path: '/tmp/esc-edge-edit.png', fullPage: true })
      await edgeModal.getByRole('button', { name: '取消', exact: true }).click()
      results.at(-1).edgeEditor = key
    }
  }
  if (errors.length) throw new Error(`${name}: 浏览器错误 ${errors.join('; ')}`)
  await page.close()
}
console.log(JSON.stringify(results, null, 2))
await browser.close()
