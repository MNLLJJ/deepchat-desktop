// 浏览器指纹伪装：让 WKWebView / WebView2 在 JS 层面呈现为标准 Chrome 特征，
// 规避 DeepSeek 等站点对 WebView 环境的“使用环境异常”检测。
//
// 说明：
// - 仅在 UA 伪装为 Chrome/Edge 时生效（UA 在 lib.rs 中设置）；
// - 本脚本在 document_start 执行，先于页面脚本，DeepSeek 读取到的即为伪装值；
// - 隐藏 window.webkit / window.isTauri 前会先保存原生 IPC 引用并改写 postMessage，
//   因此 Tauri 的 IPC（invoke / 事件）不受影响（自检日志可验证）。
(function () {
  "use strict";
  try {
    var UA = navigator.userAgent || "";
    var CHROME = /Chrome\/(\d+)/.exec(UA);
    if (!CHROME) return; // 非 Chrome UA 时不处理
    var major = CHROME[1]; // 自动跟随 lib.rs 的 UA（DS_UA_VERSION 可覆盖），无需单独维护
    var isEdge = UA.indexOf("Edg/") >= 0;

    // 平台判定（与 lib.rs 中的 UA 模板保持一致）
    var platform = "Linux";
    if (UA.indexOf("Windows") >= 0) platform = "Windows";
    else if (UA.indexOf("Macintosh") >= 0) platform = "macOS";

    // —— 高熵指纹常量（P0-2 集中管理；如需调整与 lib.rs 的 UA 一起升级）——
    // architecture：按 x86_64 主流值返回；Apple Silicon / ARM Windows 上如被高熵检测识破，
    //   再按 navigator.hardwareConcurrency 等推导，此处保持简单。
    var FP_ARCH = "x86";
    var FP_BITNESS = "64";
    // UA-CH 的 platformVersion：Windows 10/11 恒为 15.x；macOS 对应当前大版本。
    var FP_PLATFORM_VERSION = {
      Windows: "15.0.0",
      macOS: "15.0",
      Linux: "6.0",
    };

    // 品牌列表：Edge/Chrome 各自真实的 UA-CH 品牌序列
    var brands = isEdge
      ? [
          { brand: "Microsoft Edge", version: major },
          { brand: "Chromium", version: major },
          { brand: "Not/A)Brand", version: "8" },
        ]
      : [
          { brand: "Not/A)Brand", version: "8" },
          { brand: "Chromium", version: major },
          { brand: "Google Chrome", version: major },
        ];

    // 0) 保存原生 IPC 引用，改写 postMessage（在隐藏 window.webkit 之前！）
    try {
      var handlers = window.webkit && window.webkit.messageHandlers;
      if (handlers) {
        var ipcHandler = handlers.ipc || handlers.tauri;
        if (ipcHandler) {
          var post = function (msg) {
            ipcHandler.postMessage(msg);
          };
          // wry 注入的 window.ipc（Object.freeze，需重新 define）
          try {
            Object.defineProperty(window, "ipc", {
              configurable: true,
              value: Object.freeze({ postMessage: post }),
            });
          } catch (e) {}
          // Tauri 的 __TAURI_INTERNALS__.postMessage
          try {
            if (window.__TAURI_INTERNALS__) {
              Object.defineProperty(window.__TAURI_INTERNALS__, "postMessage", {
                configurable: true,
                value: post,
              });
            }
          } catch (e) {}
        }
      }
    } catch (e) {}

    // 1) 隐藏 WKWebView 特有全局：window.webkit（最大 WebView 特征）
    try {
      // 先尝试彻底删除
      var delOk = false;
      try {
        delete window.webkit;
        delOk = typeof window.webkit === "undefined";
      } catch (e) {}
      if (!delOk) {
        Object.defineProperty(window, "webkit", {
          configurable: true,
          value: undefined,
        });
      }
    } catch (e) {}

    // 2) 隐藏 Tauri 壳特征 window.isTauri（普通站点不会引用它，隐藏无副作用）
    try {
      Object.defineProperty(window, "isTauri", {
        configurable: true,
        value: undefined,
      });
    } catch (e) {}

    // 3) 关键：隐藏 window.__TAURI__
    // DeepSeek 检测 `"__TAURI__" in window && void 0 !== window.__TAURI__`，
    // 而 Tauri 2 确实会注入该全局（withGlobalTauri）。将其值置为 undefined 即可
    // 通过检测（属性还在但值 === undefined）。IPC 走 __TAURI_INTERNALS__，不受影响。
    try {
      if ("__TAURI__" in window && typeof window.__TAURI__ !== "undefined") {
        Object.defineProperty(window, "__TAURI__", {
          configurable: true,
          value: undefined,
        });
      }
    } catch (e) {}

    // 3) navigator.vendor —— 真实 Chrome/Edge 均为 "Google Inc."
    try {
      Object.defineProperty(Navigator.prototype, "vendor", {
        configurable: true,
        get: function () {
          return "Google Inc.";
        },
      });
    } catch (e) {}

    // 4) navigator.userAgentData —— UA-CH JS API（真实 Chrome/Edge 必有的特征）
    try {
      Object.defineProperty(Navigator.prototype, "userAgentData", {
        configurable: true,
        get: function () {
          var state = { brands: brands, mobile: false, platform: platform };
          return {
            get brands() {
              return state.brands;
            },
            get mobile() {
              return state.mobile;
            },
            get platform() {
              return state.platform;
            },
            getHighEntropyValues: function (hints) {
              var fullVersionList = [
                { brand: brands[0].brand, version: major + ".0.0.0" },
                { brand: brands[1].brand, version: major + ".0.0.0" },
                { brand: brands[2].brand, version: "8.0.0.0" },
              ];
              var out = {
                architecture: FP_ARCH,
                bitness: FP_BITNESS,
                brands: brands,
                fullVersionList: fullVersionList,
                mobile: false,
                model: "",
                platform: platform,
                platformVersion: FP_PLATFORM_VERSION[platform] || "0",
                uaFullVersion: major + ".0.0.0",
                wow64: false,
              };
              if (Array.isArray(hints) && hints.length > 0) {
                var filtered = {};
                hints.forEach(function (h) {
                  if (h in out) filtered[h] = out[h];
                });
                return Promise.resolve(filtered);
              }
              return Promise.resolve(out);
            },
            toJSON: function () {
              return { brands: brands, mobile: false, platform: platform };
            },
          };
        },
      });
    } catch (e) {}

    // 5) window.chrome —— 与真实 Chrome/Edge 的键完全一致（loadTimes/csi/app）
    try {
      if (typeof window.chrome === "undefined") {
        window.chrome = {
          loadTimes: function () {
            return {};
          },
          csi: function () {
            return {};
          },
          app: {
            isInstalled: false,
            InstallState: {
              DISABLED: "disabled",
              INSTALLED: "installed",
              NOT_INSTALLED: "not_installed",
            },
            RunningState: {
              CANNOT_RUN: "cannot_run",
              READY_TO_RUN: "ready_to_run",
              RUNNING: "running",
            },
            getDetails: function () {
              return {};
            },
            getIsInstalled: function () {},
          },
        };
      }
    } catch (e) {}

    // 6) navigator.plugins —— 补齐 Chrome/Edge 的 PDF 相关条目（若 WebView 缺失）
    try {
      if (navigator.plugins && navigator.plugins.length === 0) {
        var defs = [
          ["PDF Viewer", "internal-pdf-viewer"],
          ["Chrome PDF Viewer", "mhjfbmdgcfjbbpaeojofohoefgiehjai"],
          ["Chromium PDF Viewer", "mhjfbmdgcfjbbpaeojofohoefgiehjai"],
          ["Chromium PDF and Print Preview", "mhjfbmdgcfjbbpaeojofohoefgiehjai"],
          ["Native Client", "internal-nacl-plugin"],
        ];
        defs.forEach(function (d) {
          try {
            var p = {
              name: d[0],
              filename: d[1],
              description: "Portable Document Format",
              mimeTypes: [],
            };
            navigator.plugins.push
              ? navigator.plugins.push(p)
              : Object.defineProperty(navigator.plugins, d[0], {
                  configurable: true,
                  value: p,
                });
          } catch (e) {}
        });
      }
    } catch (e) {}
  } catch (e) {
    // 绝不破坏宿主页面
  }
})();
