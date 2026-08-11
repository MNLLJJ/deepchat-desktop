# DeepChat Desktop 项目完整审查报告

> 审查日期：2026-08-11 ｜ 基线：v1.0.0（含近期修复：init.js 重写 / session.rs 防污染 / lib.rs 看门狗 / capabilities 白名单）
> 轻量化基准：macOS aarch64 二进制 **5.4MB**、DMG **~2.5MB**；依赖仅 `tauri(2.11.5) + serde + serde_json + url`，无重型框架。
> 原则：每一项增强/修复均标注 **价值 / 体积·性能代价 / 开关方式**，确保默认构建保持轻量，重能力按需启用。
>
> **实施状态**：P0 六项 ✅（2026-08-11 完成）；P1-7 / P1-9 / P1-10 ✅（2026-08-11 完成，均 feature 隔离）；其余待办见下方标注。

---

## 一、需要修复的问题（按优先级从高到低）

### P0-1 单实例锁缺失（Windows 多开争用 WebView2 profile → 转圈）
- **问题**：应用无单实例保护。Windows 上双击两次启动，两个进程争用同一个 WebView2 user-data-folder（`%APPDATA%\com.deepchat.desktop\EBWebView`）的 profile 锁，后开者会长时间白屏/转圈，且强杀后可能损坏 profile —— 与用户上报的"长时间转圈 + 重启登录失效"高度相关。
- **价值**：直接消除一类真实故障；顺带获得"唤起已运行实例"能力（二次启动时聚焦已有窗口）。
- **体积/性能代价**：≈0（运行时仅一次互斥检查；编译增量 ~百 KB 级）。
- **开关方式**：Tauri 官方 `tauri-plugin-single-instance`，作为 feature 默认启用，无 UI 开关；或自实现（Windows `CreateEvent` + `FindWindow` 聚焦，macOS `NSRunningApplication`），零额外依赖。

### P0-2 UA / 指纹版本硬编码，长期可用性风险
- **问题**：`lib.rs` 中 UA 硬编码 `Chrome/138.0.0.0 ... Edg/138.0.0.0`；`spoof.js` 中 `userAgentData` 的品牌版本、`platformVersion`（Windows 写死 `15.0.0`）、架构（写死 `x86`/`bitness:64`）全部硬编码。一旦 DeepSeek 的客户端环境校验更新（UA 版本过旧/高熵指纹异常），整个应用会集体弹"使用环境异常"，无法使用。
- **价值**：应用存续的关键保障；架构位元错误在 ARM 设备（Windows ARM64 / Apple Silicon 已正常但标注 x86）上可能被高熵检测识破。
- **体积/性能代价**：0（纯常量/逻辑调整）。
- **开关方式**：编译期 `env!("DS_UA")` 可覆盖 + 默认值自动生成；或读运行时配置文件（见 E-P1 设置持久化）。建议至少把版本号集中为一个常量便于升级。

### P0-3 Windows 外部链接走 PowerShell，慢且脆弱
- **问题**：`open_in_system_browser`（Windows 分支）每次调用 `powershell -Command Start-Process '...'`：每次启动 PowerShell 进程约 200–500ms，且被企业策略禁用 PowerShell / 受限环境会静默失败；URL 含特殊字符时依赖转义正确性。
- **价值**：点外部链接的响应速度与可靠性。
- **体积/性能代价**：`windows` crate 直接 `ShellExecuteW`（增量编译约百 KB），或引入 `open` crate（~几百 KB 编译）；运行时开销降到微秒级。
- **开关方式**：平台原生分支，无需开关。

### P0-4 会话快照残留与上限不一致
- **问题**：① `save_session` 原子写入的 `session.json.tmp` 在进程被强杀时会残留，从不清理；② `init.js` 上限（单值 512KB / 总量 8MB）与 `session.rs`（总量 10MB）不一致，边界行为易混淆；③ 崩溃残留的 tmp 不会被读取，但占磁盘。
- **价值**：健壮性/可维护性，成本极低。
- **体积/性能代价**：0。
- **开关方式**：无需开关；上限统一为 8MB 常量并在两处引用同一语义（注释对齐）。

