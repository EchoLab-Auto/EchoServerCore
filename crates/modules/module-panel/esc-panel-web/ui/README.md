# esc-panel-web — 两个面板的静态前端

同仓库的两个 Web 面板（隧道面板 `:2223` / 服务器面板 `:2224`）共用这一套
Vue 3 + TypeScript 工程。真实组件来自
[`@echolab-auto/ui-frame`](https://github.com/EchoLab-Auto/ui-frame)：
`ui/vendor/ui-frame` 是固定到 commit `8835574` 的 Git submodule，不是复制品。

## 这个 crate 是什么

```
crates/modules/module-panel/esc-panel-web/
├── src/lib.rs          ← Rust 侧：路径 → 资源、HTML 外壳（零依赖）
├── build.rs            ← 无（不跑 npm，见下）
├── ui/                 ← 前端工程（源码、submodule、npm 工程）
│   ├── src/*.vue *.ts
│   ├── scripts/embed.mjs   ← 构建产物写回 ../web/ 并写 .build-stamp
│   └── vendor/ui-frame     ← submodule
└── web/                ← **构建产物，随仓库提交**
    ├── panel.css  panel.js        ← 公共 chunk（两个面板共用）
    ├── tunnel.js  server.js       ← 两个入口
    └── .build-stamp               ← ui/src 的内容哈希
```

两个面板各自只留一个 8 行的 `web.rs`，声明自己是哪个入口。

**为什么要收进一个 crate**：此前产物被复制进两个 crate 各自的 `web/`，于是
`escape_title` / `Asset` / 路径映射各写一遍（逐字相同），168 KB 的公共 chunk
在仓库里存了两份，"改前端"要记得往两处各写一遍。

## 产物为什么入库（以及怎么防漂移）

`cargo build` **绝不调用 npm**：Docker 构建、CI、离线机器只有 Rust 工具链。
所以 `web/` 里的文件是提交的构建产物——生产运行不需要 Node、CDN 或外网。

代价是漂移风险：改了 `ui/src/` 却忘了重建，源码与线上行为不一致而没人报错。
防线是内容哈希而不是时间戳（`git clone` 出来的 mtime 全一样，只会假警报）：

- `ui/scripts/embed.mjs` 每次构建把 `ui/src` 的全量内容算成 FNV-1a 写入 `web/.build-stamp`；
- `src/lib.rs` 的 `artifacts_are_fresh` 测试用同一算法重算并比对，不一致就红。

算法两侧必须一致（FNV-1a 64，按相对路径排序，逐文件喂 `path\0content\0`，
分隔符统一 `/`）。改任何一侧都要同步改另一侧。

## 重建

需要 Node.js 20+。先取回组件库，再安装并构建：

```bash
git submodule update --init --recursive crates/modules/module-panel/esc-panel-web/ui/vendor/ui-frame
cd crates/modules/module-panel/esc-panel-web/ui
npm install --install-links=true --no-audit --no-fund
npm run typecheck
npm run build
```

`npm install` 的 preinstall 与 `npm run build` 的 prebuild 都会构建 ui-frame；
`build` 随后执行 Vite 并把四个文件与戳记写进 `../web/`：

```text
crates/modules/module-panel/esc-panel-web/web/{panel.css,panel.js,tunnel.js,server.js,.build-stamp}
```

`panel.js` / `panel.css` 是 Vue、ui-frame 和共享插件树的 chunk；`tunnel.js` /
`server.js` 是各自入口。chunk 的文件名 hash 由 Rust 侧的静态资源映射处理，
部署不依赖文件名里的 hash。

## 浏览器烟测

启动两个面板后，按需修改 `smoke.mjs` 里的 URL，然后：

```bash
npx playwright install chromium
# 精简 Linux 环境可能还需：npx playwright install-deps chromium
npm run smoke
```

烟测检查真实 DOM、插件树、容器列表和新建映射弹窗，并收集浏览器错误。
