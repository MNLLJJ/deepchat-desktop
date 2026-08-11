//! 会话持久化：把 Cookie 与 localStorage 快照读写到本地文件（app_data_dir/session.json）。
//!
//! 持久化采用双层策略：
//! 1. 原生 WebView 存储（macOS WKWebView defaultDataStore / Windows WebView2 profile），
//!    自动持久化全部 Cookie（含 HttpOnly）与 localStorage —— 这是登录态保持的主路径；
//! 2. 本模块提供的文件快照 —— 双保险，覆盖原生存储被清除 / 换构建 / 系统清理等场景，
//!    由注入页面的 init.js 定期同步并在启动时恢复。
//!
//! 可选能力（Cargo feature）：
//! - `encrypt-session`：快照落盘前加密（Windows DPAPI / macOS Keychain+AES-256-GCM），
//!   旧版明文快照仍可读取（向后兼容）；
//! - `full-cookie-snapshot`：通过原生 WebView 存储接口读写 HttpOnly Cookie，
//!   让登录态在原生存储丢失后也能从文件快照完整恢复（Windows WebView2 / macOS WKWebView）。

use std::path::PathBuf;
use tauri::{AppHandle, Manager, Webview};

/// 快照文件相对 app_data_dir 的文件名
const SESSION_FILE: &str = "session.json";

fn session_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法获取应用数据目录: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("无法创建应用数据目录: {e}"))?;
    Ok(dir.join(SESSION_FILE))
}

/// 判断快照 JSON 是否"空会话"（既无 cookie 也无 localStorage 数据）。
/// 用于防污染：页面加载中 / 未登录中间态产生的空快照，不得覆盖已有的好快照。
fn is_empty_snapshot(value: &serde_json::Value) -> bool {
    let ls_empty = value
        .get("localStorage")
        .map(|v| v.as_object().map(|o| o.is_empty()).unwrap_or(true))
        .unwrap_or(true);
    let cookies_empty = value
        .get("cookies")
        .map(|v| v.as_str().map(|s| s.is_empty()).unwrap_or(true))
        .unwrap_or(true);
    ls_empty && cookies_empty
}

// =========================================================================
// 会话快照加密（feature: encrypt-session）
// =========================================================================
#[cfg(feature = "encrypt-session")]
mod cipher {
    /// 加密快照文件前缀：`DSENC1:` + 密文（二进制）。无此前缀视为旧版明文快照（向后兼容）。
    pub const MAGIC: &str = "DSENC1:";

    // ---------- Windows：DPAPI（当前用户上下文加密，无需额外密钥存储） ----------
    // 注意：windows crate 0.61 中 `DATA_BLOB` 更名为 `CRYPT_INTEGER_BLOB`，
    // `LocalFree` 位于 `Win32::Foundation`（原 `System::Memory`），
    // `CryptUnprotectData` 第 2 参为 `Option<*mut PWSTR>`（传 None）。
    #[cfg(target_os = "windows")]
    pub fn encrypt(data: &[u8]) -> Result<Vec<u8>, String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Cryptography::{
            CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };

        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|e| format!("DPAPI 加密失败: {e}"))?;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            let _ = LocalFree(Some(HLOCAL(output.pbData as *mut core::ffi::c_void)));
        }
        Ok(bytes)
    }

    #[cfg(target_os = "windows")]
    pub fn decrypt(data: &[u8]) -> Result<Vec<u8>, String> {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Cryptography::{
            CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
        };

        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &input,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .map_err(|e| format!("DPAPI 解密失败: {e}"))?;
        }
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe {
            let _ = LocalFree(Some(HLOCAL(output.pbData as *mut core::ffi::c_void)));
        }
        Ok(bytes)
    }

    // ---------- macOS：Keychain 存 AES-256-GCM 密钥 ----------
    #[cfg(target_os = "macos")]
    const SERVICE: &str = "com.deepchat.desktop";
    #[cfg(target_os = "macos")]
    const ACCOUNT: &str = "session-key";

    #[cfg(target_os = "macos")]
    fn get_or_create_key() -> Result<[u8; 32], String> {
        use rand::RngCore;
        use security_framework::passwords::{
            delete_generic_password, get_generic_password, set_generic_password,
        };

        if let Ok(key) = get_generic_password(SERVICE, ACCOUNT) {
            if key.len() == 32 {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&key);
                return Ok(arr);
            }
            let _ = delete_generic_password(SERVICE, ACCOUNT); // 长度异常，重建
        }
        let mut key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut key);
        set_generic_password(SERVICE, ACCOUNT, &key)
            .map_err(|e| format!("Keychain 写入失败: {e}"))?;
        Ok(key)
    }

    #[cfg(target_os = "macos")]
    pub fn encrypt(data: &[u8]) -> Result<Vec<u8>, String> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Key, Nonce};
        use rand::RngCore;

        let key = get_or_create_key()?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let mut nonce = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), data)
            .map_err(|_| "AES-256-GCM 加密失败".to_string())?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        Ok(out)
    }

    #[cfg(target_os = "macos")]
    pub fn decrypt(data: &[u8]) -> Result<Vec<u8>, String> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Key, Nonce};

        if data.len() < 12 {
            return Err("加密数据不完整".into());
        }
        let key = get_or_create_key()?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let (nonce, ct) = data.split_at(12);
        cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| "AES-256-GCM 解密失败（密钥可能已变更）".to_string())
    }

    // ---------- 其他平台：feature 开启但不支持，直接报错 ----------
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    pub fn encrypt(_data: &[u8]) -> Result<Vec<u8>, String> {
        Err("当前平台不支持会话加密（encrypt-session）".into())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    pub fn decrypt(_data: &[u8]) -> Result<Vec<u8>, String> {
        Err("当前平台不支持会话解密（encrypt-session）".into())
    }
}

