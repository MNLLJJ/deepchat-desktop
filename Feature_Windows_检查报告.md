# DeepChat Desktop — 新 Feature 在 Windows 上的工作情况检查报告

**检查日期**:2026-08-11
**检查对象**:`encrypt-session`(会话快照加密)与 `full-cookie-snapshot`(HttpOnly Cookie 全量快照)两个 Cargo Feature
**检查环境**:Windows(x86_64, Rust 1.97.1, tauri 2.11.5 / wry 0.55.1 / webview2-com 0.38.2 / windows 0.61.3)

---

## 一、检查结论(摘要)

**两个 Feature 此前只在 macOS 上编译验证过,在 Windows 上原本完全不可用**,存在 4 类问题:

| # | 问题 | 严重度 | 状态 |
|---|------|--------|------|
| 1 | `encrypt-session` 编译失败(5 个错误,DPAPI 旧 API) | 高 | ✅ 已修复 |
| 2 | `full-cookie-snapshot` 编译失败(10+ 错误,webview2-com 0.38 API 不符) | 高 | ✅ 已修复 |
| 3 | ACL 权限缺失(`allow-dump-all-cookies`/`allow-restore-all-cookies` 未加入 capability) | 高 | ✅ 已修复 |
| 4 | **运行时死锁**:dump 在 sync 命令主线程等待 WebView2 回调 → 重入挂起(实测) | 致命 | ✅ 已修复 |

**修复后验证**:4 种 Feature 组合编译 0 错误 0 警告;前端模拟测试 15/15 通过;**真机运行验证通过**——DPAPI 加密落盘可被标准 DPAPI 解密、dump 成功读出含 HttpOnly 的 5 条 Cookie、清空 WebView2 profile 后 restore 成功从快照恢复 5 条。

---

## 二、问题详情与修复

### 问题 1:encrypt-session 编译失败 — windows 0.61 API 大改

旧代码基于 windows crate 0.52 时代的 API,0.61 中:

- `DATA_BLOB` 更名为 **`CRYPT_INTEGER_BLOB`**
- `LocalFree` 从 `Win32::System::Memory` 移到 **`Win32::Foundation`**,签名变为 `LocalFree(hmem: Option<HLOCAL>) -> HLOCAL`(`HLOCAL` 为 newtype)
- `CryptUnprotectData` 第 2 参 `ppszdatadescr` 改为 `Option<*mut PWSTR>`(传 `None`)

**修复**(`src-tauri/src/session.rs` cipher 模块):适配新签名;`Cargo.toml` 的 windows features 改为 `Win32_Security_Cryptography + Win32_Foundation`(移除已失效的 `Win32_System_Memory`)。

### 问题 2:full-cookie-snapshot 编译失败 — webview2-com 0.38 API 与旧代码不符

对照 wry 0.55.1 同版本参考实现(wry 已内置同一套 CookieManager 读写):

- **`CookieManager()` 方法位于 `ICoreWebView2_2`**(旧代码用 `ICoreWebView2::GetCookieManager()`,0.38 中不存在),需 `core.cast::<ICoreWebView2_2>()`
- 异步方法名为 **`GetCookies(uri, handler)`**(不是 `GetCookiesAsync`)
- Cookie 属性 getter 全部为 **out 参数形式**(`Name(&mut PWSTR)`、`Expires(&mut f64)`、`IsHttpOnly(&mut BOOL)` 等),不再是返回值形式
- 属性 setter 为 `SetIsHttpOnly(bool)` / `SetIsSecure(bool)` / `SetExpires(f64)`(旧代码用 `put_*` + `BOOL`)
- **`AddOrUpdateCookie(cookie)` 是同步单参方法**(无完成回调),restore 无需异步等待
- `GetCookiesCompletedHandler::create(Box::new(|error_code: Result<()>, cookies: Option<...>| ...))` 工厂替代手写 `#[implement]`(宏闭包第一参由 HRESULT 自动转为 `Result<()>`)

