/* ── 桌面壳桥接层 ──────────────────────────────────────────────────
   浏览器里打开：什么都不做（active=false），原型照旧。
   在 Tauri 里：窗口 = 面板的实际边界，三种模式对应三种贴边方式。

   窗口尺寸不能拍脑袋定，得跟 CSS 动画配合：
   · 展开时先把窗口撑到最终尺寸，面板自己在窗口里做 380ms 的高度动画；
   · 收起时窗口先别动，等动画走完再缩，否则面板会被窗口边缘硬切一刀。   */

(function () {
  'use strict';

  var API = window.__TAURI__;
  var SHELL = { active: false, handlers: {} };
  window.__DESKLET_SHELL__ = SHELL;

  if (!API || !API.core || !API.core.invoke) return;   /* 浏览器：空壳退出 */
  SHELL.active = true;

  var invoke = API.core.invoke;
  var listen = API.event && API.event.listen;

  document.documentElement.classList.add('app');

  /* 阴影留白：收起时窗口严格贴合（不挡桌面），展开时四周留出投影位置 */
  var PAD_D = 12;
  var PAD_B = 20;
  var PAD_F = 16;
  /* 悬浮球窗口比球本身（46px）大 4px：塞进 54 的窗口里，
     四周留出的 4px 透明边刚好够 hover 放大和一圈小投影，不会被窗口边缘切平 */
  var BALL = 54;
  var HANDLE_W = 210;   /* 和 .handle 的宽度保持一致 */

  var work = { x: 0, y: 0, w: 1280, h: 800 };
  var last = { m: '', w: 0, h: 0 };
  var booted = false;

  function body() { return document.body; }
  function drawer() { return document.getElementById('drawer'); }
  function num(v) { var n = parseFloat(v); return isFinite(n) ? n : 0; }
  function props(el, k) { return num(getComputedStyle(el).getPropertyValue(k)); }

  function mode() {
    var b = body().classList;
    if (b.contains('float')) return 'float';
    if (b.contains('side')) return b.contains('edge-left') ? 'left' : 'side';
    return b.contains('edge-bottom') ? 'bottom' : 'dock';
  }
  function isCollapsed() {
    var d = drawer();
    return !!d && d.classList.contains('collapsed');
  }

  /* 悬浮窗位置由窗口自己记；这里只负责跨重启恢复 */
  var POS_KEY = 'desklet.shell.pos.v2';
  function loadPos() {
    try {
      var raw = localStorage.getItem(POS_KEY) || localStorage.getItem('desklet.shell.pos.v1');
      if (!raw) return null;
      var p = JSON.parse(raw);
      if (isFinite(p.x) && isFinite(p.y)) return p;
    } catch (e) {}
    return null;
  }
  function savePos() {
    return invoke('get_float_pos').then(function (p) {
      if (Array.isArray(p) && Number.isFinite(p[0]) && Number.isFinite(p[1])) {
        try { localStorage.setItem(POS_KEY, JSON.stringify({ x: p[0], y: p[1], physical: true })); } catch (e) {}
      }
    }).catch(function () {});
  }

  /* 目标几何：窗口该多大、该在哪 */
  function target(expanding) {
    var m = mode();
    var col = isCollapsed() && !expanding;
    var d = drawer();
    var pw = Math.round(props(body(), '--pw')) || 392;

    if (m === 'dock' || m === 'bottom') {
      var handleHeight = props(body(), '--handle-h') || 18;
      var ph = Math.round(num(d && d.style.getPropertyValue('--ph'))) || 384;
      /* 收起时窗口缩到把手那么宽：不挡桌面，也不会被无形区域误触发 */
      if (col) return { m: m, w: HANDLE_W, h: handleHeight };
      return { m: m, w: pw + PAD_D * 2, h: handleHeight + ph + PAD_B };
    }
    if (m === 'side' || m === 'left') {
      if (col) return { m: m, w: 15, h: HANDLE_W };
      return { m: m, w: 15 + pw + 10 + PAD_D, h: 0 };
    }
    var fh = Math.round(num(d && d.style.getPropertyValue('--fh'))) || Math.min(640, work.h - 60);
    if (col) return { m: m, w: BALL, h: BALL };
    return { m: m, w: pw + PAD_F * 2, h: fh + PAD_F * 2 };
  }

  function same(a, b) { return a.m === b.m && a.w === b.w && a.h === b.h; }

  /* last 只是"上一次请求过的几何"，不保证窗口真变成了那么大：
     Windows 在处理 DPI 变化、贴边或钳制时会自己改尺寸。
     所以每次拿实际视口对一下，对不上就照旧尺寸重发，别被缓存永久挡死。 */
  function viewportMatches(size) {
    return (!size.w || Math.abs(innerWidth - size.w) <= 1)
      && (!size.h || Math.abs(innerHeight - size.h) <= 1);
  }

  async function settleStartupViewport(attempts) {
    for (var i = 0; i < attempts; i++) {
      if (SHELL.dragging || SHELL.resizing || drawer().classList.contains('folding')) return;
      await sync();
      await new Promise(function (resolve) { setTimeout(resolve, 30); });
      if (viewportMatches(target())) return;
    }
  }

  function apply(t, resizeAnchor) {
    var key = { m: t.m, w: Math.round(t.w), h: Math.round(t.h) };
    /* h=0 表示"高度交给工作区"（贴边模式），实际视口会是满屏高，不能拿它比 */
    var drift = !viewportMatches(last);
    if (same(key, last) && !drift && !resizeAnchor) return;
    last = key;
    return invoke('apply_geo', { mode: key.m, w: key.w, h: key.h, resizeAnchor: resizeAnchor || null }).then(function () {
      if (key.m === 'float' && key.w === BALL && key.h === BALL) return savePos();
    }).catch(function () {
      /* 没送到就别把它记成已生效，不然之后一直差这一截 */
      last = { m: '', w: 0, h: 0 };
    });
  }

  /* 把窗口同步到目标几何。
     什么时候调由调用方决定：收起必须等 height/width 的过渡真的结束
     （index.html 监听 transitionend 后才调这里），本文件再延时一次
     就会变成 320+380 的两段等待。参数保留只为读起来有语义，不参与分支。 */
  var ready = false;
  function sync() {
    if (!ready || SHELL.dragging || SHELL.resizing || drawer().classList.contains('folding')) return;
    if (isCollapsed() && body().matches('.dock.drawer-busy')) return;
    return apply(target());
  }

  SHELL.sync = sync;
  SHELL.prepareDockOpen = async function () {
    if (!ready || SHELL.dragging || SHELL.resizing) return;
    var expanded = target(true);
    await apply(expanded);
    for (var i = 0; i < 8 && !viewportMatches(expanded); i++) {
      await new Promise(function (resolve) { setTimeout(resolve, 16); });
    }
  };
  SHELL.resizeOrigin = function () { return invoke('resize_origin'); };
  SHELL.resizeCursor = function () { return invoke('resize_cursor'); };
  SHELL.resizeTo = function (anchor) { return apply(target(), anchor); };
  SHELL.h = function () { return Math.round(work.h); };
  SHELL.w = function () { return Math.round(work.w); };
  SHELL.mode = mode;
  SHELL.collapsed = isCollapsed;
  SHELL.savePos = savePos;

  SHELL.launch = function (target) {
    return invoke('launch', { target: String(target || '') }).catch(function (e) {
      return Promise.reject(e || new Error('启动失败'));
    });
  };
  SHELL.hide = function () { return invoke('hide_window').catch(function () {}); };
  /* 收起后交还焦点，用户接着 Ctrl+V 就能粘到原来的窗口 */
  SHELL.releaseFocus = function () { invoke('restore_focus').catch(function () {}); };
  SHELL.topmost = function (on) { invoke('set_topmost', { on: !!on }).catch(function () {}); };
  SHELL.startDrag = async function () {
    SHELL.dragging = true;
    var next = null;
    try {
      next = await invoke('start_drag');
      if (next) {
        savePos();
        await refreshWorkArea();
      }
    } catch (err) {
      console.warn('[desklet] 移动窗口失败', err);
    } finally {
      SHELL.dragging = false;
      last = { m: '', w: 0, h: 0 };
    }
    return next;
  };
  SHELL.autostart = function (on) { return invoke('set_autostart', { on: !!on }); };
  SHELL.getAutostart = function () { return invoke('get_autostart'); };
  /* 拖进来的图片要读原件；WebView 拿不到绝对路径，只能由原生拖拽事件递过来 */
  SHELL.readFile = function (path) { return invoke('read_file_b64', { path: String(path) }); };
  /* 应用卡的真实图标：系统图标 → PNG dataURL。拿不到就 reject，由调用方回退首字 */
  SHELL.clipboardPaths = function () { return invoke('clipboard_paths'); };
  SHELL.localShortcut = function (path) { return invoke('local_shortcut', { path: path }); };
  SHELL.icon = function (path) { return invoke('file_icon', { path: String(path) }); };
  /* 前台是否全屏应用（游戏 / 播放器 / PPT 放映）：悬停展开要避开它 */
  SHELL.fullscreen = function () { return invoke('fullscreen_app'); };


  /* 原生文件拖拽：路径只有这条通道拿得到。
     开了 dragDropEnabled 之后 HTML5 的 drop 事件不会再触发，
     换成 tauri 的 drag-drop 事件，路径是绝对路径 */
  function currentWebview() {
    var T = API;
    try {
      /* 全局 API 里这个函数挂在 webviewWindow 下，不是 webview */
      if (T.webviewWindow && T.webviewWindow.getCurrentWebviewWindow) {
        return T.webviewWindow.getCurrentWebviewWindow();
      }
      if (T.webview && T.webview.getCurrentWebview) return T.webview.getCurrentWebview();
      if (T.window && T.window.getCurrentWindow) return T.window.getCurrentWindow();
    } catch (e) {}
    return null;
  }

  function watchDrop() {
    var win = currentWebview();
    if (!win || !win.onDragDropEvent) {
      console.warn('[desklet] 拖拽事件不可用，拖文件进来不会生效');
      return;
    }
    win.onDragDropEvent(function (e) {
      var p = (e && e.payload) || {};
      if (p.type === 'enter' || p.type === 'over') {
        document.body.classList.add('dropping');
      } else if (p.type === 'leave') {
        document.body.classList.remove('dropping');
      } else if (p.type === 'drop') {
        document.body.classList.remove('dropping');
        var paths = p.paths || [];
        if (SHELL.handlers.files) SHELL.handlers.files(paths);
      }
    }).then(function () {
      /* 注册成功才置位，方便自检"拖拽到底接上没有" */
      SHELL.dropReady = true;
    }).catch(function () {
      console.warn('[desklet] 拖拽事件注册失败');
    });
  }


  function refreshWorkArea() {
    return invoke('work_area_cmd').then(function (r) {
      if (Array.isArray(r) && r.length === 5) {
        work = { x: r[0], y: r[1], w: r[2], h: r[3] };
        body().style.setProperty('--side-handle-top', r[4] + 'px');
      }
    }).catch(function () {});
  }

  SHELL.saveBackup = function (filename, contents) {
    return invoke('save_backup', { filename: filename, contents: contents });
  };
  SHELL.getShortcut = function () { return invoke('get_shortcut'); };
  SHELL.setShortcut = function (shortcut) { return invoke('set_shortcut', { shortcut: shortcut }); };
  SHELL.quit = async function () {
    await savePos();
    return invoke('quit_app');
  };

  /* ── 启动 ──────────────────────────────────────────────── */
  var savedPos = loadPos();
  var restoring = savedPos ? invoke('set_float_pos', {
    x: savedPos.x, y: savedPos.y, physical: savedPos.physical === true
  }).catch(function () {}) : Promise.resolve();

  var startup = restoring.then(refreshWorkArea).then(function () {
    /* 面板默认高度别按窗口高算（窗口高 = 面板高，会自我循环），
       先塞一个基于工作区的初值，后面 fitPanel 会覆盖 */
    var d = drawer();
    if (d) {
      var ph = Math.round(Math.min(560, work.h * 0.62));
      if (!num(d.style.getPropertyValue('--ph'))) d.style.setProperty('--ph', ph + 'px');
      if (!num(d.style.getPropertyValue('--fh'))) {
        d.style.setProperty('--fh', Math.round(Math.min(640, work.h - 60)) + 'px');
      }
    }
    watchDrop();
    if (listen) {
      listen('shell://drop-hover', function (event) {
        body().classList.toggle('dropping', event.payload === true);
      });
      listen('shell://drop-data', function (event) {
        if (SHELL.handlers.drop) SHELL.handlers.drop(event.payload || {});
      }).then(function () {
        return invoke('setup_shortcut_drop');
      }).then(function (count) {
        SHELL.nativeDropTargets = count;
      }).catch(function (err) {
        console.warn('[desklet] Windows 拖放注册失败', err);
      });
      listen('shell://summon', function () {
        if (SHELL.handlers.summon) SHELL.handlers.summon();
      });
      listen('shell://toggle', function () {
        if (SHELL.handlers.toggle) SHELL.handlers.toggle();
      });
    }
  });

  /* 主脚本初始化完会调一次：定窗口 → 显示 */
  SHELL.boot = function () {
    if (booted) return;
    booted = true;
    startup.then(async function () {
      await invoke('reset_viewport_zoom').catch(function () {});
      if (SHELL.handlers.ready) SHELL.handlers.ready();
      ready = true;
      await settleStartupViewport(7);
      await invoke('reveal');
      // Some WebView2 versions deliver the final resize only after the HWND is shown.
      await settleStartupViewport(4);
    }).catch(function (err) { console.warn('[desklet] 显示窗口失败', err); });
  };
})();
