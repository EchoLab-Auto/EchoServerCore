import { readdir, readFile, writeFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import path from 'node:path'

// 前端工程在 crates/esc-panel-web/ui/，产物写进同 crate 的 web/。
const ui = path.dirname(path.dirname(fileURLToPath(import.meta.url)))
const crate = path.dirname(ui)
const dist = path.join(ui, 'dist')
const out = path.join(crate, 'web')

const assets = await readdir(path.join(dist, 'assets'))
const shared = assets.find(name => /^panel-.*\.js$/.test(name))
if (!shared) throw new Error('找不到 ui-frame 共享 chunk')

// 产物文件名是固定的（部署不依赖 hash）：公共 chunk → panel.js，入口 → <entry>.js。
await writeFile(path.join(out, 'panel.js'), await readFile(path.join(dist, 'assets', shared)))
await writeFile(path.join(out, 'panel.css'), await readFile(path.join(dist, 'panel.css')))

for (const entry of ['tunnel', 'server']) {
  const code = await readFile(path.join(dist, `${entry}.js`), 'utf8')
  // 浏览器按 /ui/ 取资源，把相对路径改写成绝对路径。
  await writeFile(
    path.join(out, `${entry}.js`),
    code.replaceAll('./assets/panel-', '/ui/assets/panel-'),
  )
}

// 内容戳：让 `cargo test` 能发现「改了 ui/src 却没重建」。
// 算法必须与 crates/esc-panel-web/src/lib.rs 的 hash_tree 一致：
// FNV-1a 64，按相对路径排序，逐文件喂 path\0content\0（路径分隔符统一为 /）。
await writeFile(path.join(out, '.build-stamp'), `${await hashTree(path.join(ui, 'src'))}\n`)

console.log(`已写入 ${path.relative(process.cwd(), out)}/（panel.css, panel.js, tunnel.js, server.js, .build-stamp）`)

async function hashTree(dir) {
  const files = await collect(dir)
  files.sort()
  let h = 0xcbf29ce484222325n
  const prime = 0x100000001b3n
  const mask = 0xffffffffffffffffn
  const feed = (hash, bytes) => {
    for (const byte of bytes) {
      hash ^= BigInt(byte)
      hash = (hash * prime) & mask
    }
    return hash
  }
  for (const file of files) {
    const rel = path.relative(dir, file).split(path.sep).join('/')
    h = feed(h, Buffer.from(rel, 'utf8'))
    h = feed(h, Buffer.from([0]))
    h = feed(h, await readFile(file))
    h = feed(h, Buffer.from([0]))
  }
  return h.toString(16).padStart(16, '0')
}

async function collect(dir) {
  const out = []
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) out.push(...(await collect(full)))
    else out.push(full)
  }
  return out
}