/// 读取快照文件并返回明文内容；不存在 / 空文件返回 None。
/// feature 开启时自动识别并解密 `DSENC1:` 前缀的密文；旧明文快照直接读取。
fn read_snapshot_plain(path: &std::path::Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("读取快照失败: {e}"))?;

    #[cfg(feature = "encrypt-session")]
    {
        if bytes.starts_with(cipher::MAGIC.as_bytes()) {
            let plain = cipher::decrypt(&bytes[cipher::MAGIC.len()..])?;
            let s = String::from_utf8(plain).map_err(|e| format!("快照解码失败: {e}"))?;
            return Ok(if s.trim().is_empty() { None } else { Some(s) });
        }
        // 无前缀 → 旧版明文快照，继续走下方通用读取
    }

    let s = String::from_utf8(bytes).map_err(|e| format!("快照编码非法: {e}"))?;
    Ok(if s.trim().is_empty() { None } else { Some(s) })
}

/// 判断文件中的旧快照是否为空（不存在 / 空内容 / 空会话都算空）。
/// 读取/解密失败时视为"非空"，阻止空快照覆盖（安全优先，避免误毁数据）。
fn existing_is_empty(path: &std::path::Path) -> bool {
    match read_snapshot_plain(path) {
        Ok(Some(s)) => serde_json::from_str::<serde_json::Value>(&s)
            .map(|v| is_empty_snapshot(&v))
            .unwrap_or(true),
        Ok(None) => true, // 不存在 / 空文件
        Err(_) => false,  // 读取或解密失败 → 视为非空，拒绝空快照覆盖
    }
}