### P0-5 远程 IPC 缺少 Rust 侧 origin 复核（纵深防御）
- **问题**：`capabilities` 白名单扩为 `*.deepseek.com` 后，DeepSeek 任一子域被攻破/被注入 XSS 即可直接调用 `save_session/load_session/clear_session/open_external`（可读取、覆盖、删除本地快照，可让系统浏览器打开任意 URL）。capability 是入口闸门，但没有第二道校验。
- **价值**：安全纵深；尤其 `open_external` 可被恶意页面利用做钓鱼跳转。
- **体积/性能代价**：~20 行 Rust（每个命令入口校验 `webview.url()` 的 host 以 `deepseek.com` 结尾且为 https）。
- **开关方式**：无需开关，恒生效。

### P0-6 托盘关闭行为在 Windows 的用户感知（未提交改动）
- **问题**：托盘改动（未提交）拦截 `CloseRequested`，Windows 用户点关闭按钮后窗口隐藏到托盘、进程常驻 —— 用户若看不到托盘图标（Windows 托盘折叠区）会以为应用消失；且无首次提示。
- **价值**：避免"关闭后找不到应用"的困惑与投诉。
- **体积/性能代价**：~10 行（首次隐藏时 `show_notification` 或托盘图标气泡提示）。
- **开关方式**：托盘 feature 内联；提示仅首次（localStorage 或配置文件标记）。

### P1-7 HttpOnly Cookie 无法从文件快照恢复（架构短板）✅ 已实现（feature `full-cookie-snapshot`）
- **问题**：登录态核心 Cookie 是 HttpOnly，`document.cookie` 读写不到，文件快照层只能兜底非 HttpOnly 部分。Windows 上 WebView2 profile 异常损坏/被清理时登录必失，无法自愈。
- **实现**：新增 `dump_all_cookies` / `restore_all_cookies` 命令（init.js 运行时探测，feature 未启用时自动回退 document.cookie）。Windows 走 `webview2-com` CookieManager（含 HttpOnly 全属性）；macOS 走 WKHTTPCookieStore + NSHTTPCookie（Foundation 键不含 HttpOnly/SessionOnly，恢复时这两个标志丢失，登录态仍可恢复；Windows 无此限制）。build.rs 按 feature 注册 ACL。
- **体积/性能代价**：≈0 体积（编译期 FFI）；运行时每次保存多一次 CookieManager 查询（毫秒级）。
- **开关方式**：`cargo build --features full-cookie-snapshot`；默认构建完全不受影响。
- ⚠️ Windows 分支（webview2-com FFI）无法在 macOS 编译验证，需 CI windows-latest 验证后发布。

### P1-8 无自动更新，修复版本无法触达用户
- **问题**：v1.0.0 已发布，但所有修复（含本次）都要用户手动去 Release 下载，无任何更新提示；安全/兼容问题无法及时触达。
- **价值**：运维闭环；配合 GitHub Release 静态 JSON 即可零服务器成本。
- **体积/性能代价**：`tauri-plugin-updater` 编译增量数 MB、安装包 +~1MB；需要代码签名密钥（Windows 非强制但 SmartScreen 体验更佳）。
- **开关方式**：feature `updater` + `tauri.conf.json` `bundle.updater` 配置；发布构建 `--features updater` 启用，日常 dev 不编译。

### P1-9 会话快照明文存储 ✅ 已实现（feature `encrypt-session`）
- **问题**：`session.json` 含 Cookie/localStorage（可能含 token），明文落盘；共享电脑/备份同步场景有泄露面。
- **实现**：落盘内容加 `DSENC1:` 前缀密文，旧明文快照自动识别兼容（向后兼容，无需迁移）。Windows 走 DPAPI（当前用户上下文）；macOS 走 Keychain 存 AES-256-GCM 密钥；其他平台返回不支持（保存报错，前端回退）。
- **体积/性能代价**：`aes-gcm + rand + security-framework(仅 macOS)`（编译增量小）；读写各一次加解密（微秒~毫秒级）。
- **开关方式**：`cargo build --features encrypt-session`；默认构建完全不受影响。
- ⚠️ Windows DPAPI 分支需 CI windows-latest 验证。

