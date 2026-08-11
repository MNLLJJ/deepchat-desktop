//! DeepChat Desktop —— chat.deepseek.com 桌面客户端（Tauri 2 应用入口）。
//!
//! 托盘（菜单栏）常驻行为：
//! - 点击窗口关闭按钮 / Cmd+W / Cmd+Q：不退出程序，而是隐藏主窗口、收纳到菜单栏图标
//! - 单击菜单栏图标或点击 Dock 图标：恢复并置前主窗口
//! - 右键菜单栏图标：提供"显示主界面"与"退出"（真正的退出入口）
//! - 退出前通过共享状态区分"用户主动退出"与"Cmd+Q 收纳"，托盘资源由系统在进程退出时回收

mod session;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tauri::{
    menu::{MenuBuilder, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, RunEvent, WebviewUrl, WebviewWindowBuilder,
};

const APP_TITLE: &str = "DeepChat";
const APP_URL: &str = "https://chat.deepseek.com";
/// 托盘图标的唯一标识
const TRAY_ID: &str = "main-tray";

/// 伪装 UA 的 Chrome/Edge 版本号，集中管理（P0-2：防止硬编码版本随 DeepSeek 检测升级而过时）。
/// - 可通过编译期环境变量覆盖：`DS_UA_VERSION=140 cargo build`
/// - spoof.js 会从 UA 中正则提取 Chrome major，自动跟随本常量，无需同步修改
const DEFAULT_UA_VERSION: &str = "138";

/// 生成伪装成标准浏览器（Edge）的 UA，避免 DeepSeek 识别出 WebView 环境而弹出"使用环境异常"。
///
/// - 各平台统一使用 Edge UA：DeepSeek 的客户端识别对"标准 Edge UA"放行（社区实测），
///   且与 WebView2 的 Sec-CH-UA 品牌一致；macOS 的 Edge 也是真实存在的浏览器。
fn spoofed_ua() -> String {
    let v = option_env!("DS_UA_VERSION").unwrap_or(DEFAULT_UA_VERSION);
    #[cfg(target_os = "macos")]
    let platform = "Macintosh; Intel Mac OS X 10_15_7";
    #[cfg(target_os = "windows")]
    let platform = "Windows NT 10.0; Win64; x64";
    #[cfg(target_os = "linux")]
    let platform = "X11; Linux x86_64";
    format!(
        "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{v}.0.0.0 Safari/537.36 Edg/{v}.0.0.0"
    )
}

/// 应用级共享状态（由 `app.manage` 托管，生命周期与应用一致）
struct AppState {
    /// 是否由托盘菜单"退出"发起。`true` 时放行退出流程；
    /// `false` 时拦截 Cmd+Q（收纳到菜单栏，而不是退出）。
    quitting: AtomicBool,
    /// 页面是否至少完成过一次加载（看门狗自愈用）。
    page_loaded_once: AtomicBool,
    /// 看门狗已触发的强制刷新次数（上限 2 次）。
    watchdog_reloads: AtomicU32,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            quitting: AtomicBool::new(false),
            page_loaded_once: AtomicBool::new(false),
            watchdog_reloads: AtomicU32::new(0),
        }
    }
}

/// 加载看门狗（P1-10）：启动后若长时间（约 20s）页面仍未完成任何一次加载，
/// 说明 WebView 可能卡在转圈 / 初始化异常（Windows WebView2 上偶发，多因上次异常退出的
/// profile 锁残留或网络加载挂起）。
/// - 前两轮：主动 `location.reload()` 自愈；
/// - 第三轮（约 60s）仍失败：导航到本地离线错误页（?offline=1），由页面自动探测网络并重试，
///   避免无限 reload 轰炸白屏。
fn spawn_load_watchdog(app: &tauri::AppHandle) {
    let handle = app.clone();
    std::thread::spawn(move || {
        for round in 0..3u32 {
            std::thread::sleep(std::time::Duration::from_secs(20));
            let loaded = handle
                .try_state::<AppState>()
                .map(|s| s.page_loaded_once.load(Ordering::SeqCst))
                .unwrap_or(false);
            if loaded {
                return; // 页面已正常加载，自愈完成
            }
            if round == 2 {
                // 已 reload 2 次仍失败 → 转本地离线错误页，不再轰炸
                if let Some(win) = handle.get_webview_window("main") {
                    let _ = win.navigate(local_app_url("index.html?offline=1"));
                }
                return;
            }
            let count = handle
                .try_state::<AppState>()
                .map(|s| s.watchdog_reloads.fetch_add(1, Ordering::SeqCst))
                .unwrap_or(0);
            if count >= 2 {
                return; // 已重试 2 次仍未加载成功，放弃（避免无限轰炸）
            }
            if let Some(win) = handle.get_webview_window("main") {
                let _ = win.eval("if(document.readyState!=='complete'){location.reload()}else{0}");
            }
        }
    });
}