/// 保存会话快照（JSON 字符串，原子写入：先写临时文件再改名）。
///
/// 防污染兜底（与前端 init.js 的双保险）：
/// - 新快照必须为合法 JSON 对象；
/// - 若新快照是"空会话"（无 cookie 且无 localStorage），且本地已存在非空旧快照，
///   则拒绝覆盖 —— 防止"加载中未登录中间态"毁掉最后一份好快照，导致重启后登录失效。
///   用户主动登出的清理由前端调用 clear_session 完成。
///
/// `encrypt-session` feature 开启时，落盘内容为 `DSENC1:` + 密文。
#[tauri::command]
pub fn save_session(app: AppHandle, webview: Webview, data: String) -> Result<(), String> {
    crate::ensure_deepseek_origin(&webview)?;

    // 快照体积上限：前端 init.js 生成上限 8MB（单值 512KB），此处接收上限 10MB（留余量）。
    const MAX_SIZE: usize = 10 * 1024 * 1024;
    if data.len() > MAX_SIZE {
        return Err("快照数据过大".into());
    }
    // 校验 JSON 结构
    let parsed: serde_json::Value =
        serde_json::from_str(&data).map_err(|e| format!("快照 JSON 无效: {e}"))?;
    if !parsed.is_object() {
        return Err("快照必须是 JSON 对象".into());
    }
    let path = session_path(&app)?;
    if is_empty_snapshot(&parsed) && path.exists() && !existing_is_empty(&path) {
        return Err("空快照拒绝覆盖已有会话（可能为未登录中间态）".into());
    }

    // 组装落盘字节：明文 或 加密（feature）
    let bytes: Vec<u8> = {
        #[cfg(feature = "encrypt-session")]
        {
            let mut v = Vec::with_capacity(cipher::MAGIC.len() + data.len() + 32);
            v.extend_from_slice(cipher::MAGIC.as_bytes());
            let enc = cipher::encrypt(data.as_bytes())?;
            v.extend_from_slice(&enc);
            v
        }
        #[cfg(not(feature = "encrypt-session"))]
        {
            data.into_bytes()
        }
    };

    let tmp = path.with_extension("json.tmp");
    // 清理历史崩溃可能残留的临时文件（P0-4）
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, &bytes).map_err(|e| format!("写入快照失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("落盘快照失败: {e}"))?;
    Ok(())
}

/// 读取会话快照（返回明文 JSON 字符串给前端）；不存在时返回 None
#[tauri::command]
pub fn load_session(app: AppHandle, webview: Webview) -> Result<Option<String>, String> {
    crate::ensure_deepseek_origin(&webview)?;

    let path = session_path(&app)?;
    read_snapshot_plain(&path)
}

/// 清空会话快照（用于退出登录后的清理，避免残留脏数据）
#[tauri::command]
pub fn clear_session(app: AppHandle, webview: Webview) -> Result<(), String> {
    crate::ensure_deepseek_origin(&webview)?;

    let path = session_path(&app)?;
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| format!("清理快照失败: {e}"))?;
    }
    Ok(())
}

/// 调试日志（仅自检模式使用；写入 app_data_dir/debug.log）
#[tauri::command]
pub fn debug_log(app: AppHandle, webview: Webview, msg: String) -> Result<(), String> {
    crate::ensure_deepseek_origin(&webview)?;

    let path = session_path(&app)?;
    let log_path = path.with_file_name("debug.log");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("打开日志失败: {e}"))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    writeln!(f, "[{ts}] {msg}").map_err(|e| format!("写日志失败: {e}"))?;
    Ok(())
}

// =========================================================================
// HttpOnly Cookie 全量快照（feature: full-cookie-snapshot）
// =========================================================================
// 原生 WebView 存储中 HttpOnly 的登录 Cookie 无法通过 document.cookie 读取/恢复，
// 该 feature 通过平台原生接口（Windows WebView2 CookieManager / macOS WKHTTPCookieStore）
// 全量读写 Cookie，使登录态在原生存储丢失后也能从文件快照完整恢复。

/// 单条 Cookie 的全量描述（与 WebView2 / WKHTTPCookieStore 字段对齐）。
#[cfg(feature = "full-cookie-snapshot")]
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CookieData {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub http_only: bool,
    pub secure: bool,
    /// 是否为会话级 Cookie（不持久化）
    pub session: bool,
    /// 过期时间（Unix 秒）；会话 Cookie 为 0
    pub expires: f64,
}

// ---------- Windows：WebView2 CookieManager（webview2-com 0.38，与 tauri/wry 同版本） ----------
// 实现要点（对照 wry 0.55 同版本参考实现）：
// 1. CookieManager 通过 `ICoreWebView2_2::CookieManager()` 获取（0.38 中方法名是 CookieManager，
//    不是 GetCookieManager；旧版代码用的 GetCookiesAsync 也已在 0.38 更名为 GetCookies）。
// 2. Cookie 属性读写均为 out 参数形式（Name/Value/Domain/Path/IsHttpOnly/IsSecure/IsSession/Expires），
//    属性设置方法为 SetIsHttpOnly(bool)/SetIsSecure(bool)/SetExpires(f64)。
// 3. AddOrUpdateCookie 在 0.38 中是同步方法（无完成回调），restore 无需等待。
// 4. GetCookies 是异步的：完成回调投递到创建 WebView2 的 UI 线程（Tauri 主线程）消息泵。
//    因此 dump 绝不能作为 sync 命令在主线程执行 —— 那会发生在 WebView2 事件处理器
//    （WebMessageReceived）调用栈内，等待回调构成"嵌套消息循环"触发 WebView2 不支持的重入，
//    回调永远不会到达（实测挂起）。正确模式（见命令包装层）：async 命令 + spawn_blocking，
//    with_webview 闭包被投递到空闲的主线程执行并注册 GetCookies，WebView2 回调由主线程
//    消息泵正常投递，blocking 线程用普通 recv_timeout 收结果。
#[cfg(all(feature = "full-cookie-snapshot", target_os = "windows"))]
mod win_cookie {
    use std::sync::mpsc;
    use std::time::Duration;

