//! DeepChat Desktop —— chat.deepseek.com 桌面客户端（Tauri 2 应用入口）。

mod session;

use tauri::WebviewUrl;
use tauri::WebviewWindowBuilder;

const APP_TITLE: &str = "DeepChat";
const APP_URL: &str = "https://chat.deepseek.com";

/// 伪装成标准浏览器 UA，避免 DeepSeek 识别出 WebView 环境而弹出“使用环境异常”。
///
/// - 各平台统一使用 Edge UA：DeepSeek 的客户端识别对“标准 Edge UA”放行（社区实测），
///   且与 WebView2 的 Sec-CH-UA 品牌一致；macOS 的 Edge 也是真实存在的浏览器。
#[cfg(target_os = "macos")]
const SPOOFED_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36 Edg/138.0.0.0";

#[cfg(target_os = "windows")]
const SPOOFED_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36 Edg/138.0.0.0";

#[cfg(target_os = "linux")]
const SPOOFED_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36 Edg/138.0.0.0";

/// 组装注入脚本：指纹伪装（生产） + 会话桥接（生产） + 可选的自检脚本
fn build_init_script() -> String {
    let mut parts = vec![
        include_str!("../assets/spoof.js").to_string(),
        include_str!("../assets/init.js").to_string(),
    ];
    if std::env::var("DS_SELFTEST").is_ok() {
        parts.push(include_str!("../assets/selftest.js").to_string());
    }
    parts.join("\n")
}

/// 外部链接：用系统默认浏览器打开（仅允许 http/https）
#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    let parsed = url::Url::parse(&url).map_err(|e| format!("无效 URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(format!("不允许的协议: {s}")),
    }
    open_in_system_browser(&url)
}

#[cfg(target_os = "macos")]
fn open_in_system_browser(url: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("调用 `open` 失败: {e}"))
}

#[cfg(target_os = "windows")]
fn open_in_system_browser(url: &str) -> Result<(), String> {
    // 使用 PowerShell 的 Start-Process 处理 URL（含 & 等特殊字符），比 cmd start 更稳
    std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("Start-Process '{}'", url.replace('\'', "''")),
        ])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("调用 PowerShell 失败: {e}"))
}

#[cfg(target_os = "linux")]
fn open_in_system_browser(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("调用 xdg-open 失败: {e}"))
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            session::save_session,
            session::load_session,
            session::clear_session,
            session::debug_log,
            open_external,
        ])
        .setup(|app| {
            let init_script = build_init_script();

            let _window = WebviewWindowBuilder::new(
                app,
                "main",
                WebviewUrl::External(APP_URL.parse().expect("应用 URL 无效")),
            )
            .title(APP_TITLE)
            // 默认 1280x800，最小 1000x600，可缩放
            .inner_size(1280.0, 800.0)
            .min_inner_size(1000.0, 600.0)
            .resizable(true)
            .center()
            .visible(true)
            // 伪装标准 Chrome/Edge UA，规避“使用环境异常”检测
            .user_agent(SPOOFED_UA)
            // 页面注入：会话恢复 / 同步 / 链接处理
            .initialization_script(&init_script)
            // 导航安全护栏：仅放行 http/https，拦截 data:/file:/javascript: 等
            .on_navigation(|url| matches!(url.scheme(), "http" | "https"))
            .build()
            .map_err(|e| format!("创建主窗口失败: {e}"))?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}
