import { readdir, readFile, writeFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)))
const dist = path.join(root, 'dist')
const assets = await readdir(path.join(dist, 'assets'))
const shared = assets.find(name => /^panel-.*\.js$/.test(name))
if (!shared) throw new Error('找不到 ui-frame 共享 chunk')

const targets = [
  ['../crates/esc-plugin-tunnel-panel/web', 'tunnel'],
  ['../crates/esc-server-panel/web', 'server'],
]
for (const [target, entry] of targets) {
  await writeFile(path.join(root, target, 'panel.js'), await readFile(path.join(dist, 'assets', shared)))
  await writeFile(path.join(root, target, 'panel.css'), await readFile(path.join(dist, 'panel.css')))
  const code = await readFile(path.join(dist, `${entry}.js`), 'utf8')
  await writeFile(path.join(root, target, `${entry}.js`), code.replaceAll('./assets/panel-', '/ui/assets/panel-'))
}
console.log('已把 ui-frame 构建产物嵌入两个 Rust 面板 crate')