    use tauri::Webview;
    use webview2_com::GetCookiesCompletedHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2Cookie, ICoreWebView2CookieList, ICoreWebView2_2,
        ICoreWebView2GetCookiesCompletedHandler,
    };
    use windows::core::{Interface, HSTRING, PCWSTR, PWSTR};

    use super::CookieData;

    fn pwstr(pw: PWSTR) -> String {
        if pw.is_null() {
            return String::new();
        }
        unsafe { pw.to_string().unwrap_or_default() }
    }

    fn cookie_to_data(cookie: &ICoreWebView2Cookie) -> Result<CookieData, String> {
        use windows::core::BOOL;
        unsafe {
            let mut name = PWSTR::null();
            let mut value = PWSTR::null();
            let mut domain = PWSTR::null();
            let mut path = PWSTR::null();
            let mut http_only: BOOL = false.into();
            let mut secure: BOOL = false.into();
            let mut session: BOOL = false.into();
            let mut expires = 0.0f64;
            cookie
                .Name(&mut name)
                .map_err(|e| format!("读 Cookie Name 失败: {e}"))?;
            cookie
                .Value(&mut value)
                .map_err(|e| format!("读 Cookie Value 失败: {e}"))?;
            cookie
                .Domain(&mut domain)
                .map_err(|e| format!("读 Cookie Domain 失败: {e}"))?;
            cookie
                .Path(&mut path)
                .map_err(|e| format!("读 Cookie Path 失败: {e}"))?;
            cookie
                .IsHttpOnly(&mut http_only)
                .map_err(|e| format!("读 Cookie HttpOnly 失败: {e}"))?;
            cookie
                .IsSecure(&mut secure)
                .map_err(|e| format!("读 Cookie Secure 失败: {e}"))?;
            cookie
                .IsSession(&mut session)
                .map_err(|e| format!("读 Cookie Session 失败: {e}"))?;
            cookie
                .Expires(&mut expires)
                .map_err(|e| format!("读 Cookie Expires 失败: {e}"))?;
            Ok(CookieData {
                name: pwstr(name),
                value: pwstr(value),
                domain: pwstr(domain),
                path: pwstr(path),
                http_only: http_only.as_bool(),
                secure: secure.as_bool(),
                session: session.as_bool(),
                expires,
            })
        }
    }

    pub fn dump_all_cookies(webview: &Webview) -> Result<Vec<CookieData>, String> {
        let (tx, rx) = mpsc::channel();
        let tx2 = tx.clone();
        webview
            .with_webview(move |pw| {
                let result = (|| -> Result<(), String> {
                    let core = unsafe { pw.controller().CoreWebView2() }
                        .map_err(|e| format!("获取 CoreWebView2 失败: {e}"))?;
                    // CookieManager() 是 ICoreWebView2_2 及以上接口的方法（0.38 bindings）
                    let core2 = core
                        .cast::<ICoreWebView2_2>()
                        .map_err(|e| format!("获取 ICoreWebView2_2 失败: {e}"))?;
                    let manager = unsafe { core2.CookieManager() }
                        .map_err(|e| format!("获取 CookieManager 失败: {e}"))?;
                    let uri = HSTRING::from("https://chat.deepseek.com");
                    let handler: ICoreWebView2GetCookiesCompletedHandler =
                        GetCookiesCompletedHandler::create(Box::new(
                            // 宏约定:HRESULT 参数自动转为 windows::core::Result<()>
                            move |error_code: windows::core::Result<()>,
                                  cookies: Option<ICoreWebView2CookieList>|
                             -> windows::core::Result<()> {
                                let r = (move || -> Result<Vec<CookieData>, String> {
                                    error_code
                                        .map_err(|e| format!("GetCookies 失败: {e}"))?;
                                    let mut out = Vec::new();
                                    if let Some(list) = cookies {
                                        let mut count = 0u32;
                                        unsafe { list.Count(&mut count) }
                                            .map_err(|e| format!("读 Cookie 数量失败: {e}"))?;
                                        for i in 0..count {
                                            let cookie = unsafe { list.GetValueAtIndex(i) }
                                                .map_err(|e| format!("读 Cookie 失败: {e}"))?;
                                            out.push(cookie_to_data(&cookie)?);
                                        }
                                    }
                                    Ok(out)
                                })();
                                let _ = tx2.send(r);
                                Ok(())
                            },
                        ));
                    unsafe { manager.GetCookies(PCWSTR(uri.as_ptr()), &handler) }
                        .map_err(|e| format!("GetCookies 失败: {e}"))?;
                    Ok(())
                })();
                if let Err(e) = result {
                    let _ = tx.send(Err(e));
                }
            })
            .map_err(|e| format!("with_webview 失败: {e}"))?;

        // 等待回调。本函数由命令包装层（dump_all_cookies 命令）的 spawn_blocking 线程调用：
        // with_webview 闭包会被投递到主线程执行（注册 GetCookies），WebView2 完成回调由
        // 空闲的主线程消息泵投递并触发 handler 发送 channel，这里普通 recv 即可。
        // 注意：绝不能在主线程（sync 命令）直接等待——那是在 WebView2 事件处理器栈内
        // 启动嵌套消息循环，触发 WebView2 不支持的重入，回调永远不会到达（实测挂起）。
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(cookies)) => Ok(cookies),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("获取 Cookie 超时".to_string()),
        }
    }

    pub fn restore_all_cookies(webview: &Webview, cookies: Vec<CookieData>) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        webview
            .with_webview(move |pw| {
                let result = (|| -> Result<(), String> {
                    let core = unsafe { pw.controller().CoreWebView2() }
                        .map_err(|e| format!("获取 CoreWebView2 失败: {e}"))?;
                    // CookieManager() 是 ICoreWebView2_2 及以上接口的方法（0.38 bindings）
                    let core2 = core
                        .cast::<ICoreWebView2_2>()
                        .map_err(|e| format!("获取 ICoreWebView2_2 失败: {e}"))?;
                    let manager = unsafe { core2.CookieManager() }
                        .map_err(|e| format!("获取 CookieManager 失败: {e}"))?;
                    for c in cookies {
                        let name = HSTRING::from(c.name.as_str());
                        let value = HSTRING::from(c.value.as_str());
                        let domain = HSTRING::from(c.domain.as_str());
                        let path = HSTRING::from(c.path.as_str());
                        let cookie = unsafe { manager.CreateCookie(&name, &value, &domain, &path) }
                            .map_err(|e| format!("CreateCookie 失败 ({}): {e}", c.name))?;
                        unsafe {
                            cookie
                                .SetIsHttpOnly(c.http_only)
                                .and_then(|_| cookie.SetIsSecure(c.secure))
                                .and_then(|_| {
                                    if !c.session && c.expires > 0.0 {
                                        cookie.SetExpires(c.expires)
                                    } else {
                                        Ok(())
                                    }
                                })
                        }
                        .map_err(|e| format!("设置 Cookie 属性失败 ({}): {e}", c.name))?;
                        unsafe { manager.AddOrUpdateCookie(&cookie) }
                            .map_err(|e| format!("AddOrUpdateCookie 失败 ({}): {e}", c.name))?;
                    }
                    Ok(())
                })();
                let _ = tx.send(result);
            })
            .map_err(|e| format!("with_webview 失败: {e}"))?;
        // with_webview 闭包同步执行完毕后才返回，直接取结果即可（AddOrUpdateCookie 为同步调用，无回调等待）
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| "写入 Cookie 超时".to_string())?
    }
}

