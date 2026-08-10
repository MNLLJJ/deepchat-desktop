// DS_SELFTEST 自检脚本 —— 仅在设置环境变量 DS_SELFTEST=1 时注入，用于验证持久化
(function () {
  try {
    var INVOKE = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
    if (!INVOKE) return;
    var OLD = localStorage.getItem("__ds_test_marker__");
    var now = String(Date.now());
    localStorage.setItem("__ds_test_marker__", now);
    try {
      document.cookie = "__ds_test_cookie__=" + now + "; path=/; max-age=86400";
    } catch (e) {}
    var cookieSeen = (document.cookie || "").indexOf("__ds_test_cookie__") >= 0;
    // 浏览器指纹 dump（用于对比真实 Chrome，定位"使用环境异常"检测点）
    var fingerprint = {
      ua: navigator.userAgent,
      uaData: typeof navigator.userAgentData !== "undefined",
      chrome: typeof window.chrome !== "undefined",
      plugins: navigator.plugins ? navigator.plugins.length : -1,
      mimeTypes: navigator.mimeTypes ? navigator.mimeTypes.length : -1,
      webkit: typeof window.webkit !== "undefined",
      webkitMsgHandlers:
        typeof window.webkit !== "undefined" &&
        window.webkit.messageHandlers !== undefined,
      webdriver: navigator.webdriver,
      platform: navigator.platform,
      languages: navigator.languages ? navigator.languages.join(",") : "",
      hardwareConcurrency: navigator.hardwareConcurrency,
      deviceMemory: navigator.deviceMemory,
      maxTouchPoints: navigator.maxTouchPoints,
      vendor: navigator.vendor,
      oscpu: navigator.oscpu,
      appVersion: navigator.appVersion,
    };
    // 直接复算 DeepSeek 的“使用环境异常”判定逻辑（从 main.7863ea53ee.js 解混淆）
    var envCheck = (function () {
      var ES =
        navigator.userAgent.toLowerCase().indexOf("electron") >= 0 ||
        (typeof window !== "undefined" &&
          "process" in window &&
          !!window.process &&
          window.process.type === "renderer");
      var EE =
        navigator.userAgent.indexOf("Tauri") >= 0 ||
        ("__TAURI__" in window && typeof window.__TAURI__ !== "undefined");
      return { ES: !!ES, EE: !!EE, trigger: !!(ES || EE) };
    })();
    // 延迟检查页面是否真的渲染了“使用环境异常”弹窗
    setTimeout(function () {
      var bodyText = (document.body && document.body.innerText) || "";
      var unsafeShown =
        bodyText.indexOf("使用环境异常") >= 0 ||
        bodyText.indexOf("Abnormal usage environment") >= 0;
      INVOKE("debug_log", {
        msg:
          "selftest-2 envCheck=" +
          JSON.stringify(envCheck) +
          " unsafeShown=" +
          unsafeShown +
          " url=" +
          location.href,
      }).catch(function () {});
    }, 6000);
    INVOKE("debug_log", {
      msg:
        "selftest marker_old=" +
        (OLD || "none") +
        " new=" +
        now +
        " cookie_present=" +
        cookieSeen +
        " fp=" +
        JSON.stringify(fingerprint) +
        " envCheck=" +
        JSON.stringify(envCheck) +
        " url=" +
        location.href,
    }).catch(function () {});
  } catch (e) {}
})();