**修复**:`win_cookie` 模块整体重写,采用与 wry 一致的正确 API。

### 问题 3:ACL 权限缺失 — 功能"编译能过但运行被拒"

`capabilities/default.json` 只有 5 个会话命令的权限,缺 `allow-dump-all-cookies` / `allow-restore-all-cookies`。Tauri 2 默认拒绝远程域 IPC,远程页面(chat.deepseek.com)调用这两个命令会被 ACL 拒绝。

同时 `build.rs` 原来按 feature 条件注册命令,导致权限定义随 feature 存在/消失,capability 无法静态引用。

**修复**:
- `build.rs`:7 个命令**无条件注册**(feature 关闭时命令未注册,远程调用报 "command not found",前端 `probeFullCookie` 自动回退 document.cookie——回退路径本就是为此设计的,行为一致)
- `capabilities/default.json`:补上两个权限

### 问题 4(致命):运行时死锁 — WebView2 重入

**现象**:应用启动后 `dump_all_cookies` 打印"开始"后 25 秒无结果,进程挂起。

**根因**(真机日志 + 微软官方文档确认):
- Tauri 的 sync 命令在主线程执行,而主线程的 IPC 处理发生在 **WebView2 `WebMessageReceived` 事件处理器调用栈内**
- 在事件处理器内等待异步完成回调,构成 WebView2 **不支持的重入(嵌套消息循环)**,完成回调永远不被投递 → 死等
- 微软文档原文:"在事件处理器中同步创建嵌套消息循环会导致 WebView2 不支持的重入,事件处理器会无限期留在堆栈中"

**修复**(`dump_all_cookies` 命令):改为 **async 命令 + `spawn_blocking`**:
1. `with_webview` 闭包(注册 `GetCookies`)由 Tauri 投递到**空闲的主线程**执行——不再处于事件处理器嵌套栈内
2. blocking 线程用 `recv_timeout` 等待回调结果,主线程消息泵正常运转,回调正常投递并触发 handler 发送 channel
3. 附带的 `pump_wait`(嵌套消息循环方案)经实测被证明不可行,已删除

`restore_all_cookies` 保持 sync 命令:内部全部为同步 COM 调用(无完成回调),实测无重入问题。

---

## 三、修改文件清单

| 文件 | 修改内容 |
|------|----------|
| `src-tauri/src/session.rs` | DPAPI 适配 windows 0.61 新 API;`win_cookie` 模块按 webview2-com 0.38 API 重写;`dump_all_cookies` 命令 async 化 + spawn_blocking |
| `src-tauri/Cargo.toml` | windows features:`Win32_Security_Cryptography + Win32_Foundation` |
| `src-tauri/build.rs` | 7 个命令无条件注册(ACL 权限稳定) |
| `src-tauri/capabilities/default.json` | 补 `allow-dump-all-cookies`、`allow-restore-all-cookies` |

---

## 四、验证结果

### 1. 编译验证(4 种 Feature 组合)
`cargo check`:`default` / `encrypt-session` / `full-cookie-snapshot` / `encrypt-session,full-cookie-snapshot` **全部 0 错误 0 警告**。

### 2. 前端运行时行为(node + vm 模拟,15/15 通过)
覆盖 feature 开/关两态:命令不存在自动回退 document.cookie、全量恢复走 `restore_all_cookies`、恢复失败静默、空态防污染、幽灵 Cookie 黑名单、内容指纹去重等。

### 3. 真机运行验证(Windows,encrypt+full 同时开启)
- **进程稳定性**:启动 30 秒无挂起、无崩溃
- **DPAPI 加密**:`session.json` 以 `DSENC1:` 落盘,PowerShell 标准 DPAPI(CurrentUser)可解密 → 加密格式正确且互操作
- **dump(HttpOnly 全量导出)**:成功读出 **5 条 Cookie,含 `HWWAFSESID`(HttpOnly=true, Secure=true)**——这正是 document.cookie 拿不到、只有原生 CookieManager 能读的登录相关 Cookie
- **restore(端到端恢复)**:备份并清空 WebView2 profile(EBWebView)后重启,`restore_all_cookies` 从快照成功写入 5 条(日志:开始 5 条 → 完成 true)——"原生存储丢失 → 文件快照 → 完整恢复登录态"核心链路打通