// ---------- macOS：WKHTTPCookieStore（objc2-web-kit 0.3 + objc2-foundation 0.3） ----------
// 注意：完成回调在 WebKit 主队列（Tauri 主线程 runloop）执行，
// 采用"with_webview 回调内注册、外层 channel 等待"模式，避免主线程死锁。
#[cfg(all(feature = "full-cookie-snapshot", target_os = "macos"))]
mod mac_cookie {
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2_foundation::{
        NSArray, NSDate, NSDictionary, NSHTTPCookie, NSHTTPCookieDomain, NSHTTPCookieExpires,
        NSHTTPCookieName, NSHTTPCookiePath, NSHTTPCookiePropertyKey, NSHTTPCookieSecure,
        NSHTTPCookieValue, NSCopying, NSNumber, NSString,
    };
    use objc2_web_kit::{WKHTTPCookieStore, WKWebView};
    use tauri::Webview;

    use super::CookieData;

    fn nsstring(s: &str) -> Retained<NSString> {
        NSString::from_str(s)
    }

    fn cookie_to_data(cookie: &NSHTTPCookie) -> CookieData {
        let expires = cookie
            .expiresDate()
            .map(|d| d.timeIntervalSince1970())
            .unwrap_or(0.0);
        CookieData {
            name: cookie.name().to_string(),
            value: cookie.value().to_string(),
            domain: cookie.domain().to_string(),
            path: cookie.path().to_string(),
            http_only: cookie.isHTTPOnly(),
            secure: cookie.isSecure(),
            session: cookie.isSessionOnly(),
            expires,
        }
    }