### P1-10 离线/加载失败时白屏无提示 ✅ 已实现
- **问题**：网络不可用 / chat.deepseek.com 故障时，窗口长时间白屏或转圈（看门狗最多 reload 2 次后放弃），用户得不到任何"网络异常"提示。
- **实现**：`src/index.html` 支持 `?offline=1` 错误页模式（自动探测网络 + 重试按钮 + 每 15s 自动重试，恢复后自动跳回）；看门狗第 3 轮（约 60s）仍失败时导航到本地错误页而非无限 reload；`on_navigation` 放行本地协议（tauri://localhost / http://tauri.localhost）。
- **体积/性能代价**：仅本地静态页 + ~20 行 Rust，运行时 0。
- **开关方式**：无需开关，恒生效。

---

## 二、可添加的增强功能（按优先级从高到低）

### E-P0 托盘菜单扩展：强制刷新 / 清除会话 / 显示日志
- **价值**：给用户自助恢复入口 —— 卡死时"强制刷新"、登录异常时"清除本地会话"（等价于清缓存重登），显著降低故障上报量；同时给开发留调试出口（打开 app_data_dir 日志）。
- **体积/性能代价**：~40 行 Rust（菜单项 + 已有命令），体积 0。
- **开关方式**：托盘 feature 内联，无额外开关。

### E-P1 极简设置持久化（UA 可配置 / 启动行为 / 数据目录）
- **价值**：把 P0-2 的硬编码 UA 变为可配置；支持"便携版"（`--user-data-dir` 参数）与多配置共存；为 E-P3 做准备。
- **体积/性能代价**：一个 `settings.json`（app_data_dir）+ ~50 行；启动读一次，性能 0。
- **开关方式**：默认启用；`--user-data-dir` 仅 CLI 传参时生效。

### E-P1 离线错误页（见 P1-10，功能属性）
- 说明：兼具修复与增强属性，见上。

### E-P2 全局快捷键（显示/隐藏主窗口）
- **价值**：托盘常驻模式下（macOS 菜单栏 / Windows 托盘）快速唤起窗口，符合"收纳型"应用使用习惯。
- **体积/性能代价**：`tauri-plugin-global-shortcut`（编译增量 ~1MB 内，运行 0）。
- **开关方式**：feature `global-shortcut` 默认关；启用后默认绑定 `CmdOrCtrl+Shift+D`，托盘菜单显示当前绑定。

### E-P2 崩溃日志与上次退出状态记录
- **价值**：Windows 异常退出（强杀/崩溃）后，下次启动可提示"上次未正常退出，已尝试自愈"，并沉淀 crash.log 供排查 —— 与 P0-1/转圈问题形成完整闭环。
- **体积/性能代价**：panic hook + 退出标记文件，~30 行，0 体积。
- **开关方式**：release 默认启用（debug 不写），无需用户开关。

### E-P2 URL 协议注册（`deepchat://`）
- **价值**：从浏览器/其他应用唤起应用（如打开指定会话），未来可能的 deep link 入口。
- **体积/性能代价**：Windows 注册表 / macOS Info.plist + ~30 行；安装包略微变大（manifest）。
- **开关方式**：feature `deep-link`（默认关），或仅 Windows 构建启用。

### E-P3 开机自启
- **价值**：托盘常驻类应用的常见需求（类微信/QQ 常驻）。
- **体积/性能代价**：`tauri-plugin-autostart`，很小。
- **开关方式**：feature `autostart` 默认关；托盘菜单加"开机自启"勾选（写配置）。

### E-P3 多标签/多窗口（不推荐）
- **价值**：多账号/多会话并排；与壳应用定位冲突，复杂度高（WebView 实例管理、session 隔离）。
- **体积/性能代价**：多 WebViewWindow，内存随窗口数线性增长；违背轻量原则。
- **开关方式**：如确需，仅做"新窗口打开"（复用现有 WebviewWindowBuilder，session 共享），不做标签栏 UI；**建议不做**。

