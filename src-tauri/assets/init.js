// DeepSeek Desktop —— 会话持久化桥接脚本
// 在 WebView 每个页面 document_start 时注入（仅 deepseek 相关页面会到达这里）。
// 职责：
//   1. 启动时从本地文件恢复 Cookie 与 localStorage 快照（双保险，原生 WebView 存储为第一层）
//   2. 定期 / 页面隐藏 把当前 Cookie 与 localStorage 快照同步到本地文件
//   3. 处理链接：内部链接应用内打开，外部链接交给系统浏览器
//
// 2026-08-11 修复（Windows 长时间转圈 + 重启登录失效）：
//   - 防无限刷新：reload 仅执行一次（持久化 flag），配合"幽灵 Cookie 黑名单"杜绝每次恢复
//     都 changed → 无限 reload → 页面永远转圈的循环。
//   - 恢复幂等：本地缓存 SNAP_LS_KEY 与文件快照内容一致时跳过恢复，避免每次启动重复恢复。
//   - 保存防污染：未登录中间态（无 cookie 且无 localStorage 数据）不覆盖已有好快照；
//     页面 reload/卸载期间不再触发 save（导航中异步 IPC 不可靠且易写入中间态）。
//   - 合并保存：新快照缺失/为空的字段沿用旧快照，防止"加载中未登录态"覆盖最后一份好快照。
//   - 登出清理：检测到曾登录 → 连续 60s 无登录态 → 调用 clear_session 清空文件快照。
//   - 全量 Cookie 模式（feature: full-cookie-snapshot）：运行时探测 dump_all_cookies /
//     restore_all_cookies 命令是否可用（feature 编译期启用）。可用时快照携带 cookiesFull
//     （含 HttpOnly 登录 Cookie），原生存储丢失后也能完整恢复登录态；不可用自动回退
//     document.cookie（仅非 HttpOnly）。
(function () {
  "use strict";
  try {
    // Tauri 2 在每个页面（本地 + 远程）注入 __TAURI_INTERNALS__，其 .invoke() 即 IPC 入口
    var INTERNALS = window.__TAURI_INTERNALS__;
    var INVOKE = INTERNALS && INTERNALS.invoke;
    if (!INVOKE) return;

    var SNAP_LS_KEY = "__ds_desktop_snapshot__"; // 最近一次快照的本地缓存（随原生存储持久化）
    var RESTORED_FLAG = "__ds_desktop_restored__"; // 已触发过"恢复后刷新"（跨 reload 持久化，防无限刷新）
    var BAD_COOKIES_KEY = "__ds_desktop_badcookies__"; // 幽灵 Cookie 黑名单（跨 reload 持久化）
    var MAX_VALUE_LEN = 512 * 1024; // 跳过超过 512KB 的单个值
    var MAX_TOTAL = 8 * 1024 * 1024; // 快照总大小上限 8MB
    var CLEAR_DELAY = 60 * 1000; // 判定"真登出"的连续无登录态时长（毫秒）

    // 内存级标志（初始化时从 localStorage 恢复，保证 reload 后依然生效）
    var restoreReloadDone = false; // 本会话已因恢复 reload 过一次（防无限刷新循环）
    var failedCookieNames = {}; // 本次会话恢复失败的 Cookie 名（幽灵 Cookie 黑名单）
    try {
      if (localStorage.getItem(RESTORED_FLAG) === "1") restoreReloadDone = true;
      var _bad = localStorage.getItem(BAD_COOKIES_KEY);
      if (_bad) {
        var _arr = JSON.parse(_bad);
        if (Array.isArray(_arr)) {
          for (var _i = 0; _i < _arr.length; _i++) {
            if (_arr[_i]) failedCookieNames[_arr[_i]] = true;
          }
        }
      }
    } catch (e) {}

    // 持久化幽灵 Cookie 黑名单（跨 reload）
    function persistBadCookies() {
      try {
        localStorage.setItem(BAD_COOKIES_KEY, JSON.stringify(Object.keys(failedCookieNames)));
      } catch (e) {}
    }

    var lastFp = ""; // 最近一次成功写入快照的"内容指纹"（排除 savedAt，避免时间戳导致重复写）
    var noAuthSince = 0; // 从何时开始连续无登录态（0 = 未开始计时）
    var hadAuthEver = false; // 本会话是否曾快照到过登录态（用于区分"登出"与"一直未登录"）

    // 全量 Cookie 模式（feature: full-cookie-snapshot）：null=探测中 true=可用 false=不可用
    var FULL_COOKIE = null;
    function probeFullCookie() {
      INVOKE("dump_all_cookies")
        .then(function (list) {
          FULL_COOKIE = Array.isArray(list);
        })
        .catch(function () {
          FULL_COOKIE = false;
        });
    }
    probeFullCookie();

    // 异步把全量 Cookie 列表挂到快照上（cookiesFull 字段）；
    // 探测中/不可用时回退 document.cookie（cookiesFull=[]）。
    function attachFullCookies(snap) {
      if (FULL_COOKIE === true) {
        return INVOKE("dump_all_cookies")
          .then(function (list) {
            if (Array.isArray(list)) {
              snap.cookiesFull = list;
              return snap;
            }
            FULL_COOKIE = false;
            snap.cookiesFull = [];
            return snap;
          })
          .catch(function () {
            FULL_COOKIE = false;
            snap.cookiesFull = [];
            return snap;
          });
      }
      snap.cookiesFull = [];
      return Promise.resolve(snap);
    }

    // 内容指纹：比较 localStorage / cookies / cookiesFull，忽略 savedAt。
    // savedAt 每次快照都不同，若直接比较 JSON 字符串会导致"永远有变化"而重复写盘。
    function contentFp(snap) {
      try {
        return JSON.stringify({
          ls: snap && snap.localStorage ? snap.localStorage : {},
          ck: snap && snap.cookies ? String(snap.cookies) : "",
          ckf: snap && Array.isArray(snap.cookiesFull) ? snap.cookiesFull : [],
        });
      } catch (e) {
        return "";
      }
    }

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

    // 是否"看起来有登录态"：cookie 非空 / cookiesFull 非空 / localStorage 存在任意非空值。
    // 用于防止把"页面加载中 / 未登录中间态"当成有效快照写盘。
    function hasAuthLikeData(snap) {
      if (!snap || typeof snap !== "object") return false;
      if (snap.cookies && String(snap.cookies).length > 0) return true;
      if (Array.isArray(snap.cookiesFull) && snap.cookiesFull.length > 0) return true;
      var ls = snap.localStorage;
      if (ls && typeof ls === "object") {
        var keys = Object.keys(ls);
        for (var i = 0; i < keys.length; i++) {
          if (keys[i].indexOf("__ds_") === 0) continue;
          var v = ls[keys[i]];
          if (v !== undefined && v !== null && String(v).length > 0) return true;
        }
      }
      return false;
    }

    // 合并保存：新快照 localStorage 为空则沿用旧快照，cookie / cookiesFull 为空则沿用旧的。
    // 防止"加载中未登录态"覆盖最后一份好快照；真登出由 CLEAR_DELAY + clear_session 处理。
    function mergeSnapshot(prev, next) {
      if (!prev || typeof prev !== "object") return next;
      var nextLs = next.localStorage && typeof next.localStorage === "object" ? next.localStorage : {};
      var hasNewLs = false;
      var keys = Object.keys(nextLs);
      for (var i = 0; i < keys.length; i++) {
        if (keys[i].indexOf("__ds_") !== 0) {
          hasNewLs = true;
          break;
        }
      }
      var nextCookies = next.cookies && String(next.cookies).length > 0 ? next.cookies : "";
      var nextFull =
        next.cookiesFull && Array.isArray(next.cookiesFull) && next.cookiesFull.length > 0
          ? next.cookiesFull
          : [];
      var prevFull =
        prev.cookiesFull && Array.isArray(prev.cookiesFull) && prev.cookiesFull.length > 0
          ? prev.cookiesFull
          : [];
      return {
        localStorage: hasNewLs
          ? nextLs
          : prev.localStorage && typeof prev.localStorage === "object"
            ? prev.localStorage
            : {},
        cookies: nextCookies || (prev.cookies ? String(prev.cookies) : ""),
        cookiesFull: nextFull.length > 0 ? nextFull : prevFull,
        savedAt: next.savedAt || Date.now(),
      };
    }

    // ---------- 保存 ----------
    var saving = false;
    function save() {
      if (saving) return;
      saving = true;
      try {
        var snap = snapshot();
        var hasAuth = hasAuthLikeData(snap);

        if (!hasAuth) {
          // 当前无登录态：不直接覆盖文件（可能是加载中）。
          // 曾登录过且无登录态持续 CLEAR_DELAY → 判定为用户主动登出 → 清空文件快照。
          var now = Date.now();
          if (hadAuthEver) {
            if (!noAuthSince) noAuthSince = now;
            if (now - noAuthSince >= CLEAR_DELAY) {
              noAuthSince = 0;
              hadAuthEver = false;
              try {
                localStorage.removeItem(SNAP_LS_KEY);
              } catch (e) {}
              lastFp = "";
              INVOKE("clear_session").catch(function () {});
            }
          }
          saving = false;
          return;
        }

        hadAuthEver = true;
        noAuthSince = 0;

        // 异步补全全量 Cookie 后统一落盘
        attachFullCookies(snap)
          .then(function (fullSnap) {
            try {
              doSave(fullSnap);
            } catch (e) {
              saving = false;
            }
          })
          .catch(function () {
            saving = false;
          });
      } catch (e) {
        saving = false;
      }
    }

    // 实际写盘（合并旧快照 + 内容指纹去重）
    function doSave(snap) {
      // 合并旧快照，避免字段丢失
      var merged = snap;
      try {
        var cached = localStorage.getItem(SNAP_LS_KEY);
        if (cached) {
          var prev = JSON.parse(cached);
          merged = mergeSnapshot(prev, snap);
        }
      } catch (e) {}

      // 内容指纹去重：内容无变化则不写盘（savedAt 变化不算变化）
      var fp = contentFp(merged);
      if (fp === lastFp) {
        saving = false;
        return;
      }
      var str = JSON.stringify(merged);
      try {
        localStorage.setItem(SNAP_LS_KEY, str);
      } catch (e) {}
      lastFp = fp;
      INVOKE("save_session", { data: str })
        .catch(function () {})
        .finally(function () {
          saving = false;
        });
    }

    // ---------- 恢复 Cookie ----------
    // 优先全量模式（含 HttpOnly）；不可用则回退 document.cookie 逐个设置（带幽灵黑名单）。
    // 返回 Promise<{ changed: boolean }>。
    function restoreCookies(snap) {
      if (Array.isArray(snap.cookiesFull) && snap.cookiesFull.length > 0) {
        return INVOKE("restore_all_cookies", { cookies: snap.cookiesFull })
          .then(function () {
            return { changed: false }; // 全量恢复成功；是否需要刷新由 localStorage 变化决定
          })
          .catch(function () {
            return { changed: false }; // 恢复失败静默，交给原生存储/document.cookie 兜底
          });
      }
      return Promise.resolve().then(function () {
        var changed = false;
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
              if (!name || curNames[name] || failedCookieNames[name]) continue;
              try {
                document.cookie = ck;
                changed = true;
              } catch (e) {
                failedCookieNames[name] = true;
                persistBadCookies();
              }
            }
            // 设置后仍不存在的 → 幽灵 Cookie，本会话黑名单
            var namesNow = {};
            var afterParts = (document.cookie || "").split(";");
            for (var n = 0; n < afterParts.length; n++) {
              var nn = afterParts[n].trim().split("=")[0];
              if (nn) namesNow[nn] = true;
            }
            for (var p = 0; p < parts.length; p++) {
              var ck2 = parts[p].trim();
              if (!ck2) continue;
              var n2 = ck2.split("=")[0];
              if (n2 && !namesNow[n2]) {
                failedCookieNames[n2] = true;
                persistBadCookies();
              }
            }
          }
        } catch (e) {}
        return { changed: changed };
      });
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

          // 幂等：本地缓存与文件快照"内容一致" → 本会话已恢复过（或已被原生存储覆盖），直接跳过。
          // 修复点：此前每次导航都会重复恢复 + reload，配合跨域重定向可形成无限刷新循环。
          // 用内容指纹比较（忽略 savedAt），否则时间戳差异会导致幂等判断失效。
          try {
            var cached = localStorage.getItem(SNAP_LS_KEY);
            if (cached) {
              var cachedSnap = JSON.parse(cached);
              if (contentFp(cachedSnap) === contentFp(snap)) return;
            }
          } catch (e) {}

          var changed = false;

          // 恢复 localStorage（跳过 __ds_ 前缀）
          try {
            if (snap.localStorage && typeof snap.localStorage === "object") {
              var keys = Object.keys(snap.localStorage);
              for (var i = 0; i < keys.length; i++) {
                var k = keys[i];
                if (k.indexOf("__ds_") === 0) continue;
                var v = String(snap.localStorage[k]);
                if (localStorage.getItem(k) !== v) {
                  try {
                    localStorage.setItem(k, v);
                    changed = true;
                  } catch (e) {}
                }
              }
            }
          } catch (e) {}

          // 恢复 Cookie（全量优先 / document.cookie 回退）→ 对齐保存 → 仅 reload 一次
          restoreCookies(snap)
            .then(function (r) {
              if (r.changed) changed = true;

              // 恢复完成：把（合并后的）完整状态写回文件 + 本地缓存，与原生存储对齐
              try {
                var restored = mergeSnapshot(snap, snapshot());
                return attachFullCookies(restored).then(function (fullRestored) {
                  var rs = JSON.stringify(fullRestored);
                  try {
                    localStorage.setItem(SNAP_LS_KEY, rs); // 缓存必须写（供幂等判断）
                  } catch (e) {}
                  hadAuthEver = hasAuthLikeData(fullRestored);
                  var rfp = contentFp(fullRestored);
                  if (rfp !== lastFp) {
                    lastFp = rfp;
                    INVOKE("save_session", { data: rs }).catch(function () {});
                  }
                  return fullRestored;
                });
              } catch (e) {
                return null;
              }
            })
            .then(function () {
              // 仅 reload 一次，让 SPA 带着恢复的数据初始化（避免"未登录"闪屏）
              if (changed && !restoreReloadDone) {
                restoreReloadDone = true; // 持久化标记：跨 reload 依旧生效，防无限刷新
                try {
                  localStorage.setItem(RESTORED_FLAG, "1");
                } catch (e) {}
                setTimeout(function () {
                  try {
                    location.reload();
                  } catch (e) {}
                }, 150);
              }
            })
            .catch(function () {});
        })
        .catch(function () {});
    }

    // ---------- 链接处理（独立 try：此处异常不得影响上面的会话恢复/保存） ----------
    try {
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
            if (a.target === "_blank" || (a.rel && a.rel.indexOf("external") >= 0)) {
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
    } catch (e) {
      /* 链接处理异常不影响会话持久化 */
    }

    // ---------- 定时同步 ----------
    restoreOnce();

    // 5s 轮询：save() 内部自带防污染（空态不覆盖）、内容指纹去重（savedAt 不算变化）、
    // 以及登出判定（曾登录 → 空态持续 CLEAR_DELAY → clear_session）。
    setInterval(function () {
      try {
        save();
      } catch (e) {}
    }, 5000);

    // 窗口隐藏时保存一次（此时页面仍在运行，快照是稳定有效的）
    document.addEventListener("visibilitychange", function () {
      if (document.visibilityState === "hidden") save();
    });

    // 注意：不在 pagehide / beforeunload 中保存。
    // 原因：导航期间异步 IPC 不可靠，且 reload/关闭时的"页面中间态"极易污染好快照
    // （这正是 Windows 上"转圈后重启登录失效"的根因之一）。
  } catch (e) {
    // 绝不破坏宿主页面
  }
})();