    /// 从 with_webview 回调里拿 WKWebView，再取其 CookieStore。
    unsafe fn cookie_store_of(pw: &tauri::webview::PlatformWebview) -> Retained<WKHTTPCookieStore> {
        let wv: &WKWebView = unsafe { &*(pw.inner() as *const WKWebView) };
        wv.configuration().websiteDataStore().httpCookieStore()
    }

    pub fn dump_all_cookies(webview: &Webview) -> Result<Vec<CookieData>, String> {
        let (tx, rx) = mpsc::channel();
        let tx2 = tx.clone();
        webview
            .with_webview(move |pw| {
                let store = unsafe { cookie_store_of(&pw) };
                let block = RcBlock::new(move |cookies: std::ptr::NonNull<NSArray<NSHTTPCookie>>| {
                    let mut list = Vec::new();
                    let arr = unsafe { cookies.as_ref() };
                    for i in 0..arr.count() {
                        let c = arr.objectAtIndex(i);
                        list.push(cookie_to_data(&c));
                    }
                    let _ = tx2.send(list);
                });
                unsafe { store.getAllCookies(&block) };
            })
            .map_err(|e| format!("with_webview 失败: {e}"))?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| "获取 Cookie 超时".to_string())
    }

    pub fn restore_all_cookies(webview: &Webview, cookies: Vec<CookieData>) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let total = cookies.len();
        let tx_count = tx.clone();
        webview
            .with_webview(move |pw| {
                let store = unsafe { cookie_store_of(&pw) };

                // Foundation 的 NSHTTPCookie 属性键不含 HttpOnly/SessionOnly（WebKit 私有键），
                // 恢复时这两个标志会丢失（name/value/domain/path/expires/Secure 均保留），
                // 登录态可恢复；Windows 分支无此限制（CookieManager 支持全部属性）。
                // （keys 需在闭包内构造：ProtocolObject 非 Send，闭包须满足 Send 约束）
                let keys: Retained<NSArray<ProtocolObject<dyn NSCopying>>> = NSArray::from_slice(&[
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieName) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieValue) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieDomain) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookiePath) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieExpires) },
                    unsafe { ProtocolObject::from_ref(NSHTTPCookieSecure) },
                ]);

                for c in cookies {
                    let values: Retained<NSArray<AnyObject>> = NSArray::from_retained_slice(&[
                        nsstring(&c.name).into(),
                        nsstring(&c.value).into(),
                        nsstring(&c.domain).into(),
                        nsstring(&c.path).into(),
                        NSDate::dateWithTimeIntervalSince1970(c.expires.max(0.0)).into(),
                        NSNumber::numberWithBool(c.secure).into(),
                    ]);
                    let dict: Retained<NSDictionary<NSHTTPCookiePropertyKey, AnyObject>> =
                        unsafe { NSDictionary::dictionaryWithObjects_forKeys(&values, &keys) };
                    let cookie = match unsafe { NSHTTPCookie::cookieWithProperties(&dict) } {
                        Some(c) => c,
                        None => {
                            let _ = tx.send(Err(format!("创建 Cookie 失败: {}", c.name)));
                            return;
                        }
                    };
                    let txc = tx_count.clone();
                    let block = RcBlock::new(move || {
                        let _ = txc.send(Ok(()));
                    });
                    unsafe { store.setCookie_completionHandler(&cookie, Some(&block)) };
                }
                if total == 0 {
                    let _ = tx.send(Ok(()));
                }
            })
            .map_err(|e| format!("with_webview 失败: {e}"))?;
        // 等待全部 cookie 写入完成
        for _ in 0..total {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err("写入 Cookie 超时".to_string()),
            }
        }
        Ok(())
    }
}

