// DeepSeek Desktop —— 会话持久化桥接脚本
// 在 WebView 每个页面 document_start 时注入（仅 deepseek 相关页面会到达这里）。
// 职责：
//   1. 启动时从本地文件恢复 Cookie（非 HttpOnly）与 localStorage 快照（双保险，原生 WebView 存储为第一层）
//   2. 定期 / 页面隐藏 / 卸载前把当前 Cookie 与 localStorage 快照写入本地文件
//   3. 处理链接：内部链接应用内打开，外部链接交给系统浏览器
(function () {
  "use strict";
  try {
    // Tauri 2 在每个页面（本地 + 远程）注入 __TAURI_INTERNALS__，其 .invoke() 即 IPC 入口
    var INTERNALS = window.__TAURI_INTERNALS__;
    var INVOKE = INTERNALS && INTERNALS.invoke;
    if (!INVOKE) return;

    var SNAP_LS_KEY = "__ds_desktop_snapshot__"; // 最近一次快照的本地缓存（随原生存储持久化）
    var RESTORED_FLAG = "__ds_desktop_restored__"; // 本次会话是否已恢复
    var MAX_VALUE_LEN = 512 * 1024; // 跳过超过 512KB 的单个值
    var MAX_TOTAL = 8 * 1024 * 1024; // 快照总大小上限 8MB

    // ---------- 快照 ----------
    function snapshot() {
      var out = { localStorage: {}, cookies: "", savedAt: Date.now() };
      var total = 0;
      try {
        for (var i = 0; i < localStorage.length; i++) {
          var k = localStorage.key(i);
          if (!k || k.indexOf("__ds_") === 0) continue; // 跳过自身标记
          var v = localStorage.getItem(k);
          if (v === null) continue;
          if (v.length > MAX_VALUE_LEN) continue;
          total += k.length + v.length;
          if (total > MAX_TOTAL) break;
          out.localStorage[k] = v;
        }
      } catch (e) {
        /* 存储被禁用时忽略 */
      }
      try {
        out.cookies = document.cookie || "";
      } catch (e) {}
      return out;
    }

    // ---------- 保存 ----------
    var saving = false;
    function save() {
      if (saving) return;
      saving = true;
      try {
        var snap = snapshot();
        try {
          localStorage.setItem(SNAP_LS_KEY, JSON.stringify(snap));
        } catch (e) {}
        INVOKE("save_session", { data: JSON.stringify(snap) })
          .catch(function () {})
          .finally(function () {
            saving = false;
          });
      } catch (e) {
        saving = false;
      }
    }

    // ---------- 恢复（每次会话首次加载时执行一次；幂等） ----------
    function restoreOnce() {
      INVOKE("load_session")
        .then(function (saved) {
          if (!saved) return;
          var snap;
          try {
            snap = JSON.parse(saved);
          } catch (e) {
            return;
          }
          if (!snap || typeof snap !== "object") return;
          var changed = false;

          // 恢复 localStorage
          try {
            if (snap.localStorage && typeof snap.localStorage === "object") {
              var keys = Object.keys(snap.localStorage);
              for (var i = 0; i < keys.length; i++) {
                var k = keys[i];
                if (k.indexOf("__ds_") === 0) continue;
                var v = String(snap.localStorage[k]);
                if (localStorage.getItem(k) !== v) {
                  localStorage.setItem(k, v);
                  changed = true;
                }
              }
            }
          } catch (e) {}

          // 恢复 Cookie（仅非 HttpOnly 的可见 Cookie；HttpOnly 由原生 WebView 存储负责）
          try {
            if (snap.cookies && typeof snap.cookies === "string" && snap.cookies.length > 0) {
              var curNames = {};
              var curParts = (document.cookie || "").split(";");
              for (var j = 0; j < curParts.length; j++) {
                var nm = curParts[j].trim().split("=")[0];
                if (nm) curNames[nm] = true;
              }
              var parts = snap.cookies.split(";");
              for (var m = 0; m < parts.length; m++) {
                var ck = parts[m].trim();
                if (!ck) continue;
                var name = ck.split("=")[0];
                if (name && !curNames[name]) {
                  try {
                    document.cookie = ck;
                    changed = true;
                  } catch (e) {}
                }
              }
            }
          } catch (e) {}

          // 若恢复了页面原本没有的数据，刷新一次，
          // 让站点带着恢复后的会话重新初始化（避免“未登录”闪屏）。
          if (changed) {
            try {
              if (!sessionStorage.getItem(RESTORED_FLAG)) {
                sessionStorage.setItem(RESTORED_FLAG, "1");
                setTimeout(function () {
                  location.reload();
                }, 50);
              }
            } catch (e) {}
          }
        })
        .catch(function () {});
    }

    // ---------- 链接处理 ----------
    function isInternalHost(host) {
      var h = String(host || "").toLowerCase();
      return h === "deepseek.com" || h.endsWith(".deepseek.com");
    }

    function openExternal(url) {
      INVOKE("open_external", { url: url }).catch(function () {});
    }

    document.addEventListener(
      "click",
      function (e) {
        try {
          var a = e.target && e.target.closest ? e.target.closest("a") : null;
          if (!a) return;
          var href = a.getAttribute("href") || "";
          if (!href || href.indexOf("#") === 0) return;
          var resolved;
          try {
            resolved = new URL(href, location.href);
          } catch (e2) {
            return;
          }
          if (!isInternalHost(resolved.hostname)) {
            // 外部链接 → 系统浏览器
            e.preventDefault();
            openExternal(resolved.href);
            return;
          }
          if (a.target === "_blank" || a.rel && a.rel.indexOf("external") >= 0) {
            // 内部链接但要求新窗口 → 本窗口打开，避免弹出多余窗口
            e.preventDefault();
            location.href = resolved.href;
          }
        } catch (e3) {
          /* 忽略 */
        }
      },
      true
    );

    var originalOpen = window.open;
    window.open = function (url, target, features) {
      try {
        var resolved = new URL(String(url), location.href);
        if (isInternalHost(resolved.hostname)) {
          location.href = resolved.href;
          return null;
        }
        openExternal(resolved.href);
      } catch (e) {
        if (url) openExternal(String(url));
      }
      return null;
    };

    // ---------- 定时同步 ----------
    restoreOnce();

    setInterval(function () {
      try {
        var s = snapshot();
        var cached = localStorage.getItem(SNAP_LS_KEY);
        if (!cached || cached !== JSON.stringify(s)) save();
      } catch (e) {}
    }, 5000);

    document.addEventListener("visibilitychange", function () {
      if (document.visibilityState === "hidden") save();
    });
    window.addEventListener("pagehide", function () {
      save();
    });
    window.addEventListener("beforeunload", function () {
      save();
    });
  } catch (e) {
    // 绝不破坏宿主页面
  }
})();