---

## 五、遗留风险与建议

1. **macOS 分支需 CI 验证**:本次改动平台无关(命令 async 化改变了 `mac_cookie` 的调用线程,`with_webview` 投递主线程 + WebKit GCD 回调的组合理论可行),但本机无 macOS 环境,建议下个 CI run(全开 feature)确认 macOS 编译与行为。
2. **dump 覆盖范围**:`GetCookies` 只取 `https://chat.deepseek.com` 关联的 Cookie;若登录流程涉及其他子域(auth/passport 等)仅属该子域的 Cookie 不在快照内。当前对 chat 登录态足够,后续可考虑对登录 URL 追加 dump。
3. **恢复后不刷新**:HttpOnly Cookie 恢复成功但 localStorage 无变化时不触发 reload,登录态要到下次导航才完全生效(低风险,DeepSeek 登录态通常 localStorage 有数据会触发 reload)。
4. **验证残留**:`AppData\Local\com.deepchat.desktop\EBWebView.new` 为 restore 验证时新建的 profile,已无害,可手动删除。

---

## 六、macOS 验证补充（2026-08-11，Mac 环境复验 + 发现并修复新问题）

**验证方式**:双 feature 构建 + `DS_SELFTEST=1` 自检模式真机运行(macOS aarch64, Rust 1.9x, tauri 2.11.5 / wry 0.55.1 / objc2-web-kit),selftest 增加 dump/restore/快照断言并写入 debug.log。

### 1. 编译验证
4 种 feature 组合 `cargo check` 全部 0 错误 0 警告(含修复后的最终代码)。

### 2. 真机运行验证（encrypt+full 同时开启）
| 断言 | 结果 |
|------|------|
| 加密落盘 | ✅ `session.json` 以 `DSENC1:` 密文保存(macOS Keychain + AES-256-GCM) |
| 解密读回 | ✅ `load_session` 成功解密,字段完整(localStorage/cookies/cookiesFull/savedAt) |
| dump 全量导出 | ✅ 读出 **6 条 Cookie,含 3 条 HttpOnly**(`HWWAFSESID`/`HWWAFSESTIME`/`ds_session_id`) |
| restore 恢复 | ✅ dump→restore→再 dump 一致(6→6, same=true) |
| 指纹伪装 | ✅ UA=Edge 伪装生效,未触发"使用环境异常"弹窗 |

### 3. ⚠️ 新发现并修复:macOS 上 `restore_all_cookies` sync 命令死锁

- **现象**:restore 调用 10s 超时(`写入 Cookie 超时`),dump 正常。
- **根因**:Windows 修复时仅将 `dump_all_cookies` async 化(GetCookies 是异步 COM 调用);
  `restore_all_cookies` 在 Windows 因 `AddOrUpdateCookie` 同步而无碍,但 **macOS 的
  `setCookie_completionHandler` 是异步回调**,sync 命令在主线程等待回调 → 回调投递到主线程
  消息泵却被主线程阻塞 → 死锁超时。
- **修复**(`src-tauri/src/session.rs`):`restore_all_cookies` 同样改为 **async + spawn_blocking**,
  等待移出主线程。修复后实测 restore 6→6 成功。
- **结论**:async 化是**两个命令**都必须的(macOS 路径),Windows 修复只覆盖了一半;本次在 macOS
  复验补齐。建议 Windows 侧保留 async 形态(对 Windows 亦无副作用,行为一致)。

### 4. 遗留风险更新
- 风险 1(macOS 需 CI 验证)→ **已在本机 macOS 真机复验通过**;另补修 restore 命令 async 化。
- 其余风险 2/3/4 仍适用(macOS 侧同样存在 dump 仅 chat 域、恢复后不刷新、验证残留 profile)。