---

## 三、其他维护性/安全观察（不构成独立优先级）

- **capabilities 通配确认**：`*.deepseek.com` 已覆盖登录跳转域（必须），配合 P0-5 的 Rust 侧复核后风险可控。
- **crate-type 冗余**：`staticlib/cdylib/rlib` 是为 iOS/Android 预留；无移动端计划可减为 `rlib`，缩短编译时间（体积几乎不变）。低优先级。
- **CI 并发限制**：`build.yml` 无 `concurrency`，多 tag push 可能并行跑重复构建浪费 Actions 分钟（记忆日志中标记的中风险遗留项）。+3 行。
- **README 与托盘改动同步**：托盘功能文档缺失（未提交），发布前补齐。

---

## 四、整体优先级建议清单（发布路线图）

| 优先级 | 项目 | 类别 | 体积/性能代价 | 开关/feature | 建议 |
|---|---|---|---|---|---|
| **P0** | 单实例锁 | 修复 | ≈0 | 插件，默认开 | 下个补丁版本必做 |
| **P0** | UA/指纹版本集中化（防失效） | 修复 | 0 | 编译常量/配置 | 下个补丁版本必做 |
| **P0** | Windows 外部链接改 ShellExecuteW | 修复 | ~百KB | 平台原生 | 下个补丁版本必做 |
| **P0** | 快照 tmp 残留清理 + 上限统一 | 修复 | 0 | — | 顺手做 |
| **P0** | IPC Rust 侧 origin 复核 | 修复 | ~20行 | 恒生效 | 顺手做 |
| **P0** | 托盘关闭行为 Windows 提示 | 修复 | ~10行 | 托盘 feature | 随托盘一起发 |
| **P1** | HttpOnly Cookie 全量快照 | 修复 | FFI ~200行，体积≈0 | `full-cookie-snapshot` 默认关 | Windows 先行，v1.1 |
| **P1** | 自动更新 | 增强 | 安装包 +~1MB | `updater` 默认关 | v1.1（需签名） |
| **P1** | 会话快照加密 | 增强 | ~百KB | `encrypt-session` 默认关 | v1.1 可选 |
| **P1** | 离线/加载失败提示页 | 修复+增强 | ~30行 | — | 与看门狗配合 |
| **P2** | 托盘菜单：强制刷新/清会话/日志 | 增强 | ~40行 | 托盘 feature | v1.1 |
| **P2** | 极简设置持久化（含数据目录参数） | 增强 | ~50行 | 默认开 | v1.2 |
| **P2** | 崩溃日志 + 退出状态标记 | 增强 | ~30行 | release 默认 | v1.1 |
| **P2** | 全局快捷键 | 增强 | ~1MB 编译 | `global-shortcut` 默认关 | v1.2 可选 |
| **P2** | URL 协议注册 | 增强 | 小 | `deep-link` 默认关 | v1.2 可选 |
| **P3** | 开机自启 | 增强 | 小 | `autostart` 默认关 | 可选 |
| **P3** | 多标签/多窗口 | 增强 | 高 | — | **不建议** |
| — | CI concurrency / crate-type 精简 / README 同步 | 维护 | 0 | — | 随发布顺手 |

**默认构建（`tauri build` 不带任何 feature）应保持体积 ≈ 现状（~5.4MB 二进制）**；上表中所有"重"能力均以 feature 形式按需启用，不破坏轻量化原则。

---

## 五、结论

- **轻量化现状优秀**：依赖面极小、无前端框架、无重型插件，5.4MB 二进制在同类壳应用中属于第一梯队，增强空间充足。
- **最优先动作（P0 六项）**：全部为低代价修复，其中**单实例锁**与**UA 集中化**直接关系"转圈/登录失效/环境异常"三类已见故障，建议与当前修复一并进入下个补丁版本。
- **P1 三项**（HttpOnly 全量快照、自动更新、快照加密）是"能力完整化"的关键，均可用 feature 隔离，默认不破坏轻量。
- **P2/P3** 为体验增强，按需启用即可；多标签等重功能建议明确不做。