/// 本地资源页 URL（Tauri 2 生产构建本地协议：macOS 为 tauri://localhost，
/// Windows/Linux 为 http://tauri.localhost）。用于离线错误页等本地兜底页面。
fn local_app_url(path: &str) -> url::Url {
    #[cfg(target_os = "macos")]
    let base = "tauri://localhost";
    #[cfg(not(target_os = "macos"))]
    let base = "http://tauri.localhost";
    url::Url::parse(&format!("{base}/{path}")).expect("本地页面 URL 无效")
}

/// 隐藏主窗口（窗口实例保留，WebView 会话不中断，便于再次唤醒）
fn hide_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.hide();
    }
}

/// 恢复并置前主窗口（若曾最小化则先还原，再置顶聚焦）
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

/// 创建菜单栏（托盘）图标与右键菜单。
/// 首次启动即自动创建，之后常驻；进程退出时由 Tauri/系统自动回收。
fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    // 右键菜单：显示主界面 / 退出
    let show_item = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = MenuBuilder::new(app).item(&show_item).item(&quit_item).build()?;

    // 托盘图标：纯黑透明 PNG（macOS 模板图：仅 alpha 通道生效，系统自动适配菜单栏深浅色）
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;

    let tray_builder = TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip(APP_TITLE)
        .menu(&menu)
        // 左键点击不弹出菜单，改为触发 TrayIconEvent::Click，用于快速唤醒窗口
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "quit" => {
                // 标记"真正退出"：放行 ExitRequested，避免被当作 Cmd+Q 收纳拦截
                if let Some(state) = app.try_state::<AppState>() {
                    state.quitting.store(true, Ordering::SeqCst);
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 单击（松开左键）托盘图标 → 恢复主窗口
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    // macOS：将图标注册为模板图像，菜单栏深色模式下自动反色为白色
    #[cfg(target_os = "macos")]
    let tray_builder = tray_builder.icon_as_template(true);

    tray_builder.build(app)?;

    Ok(())
}

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

/// 纵深防御（P0-5）：capability 白名单（*.deepseek.com）只是入口闸门，
/// 这里在命令侧对调用方 origin 二次校验（https + deepseek.com 主域/子域），
/// 防止 DeepSeek 任一子域被注入/攻破后滥用本地命令（读/写/删快照、打开任意链接）。
/// 所有暴露给远程页的 command 必须调用本函数。
pub(crate) fn ensure_deepseek_origin(webview: &tauri::Webview) -> Result<(), String> {
    let url = webview
        .url()
        .map_err(|e| format!("获取调用方 URL 失败: {e}"))?;
    if url.scheme() != "https" {
        return Err("拒绝非 https 调用".into());
    }
    let host = url.host_str().unwrap_or("");
    if host != "deepseek.com" && !host.ends_with(".deepseek.com") {
        return Err("拒绝非 deepseek 域调用".into());
    }
    Ok(())
}

/// 外部链接：用系统默认浏览器打开（仅允许 http/https）。
/// 统一走 `open` crate：Windows 内部为 ShellExecuteW（替代启动 PowerShell 进程，
/// 启动快且不依赖 PowerShell 可用性，P0-3）；macOS/Linux 分别为 open / xdg-open。
#[tauri::command]
fn open_external(webview: tauri::Webview, url: String) -> Result<(), String> {
    ensure_deepseek_origin(&webview)?;

    let parsed = url::Url::parse(&url).map_err(|e| format!("无效 URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(format!("不允许的协议: {s}")),
    }
    open::that(&url).map_err(|e| format!("打开外部链接失败: {e}"))
}

pub fn run() {
    let app = tauri::Builder::default()
        // 单实例锁（P0-1）：阻止多开。Windows 上多进程会争用 WebView2 profile 锁
        // 导致白屏/转圈；二次启动时唤起已有窗口并聚焦（含从托盘/最小化恢复）。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        .invoke_handler(tauri::generate_handler![
            session::save_session,
            session::load_session,
            session::clear_session,
            session::debug_log,
            open_external,
            #[cfg(feature = "full-cookie-snapshot")]
            session::dump_all_cookies,
            #[cfg(feature = "full-cookie-snapshot")]
            session::restore_all_cookies,
        ])
        .setup(|app| {
            // 1) 注册共享状态 + 创建菜单栏图标（每次启动自动创建，常驻）
            app.manage(AppState::default());
            setup_tray(app.handle())?;

            // 2) 创建主窗口
            let init_script = build_init_script();
            let ua = spoofed_ua();

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
            // 伪装标准 Chrome/Edge UA，规避“使用环境异常”检测（版本集中管理，可用 DS_UA_VERSION 覆盖）
            .user_agent(&ua)
            // 页面注入：会话恢复 / 同步 / 链接处理
            .initialization_script(&init_script)
            // 记录页面加载完成标记，供看门狗判断"是否卡在转圈"
            .on_page_load(|window, _payload| {
                if window.label() == "main" {
                    if let Some(state) = window.app_handle().try_state::<AppState>() {
                        state.page_loaded_once.store(true, Ordering::SeqCst);
                    }
                }
            })
            // 导航安全护栏：仅放行 http/https（远程站点），以及本地资源页
            // （tauri://localhost / http://tauri.localhost，离线错误页与占位页），
            // 拦截 data:/file:/javascript: 等
            .on_navigation(|url| {
                let scheme = url.scheme();
                if scheme == "http" || scheme == "https" {
                    return true;
                }
                let host = url.host_str().unwrap_or("");
                (scheme == "tauri" && host == "localhost")
                    || (scheme == "http" && host == "tauri.localhost")
            })
            .build()
            .map_err(|e| format!("创建主窗口失败: {e}"))?;

            // 3) 加载看门狗：页面长时间未加载完成时自动刷新自愈
            spawn_load_watchdog(app.handle());

            Ok(())
        })
        // 3) 拦截窗口关闭：关闭按钮 / Cmd+W → 阻止关闭。
        //    macOS/Linux：隐藏到菜单栏（托盘常驻）；Windows：最小化到任务栏
        //    （P0-6：完全隐藏会让用户误以为应用退出了，最小化可见且可由托盘/任务栏恢复）
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                #[cfg(target_os = "windows")]
                {
                    let _ = window.minimize();
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = window.hide();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("Tauri 应用启动失败");

    // 4) 应用级事件：Cmd+Q 收纳 / Dock 点击恢复 / 退出清理
    app.run(|app, event| match event {
        // Cmd+Q：默认会退出应用，这里拦截并收纳到菜单栏
        // （除非用户已通过托盘菜单"退出"显式请求退出）
        RunEvent::ExitRequested { api, .. } => {
            let quitting = app
                .try_state::<AppState>()
                .map(|s| s.quitting.load(Ordering::SeqCst))
                .unwrap_or(false);
            if !quitting {
                api.prevent_exit();
                hide_main_window(app);
            }
        }
        // macOS：点击 Dock 图标时恢复主窗口（该事件仅 macOS 存在，须按平台隔离）
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => {
            show_main_window(app);
        }
        // 真正退出前：托盘图标由 Tauri/系统自动回收，此处仅作兜底记录
        RunEvent::Exit => {
            eprintln!("[tray] 应用退出，托盘资源已由系统回收");
        }
        _ => {}
    });
}
