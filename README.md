# DeepChat Desktop

把 [chat.deepseek.com](https://chat.deepseek.com) 打包为原生桌面应用的 Tauri 2 项目。

## 功能

- **原生桌面体验**：基于 Tauri 2 + 系统 WebView（macOS WKWebView / Windows WebView2），启动快、体积小、占用低
- **全功能保留**：聊天、上传附件、对话历史、新建对话、设置等核心能力全部可用——所有 DeepSeek 网页端功能
- **登录态持久化**（双保险）：
  1. **原生 WebView 存储** —— macOS 默认使用 `WKWebsiteDataStore.defaultDataStore()`（持久化），Windows WebView2 自动持久化到用户配置目录。HttpOnly Cookie 由这一层负责
  2. **本地文件快照** —— `init.js` 定期将 Cookie + localStorage 同步到 `app_data_dir/session.json`，启动时恢复。覆盖原生存储被清理、换构建、系统清理等极端场景
- **窗口配置**：默认 1280×800，最小 1000×600，自由缩放
- **链接处理**：
  - 应用内（`*.deepseek.com`）的链接在应用内打开
  - 外部链接（含 OAuth 提供方、文档站、GitHub 等）自动跳转系统默认浏览器
  - `window.open` 与 `target="_blank` 已重定向/拦截
- **CORS / 跨域**：窗口直接加载 `https://chat.deepseek.com`，保持同源；不会触发 CORS 拦截；CORS 同源策略天然避免
- **跨平台**：macOS 10.15+ 与 Windows 10/11（需 WebView2 Runtime，已内置）
- **桌面环境兼容**：通过标准 Edge UA 与浏览器运行时特征适配，通过 chat.deepseek.com 的桌面端环境校验（详见下文说明）

## 桌面环境兼容说明

chat.deepseek.com 对非标准浏览器环境（如 Electron、Tauri WebView）有客户端校验，命中会提示“使用环境异常”。本应用通过 `src-tauri/src/lib.rs` 设置标准 Edge UA，并在 `document_start` 注入 `src-tauri/assets/spoof.js` 适配浏览器运行时特征，以通过该校验、正常使用全部功能。

> ⚠️ 免责声明：本项目是 Web 桌面壳（WebView wrapper）的通用适配实践，不包含对 chat.deepseek.com 服务或数据的任何破解、篡改。使用本应用前，请阅读并遵守 [chat.deepseek.com](https://chat.deepseek.com) 的服务条款，因违规使用产生的影响由使用者自行承担。

## 项目结构

```
deepchat-desktop/
├── package.json                      # @tauri-apps/cli
├── src/
│   └── index.html                    # 前端占位资源（实际加载 chat.deepseek.com）
├── src-tauri/
│   ├── Cargo.toml
│   ├── build.rs
│   ├── tauri.conf.json
│   ├── capabilities/default.json
│   ├── icons/                        # 各平台图标（自动生成）
│   ├── assets/
│   │   ├── spoof.js                   # 桌面环境适配（浏览器运行时特征处理）
│   │   ├── init.js                    # 会话持久化 + 链接处理注入脚本
│   │   └── selftest.js                # 可选自检脚本（仅在 DS_SELFTEST=1 时注入）
│   └── src/
│       ├── main.rs                   # 入口
│       ├── lib.rs                    # 窗口 / 命令注册
│       └── session.rs                # 会话文件读写命令
└── .github/workflows/build.yml       # 跨平台 CI 构建
```

## 开发与构建

### 前置条件

- Node.js ≥ 18（仅运行 `@tauri-apps/cli`）
- Rust stable（1.97+ 已验证）
- **macOS**：Xcode Command Line Tools（`xcode-select --install`）
- **Windows**：Microsoft C++ Build Tools 与 WebView2 Runtime

### 安装与运行

```bash
# 安装 Tauri CLI
npm install

# 开发模式（热重载，构建后启动桌面应用）
npm run tauri dev

# 生产构建（macOS 产出 .app 与 .dmg；Windows 产出 .exe 与 .msi/.exe 安装包）
npm run tauri build
```

### 跨平台构建

当前机器不能直接产出其他平台安装包。建议在 macOS 上跑 macOS 构建，在 Windows 上跑 Windows 构建；或用 GitHub Actions 跨平台构建（见 `.github/workflows/build.yml`）。

```bash
# 仅编译当前平台的 release 二进制（不带安装包）
cd src-tauri && cargo build --release

# 仅构建 .app（macOS）
npm run tauri build -- --bundles app

# 仅构建 dmg（macOS）
npm run tauri build -- --bundles dmg
```

### 自检模式（验证持久化）

```bash
# 第一次启动（写入测试标记 + 日志）
DS_SELFTEST=1 ./src-tauri/target/release/bundle/macos/DeepChat\ Desktop.app/Contents/MacOS/deepchat-desktop
# 退出后日志位置：~/Library/Application Support/com.deepchat.desktop/debug.log
cat ~/Library/Application\ Support/com.deepchat.desktop/debug.log

# 再次启动（应看到 marker_old= 上次的 new 值 → 原生 localStorage 已持久化）
DS_SELFTEST=1 ./src-tauri/target/release/bundle/macos/DeepSeek\ Desktop.app/Contents/MacOS/deepseek-desktop
```

会话快照路径：`~/Library/Application Support/com.deepchat.desktop/session.json`

## 关键设计说明

### 1. 登录态持久化（双保险）

```
┌──────────────────────────────────────────────────────────┐
│  chat.deepseek.com 页面                                   │
└──────────────────────────────────────────────────────────┘
                       ↕ invoke
┌──────────────────────────────────────────────────────────┐
│  init.js（document_start 注入）                           │
│  - restoreOnce()：从 session.json 恢复 Cookie/LS          │
│  - 5s 定时 / 页面隐藏 / 卸载前：sync → save_session        │
│  - 点击拦截：内部链接 in-app、外部链接 open_external       │
│  - window.open 重定向                                     │
└──────────────────────────────────────────────────────────┘
                       ↕ 写入 app_data_dir
┌──────────────────────────────────────────────────────────┐
│  session.json（JSON 快照，原子写入）                       │
│  { localStorage: {…}, cookies: "…", savedAt: … }          │
└──────────────────────────────────────────────────────────┘
                      ↕（主路径：原生 WebView）
┌──────────────────────────────────────────────────────────┐
│  WKWebView.defaultDataStore / WebView2 Profile           │
│  Cookie（含 HttpOnly）+ localStorage 全量持久化            │
└──────────────────────────────────────────────────────────┘
```

- **macOS**：wry 默认使用 `WKWebsiteDataStore::defaultDataStore()`（持久化）。源验证：`tauri-apps/wry` 的 `src/wkwebview/mod.rs`，`match (incognito, …)` 的非隐身分支落 `defaultDataStore`
- **Windows**：WebView2 自动持久化到 `%LOCALAPPDATA%\com.deepchat.desktop\EBWebView\`
- 文件快照层：覆盖原生层被清空的极端场景；同时作为可导出的备份

### 2. 链接处理

```js
// init.js（核心）
document.addEventListener('click', e => {
  const a = e.target.closest('a');
  if (!a) return;
  const url = new URL(a.href, location.href);
  if (!/\.deepseek\.com$/.test(url.hostname)) {
    e.preventDefault();
    invoke('open_external', { url });  // Rust: open::that(url) → 系统浏览器
  }
}, true);
```

`on_navigation` 仅作为安全护栏——只放行 http/https，拦截 `data:` / `file:` / `javascript:`。OAuth 重定向和页面内 JS 跳转保持原行为（不被误拦）。

### 3. CORS / 跨域

窗口直接加载 `https://chat.deepseek.com`，所有页面内 API 请求同源，无 CORS。`tauri.conf.json` 中 `csp: null` 关闭 Tauri 的 CSP 头（远程页不会被注入限制性 CSP）。

### 4. 远程域 IPC（持久化桥所必需）

`src-tauri/capabilities/default.json`：
```json
{
  "windows": ["main"],
  "remote": { "urls": ["https://chat.deepseek.com", "https://api.deepseek.com"] },
  "permissions": ["core:default", "core:window:default", "core:webview:default", "core:event:default"]
}
```

这是 init.js 能从 chat.deepseek.com 调用 `save_session` / `load_session` 的前提。仅信任 deepseek 主域，未授权的远程域完全无法访问 IPC。

## 已知限制

- **OAuth / 第三方登录**：如果 DeepSeek 引入外部 OAuth（目前主要是手机号/邮箱 + 验证码），外部授权页因 `on_navigation` 的 http/https 放行保留在应用内（与系统浏览器打开会中断回调）。如需外部跳转，扩展 `on_navigation` 屏蔽规则
- **文件下载**：Tauri 2 当前对 WebView 下载的接管有限；WKWebView 下载事件默认静默，WebView2 走系统下载目录。如需自定义保存对话框，监听 `WebviewDownloadEvent`（需要 Tauri 主线 nightly/next 编译选项）
- **会话文件大小**：单值 > 512KB 或总快照 > 8MB 会被忽略（init.js 跳过）；超大会触发 `save_session` 拒绝
- **macOS 上 HttpOnly Cookie**：依赖原生 WebView 存储的持久化。如果用户手动清空 WebKit 网站数据，原生 Cookie 会丢失但文件快照仍可恢复部分（仅 JS 可见的 Cookie）。最坏情况是要求重新登录

## 许可与声明

- 本仓库代码基于 [MIT](LICENSE) 协议开源。
- 本项目为独立的第三方开源项目，与 DeepSeek 官方无任何隶属或授权关系；项目不使用 DeepSeek 官方标识（图标为原创手绘）。
- 项目名 "DeepChat" 与 DeepSeek 品牌无关。使用本应用时请遵守 [chat.deepseek.com](https://chat.deepseek.com) 的服务条款。

## 免责声明

本应用为 WebView 桌面壳的通用技术实践，仅供学习与个人使用。会话快照（Cookie/localStorage）以明文形式存储在本地磁盘，请勿在共享设备上使用。用户需自行确保使用行为符合目标网站的服务条款及相关法律法规；因使用本应用产生的任何问题与作者无关。