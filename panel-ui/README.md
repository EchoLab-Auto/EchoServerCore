# server-core 面板前端

两个 Rust Web 面板共用一套 Vue 3 + TypeScript 工程，真实组件来自
[`@echolab-auto/ui-frame`](https://github.com/EchoLab-Auto/ui-frame)。
`vendor/ui-frame` 是固定到 commit `8835574` 的 Git submodule，不是复制出来的另一套组件。

## 目录

- `src/TunnelPanel.vue`：映射、Spoke、边缘路由、配置导出。
- `src/ServerPanel.vue`：主机指标、容器、镜像、后台任务。
- `src/PluginTree.vue`：两个面板共用的宿主插件树。
- `src/ui.ts`：API、主题、格式化及 ui-frame 组件注册。
- `scripts/embed.mjs`：把构建产物写入两个 crate 的 `web/`。

## 重建

需要 Node.js 20+。先取回组件库，再安装并构建：

```bash
git submodule update --init --recursive panel-ui/vendor/ui-frame
cd panel-ui
npm install --install-links=true --no-audit --no-fund
npm run typecheck
npm run build
```

`npm install` 的 preinstall 与 `npm run build` 的 prebuild 都会构建 ui-frame；
`build` 随后执行 Vite 并把以下文件嵌入 Rust 二进制：

```text
crates/esc-plugin-tunnel-panel/web/{panel.css,panel.js,tunnel.js}
crates/esc-server-panel/web/{panel.css,panel.js,server.js}
```

`panel.js` 是 Vue、ui-frame 和共享插件树的 chunk；`tunnel.js` / `server.js`
是各自入口。chunk 的 hash 由 Rust 的静态资源映射处理，部署不依赖文件名中的 hash。
普通 `cargo build` **不调用 npm**：产物随仓库提交，生产运行不需要 Node、CDN 或外网。
只有改前端源码时需要上面的重建步骤。

## 浏览器烟测

启动两个面板后，按需修改 `smoke.mjs` 中的 URL，然后：

```bash
npx playwright install chromium
# 精简 Linux 环境可能还需：npx playwright install-deps chromium
npm run smoke
```

烟测检查真实 DOM、插件树、容器列表和新建映射弹窗，并收集浏览器错误。