// ---------- 其他平台：feature 开启但不支持，返回错误（前端回退 document.cookie） ----------
#[cfg(all(feature = "full-cookie-snapshot", not(any(target_os = "windows", target_os = "macos"))))]
mod other_cookie {
    use super::CookieData;
    use tauri::Webview;

    pub fn dump_all_cookies(_w: &Webview) -> Result<Vec<CookieData>, String> {
        Err("当前平台不支持 HttpOnly Cookie 全量快照（full-cookie-snapshot）".into())
    }

    pub fn restore_all_cookies(_w: &Webview, _c: Vec<CookieData>) -> Result<(), String> {
        Err("当前平台不支持 HttpOnly Cookie 全量快照（full-cookie-snapshot）".into())
    }
}

/// 导出全部 Cookie（含 HttpOnly）—— 供 init.js 在 feature 启用时优先调用
///
/// 必须为 async：Windows 的 GetCookies 是异步 COM 调用，完成回调投递到主线程消息泵。
/// 若在 sync 命令（主线程 = WebView2 事件处理器栈内）等待回调，会构成 WebView2 不支持的
/// 重入（嵌套消息循环），回调永不到达（实测挂起）。async + spawn_blocking 使等待发生在
/// blocking 线程，主线程保持空闲并正常投递回调。
#[cfg(feature = "full-cookie-snapshot")]
#[tauri::command]
pub async fn dump_all_cookies(webview: Webview) -> Result<Vec<CookieData>, String> {
    crate::ensure_deepseek_origin(&webview)?;

    let wv = webview.clone();
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "windows")]
        {
            win_cookie::dump_all_cookies(&wv)
        }
        #[cfg(target_os = "macos")]
        {
            mac_cookie::dump_all_cookies(&wv)
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            other_cookie::dump_all_cookies(&wv)
        }
    })
    .await
    .map_err(|e| format!("dump_all_cookies 任务失败: {e}"))?
}

/// 全量恢复 Cookie（含 HttpOnly）—— 供 init.js 在 feature 启用时优先调用
///
/// 与 dump_all_cookies 同理必须为 async：
/// - Windows：AddOrUpdateCookie 虽为同步 COM 调用，但 with_webview 闭包经 spawn_blocking
///   投递到空闲主线程执行，避免 sync 命令在主线程（事件处理器栈内）直接操作 WebView 的风险。
/// - macOS：`setCookie_completionHandler` 是异步调用，完成回调投递到主线程消息泵；
///   若在 sync 命令主线程内等待回调，同样构成"主线程阻塞等回调、回调等主线程泵"的死锁
///   （实测超时 10s）。async + spawn_blocking 使等待发生在 blocking 线程，主线程保持空闲。
#[cfg(feature = "full-cookie-snapshot")]
#[tauri::command]
pub async fn restore_all_cookies(
    webview: Webview,
    cookies: Vec<CookieData>,
) -> Result<(), String> {
    crate::ensure_deepseek_origin(&webview)?;

    let wv = webview.clone();
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "windows")]
        {
            win_cookie::restore_all_cookies(&wv, cookies)
        }
        #[cfg(target_os = "macos")]
        {
            mac_cookie::restore_all_cookies(&wv, cookies)
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            other_cookie::restore_all_cookies(&wv, cookies)
        }
    })
    .await
    .map_err(|e| format!("restore_all_cookies 任务失败: {e}"))?
}
