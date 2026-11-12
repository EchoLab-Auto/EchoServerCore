import { chromium } from '@playwright/test'

const EDIT = '\u7f16\u8f91'
const SAVE = '\u4fdd\u5b58'
const CANCEL = '\u53d6\u6d88'
const CHAIN = '\u94fe\u8def\u7ec8\u70b9'
const NET = '\u7f51\u7edc'
const TLS_TAB = 'TLS \u7ec8\u7ed3'

const ORIG = 'https://{hostname}:2224{uri}'
const TEMP = 'https://{hostname}:2224{uri}?from=inline'

const browser = await chromium.launch({
  executablePath: process.env.HOME + '/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome',
  args: ['--no-sandbox'],
})
const page = await browser.newPage({ ignoreHTTPSErrors: true })
page.on('pageerror', e => console.log('  pageerror:', String(e).slice(0, 300)))

const status = () => page.evaluate(async () => (await fetch('/api/tunnel/tls')).json())
const targetOf = (st, port) =>
  (st.sites || []).find(s => String(s.listen).endsWith(':' + port))?.target

await page.goto('http://127.0.0.1:18788/', { waitUntil: 'networkidle' })
await page.getByRole('tab', { name: new RegExp(NET) }).first().click()
await page.waitForTimeout(600)
await page.getByRole('tab', { name: TLS_TAB, exact: true }).click()
await page.waitForTimeout(1200)

console.log('  before 2223 =', targetOf(await status(), 2223))

async function editRow(port, value, tag) {
  const table = page.locator('table.esc-table:visible').filter({ hasText: CHAIN }).first()
  // 按 data-listen 定位：编辑后"监听"格变成 input，行内文本里就没有那个地址了，
  // 用 hasText 会立刻匹配不到（locator 是惰性求值的，每次操作都重新算）。
  const row = table.locator('tr[data-listen="0.0.0.0:' + port + '"]').first()
  await row.getByRole('button', { name: EDIT, exact: true }).click()
  const cancel = table.getByRole('button', { name: CANCEL, exact: true }).first()
  await cancel.waitFor({ state: 'visible' })

  const cell = row.locator('input[data-field="target"]').first()
  await cell.fill(value)
  await table.getByRole('button', { name: SAVE, exact: true }).first().click()
  await cancel.waitFor({ state: 'hidden', timeout: 15000 })
  const err = page.locator('.esc-form-error')
  if (await err.count()) {
    console.log('  [' + tag + '] error:', (await err.allInnerTexts()).join(' ').slice(0, 200))
  }
  await page.waitForTimeout(500)
  console.log('  [' + tag + '] after 2223 =', targetOf(await status(), 2223))
}

await editRow(2223, TEMP, 'set')
await editRow(2223, ORIG, 'restore')

const after = await status()
console.log('  final sites =', (after.sites || []).length, '2223 =', targetOf(after, 2223))
await browser.close()
