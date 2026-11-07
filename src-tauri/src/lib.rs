use std::sync::{atomic::{AtomicBool, Ordering}, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, WebviewWindow, WindowEvent};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

mod backup;
mod icons;
mod shortcuts;

/* ── Win32：直接调，避免为一个矩形拖一整个 windows crate ───────────── */

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RECT {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MONITORINFO {
    cb_size: u32,
    rc_monitor: RECT,
    rc_work: RECT,
    dw_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct POINT {
    x: i32,
    y: i32,
}

#[link(name = "user32")]
extern "system" {
    fn MonitorFromPoint(pt: POINT, flags: u32) -> isize;
    fn GetMonitorInfoW(hmonitor: isize, lpmi: *mut MONITORINFO) -> i32;
    fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn GetForegroundWindow() -> isize;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn BringWindowToTop(hwnd: isize) -> i32;
    fn AttachThreadInput(id_attach: u32, id_attach_to: u32, f_attach: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
    fn GetCurrentThreadId() -> u32;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    fn IsWindow(hwnd: isize) -> i32;
    fn IsWindowVisible(hwnd: isize) -> i32;
    fn GetWindowRect(hwnd: isize, rc: *mut RECT) -> i32;
    fn GetCursorPos(point: *mut POINT) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
    fn GetSystemMetrics(index: i32) -> i32;
    fn ReleaseCapture() -> i32;
    fn SendMessageW(hwnd: isize, message: u32, wparam: usize, lparam: isize) -> isize;
    fn GetDpiForWindow(hwnd: isize) -> u32;
}

#[link(name = "shcore")]
extern "system" {
    fn GetDpiForMonitor(hmon: isize, dpi_type: i32, dpi_x: *mut u32, dpi_y: *mut u32) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: isize,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> isize;
    /* 查「用户是否在全屏应用里」：游戏、播放器、PPT 放映都会让系统进入这些状态。
       返回值见 QUNS_*；HRESULT 形式，>=0 才算拿到。 */
    fn SHQueryUserNotificationState(state: *mut i32) -> i32;
}

/* QUNS_* 里需要抑制悬停的那几个：忙（全屏程序）、D3D 独占全屏、放映模式 */
const QUNS_BUSY: i32 = 2;
const QUNS_RUNNING_D3D_FULL_SCREEN: i32 = 3;
const QUNS_PRESENTATION_MODE: i32 = 4;

const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_FRAMECHANGED: u32 = 0x0020;
const HWND_TOPMOST: isize = -1;
const HWND_NOTOPMOST: isize = -2;
const MONITOR_DEFAULTTONEAREST: u32 = 2;
const MONITOR_DEFAULTTOPRIMARY: u32 = 1;
const MONITOR_DEFAULTTONULL: u32 = 0;

#[derive(Default)]
struct ShellState {
    /* 唤出前的前台窗口：收起时还回去，别把焦点吞掉 */
    prev_fg: Mutex<isize>,
    // Physical pixels: floating top-right interior point or dock drop point.
    anchor: Mutex<Option<(f64, f64)>>,
    dragging: AtomicBool,
    summon_shortcut: Mutex<Option<Shortcut>>,
    /* 最后一次应用的逻辑几何：DPI 变化后按它重放一遍 */
    last_geo: Mutex<(String, f64, f64)>,
}

fn hwnd_of(w: &WebviewWindow) -> isize {
    w.hwnd().map(|h| h.0 as isize).unwrap_or(0)
}

/* 窗口是不是真在屏幕上。
   别用 Tauri 的 is_visible()：我们是拿 Win32 直接 ShowWindow 的，
   它内部那份可见性状态不跟着变，会一直说"可见"，热键就永远走不到唤出分支 */
fn is_shown(hwnd: isize) -> bool {
    hwnd != 0 && unsafe { IsWindowVisible(hwnd) } != 0
}

/* 真正藏起来。同样绕开 Tauri：它在配置里登记的是 visible:false，
   调 window.hide() 时以为已经藏好就直接返回了，什么也没发生 */
fn hide_native(hwnd: isize) {
    if hwnd != 0 {
        unsafe {
            ShowWindow(hwnd, 0 /* SW_HIDE */);
        }
    }
}

/* 窗口当前所在显示器的 DPI 比例 */
fn dpi_scale(hwnd: isize) -> f64 {
    let d = unsafe { GetDpiForWindow(hwnd) };
    if d == 0 { 1.0 } else { d as f64 / 96.0 }
}

fn window_rect(hwnd: isize) -> Option<RECT> {
    let mut rect = RECT::default();
    (hwnd != 0 && unsafe { GetWindowRect(hwnd, &mut rect) } != 0).then_some(rect)
}

/* 指定显示器的缩放。目标屏和当前屏不是同一块时（多屏不同缩放），
   必须问目标屏，不能拿窗口现在的 dpi 凑 */
fn monitor_scale(hmon: isize) -> f64 {
    let mut x: u32 = 96;
    let mut y: u32 = 96;
    let rc = unsafe { GetDpiForMonitor(hmon, 0 /* MDT_EFFECTIVE_DPI */, &mut x, &mut y) };
    if rc != 0 || x == 0 { dpi_scale(unsafe { GetForegroundWindow() }) } else { x as f64 / 96.0 }
}

fn primary_monitor() -> isize {
    unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }
}

/* 显示器工作区（物理像素，已排除任务栏） */
fn monitor_work(hmon: isize) -> (f64, f64, f64, f64) {
    let mut mi = MONITORINFO {
        cb_size: std::mem::size_of::<MONITORINFO>() as u32,
        rc_monitor: RECT::default(),
        rc_work: RECT::default(),
        dw_flags: 0,
    };
    if unsafe { GetMonitorInfoW(hmon, &mut mi) } == 0 {
        return (0.0, 0.0, 1920.0, 1080.0);
    }
    (
        mi.rc_work.left as f64,
        mi.rc_work.top as f64,
        (mi.rc_work.right - mi.rc_work.left) as f64,
        (mi.rc_work.bottom - mi.rc_work.top) as f64,
    )
}

/* Choose the display from the saved drop position. */
fn target_monitor(anchor: Option<(f64, f64)>) -> isize {
    if let Some((x, y)) = anchor {
        let monitor = unsafe {
            MonitorFromPoint(POINT { x: x as i32, y: y as i32 }, MONITOR_DEFAULTTONEAREST)
        };
        if monitor != 0 { return monitor; }
    }
    primary_monitor()
}

fn place(hwnd: isize, x: f64, y: f64, w: f64, h: f64, extra: u32) {
    unsafe {
        SetWindowPos(
            hwnd,
            0,
            x.round() as i32,
            y.round() as i32,
            w.round().max(1.0) as i32,
            h.round().max(1.0) as i32,
            SWP_NOZORDER | SWP_NOACTIVATE | extra,
        );
    }
}

/* 前端只认 CSS 像素，这里换算成物理：工作区尺寸 / 缩放 */
#[tauri::command]
fn work_area_cmd(state: tauri::State<'_, ShellState>) -> (f64, f64, f64, f64, f64) {
    let pos = *state.anchor.lock().unwrap();
    let m = target_monitor(pos);
    let s = monitor_scale(m);
    let (x, y, w, h) = monitor_work(m);
    let handle = (210.0 * s).round().min(h);
    let center = pos.map_or(y + h / 2.0, |(_, ay)| ay);
    let handle_top = (center - handle / 2.0).clamp(y, y + h - handle) - y;
    (x / s, y / s, w / s, h / s, handle_top / s)
}

#[tauri::command]
fn resize_origin(window: WebviewWindow, state: tauri::State<'_, ShellState>) -> Option<[f64; 9]> {
    let rect = window_rect(hwnd_of(&window))?;
    let monitor = target_monitor(*state.anchor.lock().unwrap());
    let (x, y, w, h) = monitor_work(monitor);
    Some([rect.left.into(), rect.top.into(), rect.right.into(), rect.bottom.into(),
        x, y, w, h, monitor_scale(monitor)])
}

#[tauri::command]
fn resize_cursor() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    (unsafe { GetCursorPos(&mut point) } != 0).then_some((point.x, point.y))
}

/* The window and its anchor are updated together so resizing can keep the opposite edge fixed. */
#[tauri::command]
fn apply_geo(
    window: WebviewWindow,
    state: tauri::State<'_, ShellState>,
    mode: String,
    w: f64,
    h: f64,
    resize_anchor: Option<(f64, f64)>,
) {
    if state.dragging.load(Ordering::Relaxed) { return; }
    let folding = {
        let mut previous = state.last_geo.lock().unwrap();
        let folding = mode == "float" && w == 54.0 && h == 54.0
            && previous.0 == "float" && previous.1 > 54.0;
        *previous = (mode.clone(), w, h);
        folding
    };
    let position = if folding {
        window_rect(hwnd_of(&window)).map(|r| (f64::from(r.right - 1), f64::from(r.top)))
    } else {
        resize_anchor.filter(|(x, y)| x.is_finite() && y.is_finite())
    };
    if let Some(position) = position {
        *state.anchor.lock().unwrap() = Some(position);
    }
    apply_geo_inner(&window, &state, &mode, w, h);
}

fn apply_geo_inner(window: &WebviewWindow, state: &ShellState, mode: &str, w: f64, h: f64) {
    let anchor = *state.anchor.lock().unwrap();
    let monitor = target_monitor(anchor);
    let scale = monitor_scale(monitor);
    let (mx, my, mw, mh) = monitor_work(monitor);
    let pw = (w * scale).round().clamp(1.0, mw);
    let ph = if h == 0.0 { mh } else { (h * scale).round().clamp(1.0, mh) };
    let (ax, ay) = anchor.unwrap_or((mx + mw / 2.0, my + mh / 2.0));
    let (x, y) = match mode {
        "left" | "side" => {
            let x = if mode == "left" { mx } else { mx + mw - pw };
            (x, (ay - ph / 2.0).clamp(my, my + mh - ph))
        }
        "float" => {
            let (right, top) = anchor.unwrap_or((mx + mw - 24.0 * scale, my + 64.0 * scale));
            ((right - pw + 1.0).clamp(mx, mx + mw - pw), top.clamp(my, my + mh - ph))
        }
        _ => {
            let y = if mode == "bottom" { my + mh - ph } else { my };
            ((ax - pw / 2.0).clamp(mx, mx + mw - pw), y)
        }
    };
    place(hwnd_of(window), x, y, pw, ph, SWP_FRAMECHANGED);
}

#[tauri::command]
fn get_float_pos(state: tauri::State<'_, ShellState>) -> Option<(f64, f64)> {
    *state.anchor.lock().unwrap()
}

#[tauri::command]
fn set_float_pos(state: tauri::State<'_, ShellState>, window: WebviewWindow, x: f64, y: f64, physical: bool) {
    let scale = if physical { 1.0 } else { dpi_scale(hwnd_of(&window)) };
    let (x, y) = (x * scale, y * scale);
    if !x.is_finite() || !y.is_finite() { return; }
    let monitor = unsafe {
        MonitorFromPoint(POINT { x: x as i32, y: y as i32 }, MONITOR_DEFAULTTONULL)
    };
    *state.anchor.lock().unwrap() = (monitor != 0).then_some((x, y));
}

#[tauri::command]
fn set_topmost(window: WebviewWindow, on: bool) {
    unsafe {
        SetWindowPos(
            hwnd_of(&window),
            if on { HWND_TOPMOST } else { HWND_NOTOPMOST },
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

#[tauri::command]
fn reveal(window: WebviewWindow) {
    let h = hwnd_of(&window);
    /* 开机自启时会走到这里：只显示不抢焦点，别打扰登录后正在用的程序 */
    unsafe {
        ShowWindow(h, 4 /* SW_SHOWNOACTIVATE */);
        SetWindowPos(h, 0, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
}

/* 把前台还给唤出前的那个窗口。后台线程上 SetForegroundWindow 常静默失败，
   失败就挂到当前前台线程再试一次 */
fn give_focus_to(hwnd: isize) {
    if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
        return;
    }
    unsafe {
        if SetForegroundWindow(hwnd) != 0 {
            return;
        }
        let fg = GetForegroundWindow();
        let fg_thread = if fg != 0 { GetWindowThreadProcessId(fg, std::ptr::null_mut()) } else { 0 };
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me {
            AttachThreadInput(fg_thread, me, 1);
            SetForegroundWindow(hwnd);
            AttachThreadInput(fg_thread, me, 0);
        }
    }
}

/* 收起面板时把焦点还给刚才那个程序：不然复制完按 Ctrl+V 会打到我们身上 */
#[tauri::command]
fn restore_focus(window: WebviewWindow, state: tauri::State<'_, ShellState>) {
    let prev = *state.prev_fg.lock().unwrap();
    if prev != hwnd_of(&window) {
        give_focus_to(prev);
    }
}

#[tauri::command]
fn hide_window(window: WebviewWindow, state: tauri::State<'_, ShellState>) {
    let prev = *state.prev_fg.lock().unwrap();
    hide_native(hwnd_of(&window));
    give_focus_to(prev);
}

#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    app.exit(0);
}

/* 应用卡/文件夹/网址都走 ShellExecute，路径带空格也不会断 */
#[tauri::command]
fn launch(target: String) -> Result<(), String> {
    let t = target.trim().trim_matches('"');
    if t.is_empty() || t.contains('\0') {
        return Err("没有可启动的目标".into());
    }
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let op = wide("open");
    let target_path = std::path::Path::new(t);
    let work_dir = target_path.is_absolute().then_some(target_path)
        .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")))
        .and_then(|path| path.parent()).map(|parent| wide(&parent.to_string_lossy()));
    let file = wide(t);
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut::<std::ffi::c_void>() as isize,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            work_dir.as_ref().map_or(std::ptr::null(), |dir| dir.as_ptr()),
            1,
        )
    };
    if r as isize <= 32 {
        /* ShellExecuteW 失败时返回的是 Win32 错误码，直接甩数字没人看得懂 */
        let code = r as isize;
        let why = match code {
            2 => "找不到这个文件或文件夹",
            3 => "找不到路径",
            5 => "没有权限打开",
            15 => "不知道用什么程序打开它",
            31 => "没有关联的打开方式",
            _ => "打不开",
        };
        Err(format!("{why}（错误码 {code}）"))
    } else {
        Ok(())
    }
}

/* 拖进来的图片要读原件：WebView 的 File 对象拿不到绝对路径，
   只能由拖拽事件把路径递过来，再回这里读字节 */
#[tauri::command]
fn read_file_b64(path: String) -> Result<String, String> {
    const MAX: u64 = 24 * 1024 * 1024;
    let p = std::path::Path::new(&path);
    let meta = std::fs::metadata(p).map_err(|_| "读不到这个文件".to_string())?;
    if !meta.is_file() {
        return Err("这不是文件".into());
    }
    if meta.len() > MAX {
        return Err("图片太大（超过 24MB）".into());
    }
    let bytes = std::fs::read(p).map_err(|_| "读不到这个文件".to_string())?;
    Ok(base64_encode(&bytes))
}

#[tauri::command]
fn file_icon(path: String) -> Result<String, String> {
    icons::data_url(&path)
}

#[tauri::command]
fn local_shortcut(path: String) -> Option<String> {
    let path = std::path::Path::new(path.trim().trim_matches('"'));
    (path.is_absolute() && path.exists()).then(|| path.to_string_lossy().into_owned())
}

/* 自己写一个，省得为一个编码拖一个 crate 进来 */
pub(crate) fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[tauri::command]
fn get_autostart(app: tauri::AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, on: bool) -> bool {
    let al = app.autolaunch();
    let _ = if on { al.enable() } else { al.disable() };
    al.is_enabled().unwrap_or(false)
}

const SNAP_DISTANCE: f64 = 28.0;

fn snap_mode(point: POINT, work: (f64, f64, f64, f64), scale: f64) -> &'static str {
    let (x, y) = (f64::from(point.x), f64::from(point.y));
    let (left, top, w, h) = work;
    let edges = [
        ("dock", y - top), ("bottom", top + h - y),
        ("left", x - left), ("side", left + w - x),
    ];
    edges.into_iter()
        .map(|(edge, distance)| (edge, distance.max(0.0)))
        .filter(|(_, distance)| *distance <= SNAP_DISTANCE * scale)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map_or("float", |(edge, _)| edge)
}

fn drag_to_edge(window: &WebviewWindow, state: &ShellState) -> Option<&'static str> {
    const VK_LBUTTON: i32 = 1;
    const VK_RBUTTON: i32 = 2;
    const SM_SWAPBUTTON: i32 = 23;
    const WM_NCLBUTTONDOWN: u32 = 0x00A1;
    const HTCAPTION: usize = 2;
    let hwnd = hwnd_of(window);
    let before = window_rect(hwnd)?;
    let mut cursor = POINT::default();
    let primary = if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } == 0 { VK_LBUTTON } else { VK_RBUTTON };
    if unsafe { GetAsyncKeyState(primary) } >= 0 || unsafe { GetCursorPos(&mut cursor) } == 0 {
        return None;
    }
    let coordinates = ((cursor.x as u16 as u32) | ((cursor.y as u16 as u32) << 16)) as isize;
    // Windows runs the move loop until mouse release or Escape.
    unsafe {
        ReleaseCapture();
        SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, coordinates);
    }
    let after = window_rect(hwnd)?;
    if (before.left, before.top) == (after.left, after.top)
        || unsafe { GetCursorPos(&mut cursor) } == 0
    {
        return None;
    }
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    let scale = monitor_scale(monitor);
    let work = monitor_work(monitor);
    let mode = snap_mode(cursor, work, scale);
    let (x, y) = (f64::from(cursor.x), f64::from(cursor.y));
    let (mx, my, mw, mh) = work;
    let old_float = state.last_geo.lock().unwrap().0 == "float";
    let anchor = if mode == "float" && old_float {
        (f64::from(after.right - 1), f64::from(after.top))
    } else if mode == "float" {
        (x + SNAP_DISTANCE * scale, y - 16.0 * scale)
    } else {
        (x, y)
    };
    *state.anchor.lock().unwrap() = Some((
        anchor.0.clamp(mx, mx + mw - 1.0),
        anchor.1.clamp(my, my + mh - 1.0),
    ));
    Some(mode)
}

#[tauri::command]
async fn start_drag(window: WebviewWindow) -> Result<Option<&'static str>, String> {
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    let drag_window = window.clone();
    window.run_on_main_thread(move || {
        let state = drag_window.state::<ShellState>();
        if state.dragging.swap(true, Ordering::Relaxed) {
            let _ = tx.try_send(None);
            return;
        }
        let mode = drag_to_edge(&drag_window, &state);
        state.dragging.store(false, Ordering::Relaxed);
        let _ = tx.try_send(mode);
    }).map_err(|err| err.to_string())?;
    Ok(rx.recv().await.flatten())
}

/* 前台是不是全屏应用。
   屏幕上边缘在被全屏播放器/游戏/PPT 放映占用时，鼠标划过顶边多半是在碰
   播放控制条或进度条，这时不该弹抽屉。热键不受影响。 */
#[tauri::command]
fn fullscreen_app() -> bool {
    unsafe {
        let mut state: i32 = 1; /* QUNS_NOT_PRESENT */
        let hr = SHQueryUserNotificationState(&mut state);
        if hr < 0 {
            return false;
        }
        state == QUNS_BUSY || state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE
    }
}

/* Windows 不让后台进程随手抢前台：直接 SetForegroundWindow 常常静默失败，
   表现就是"面板弹出来了，但打字没反应"。挂到当前前台线程上再抢，才有焦点 */
fn force_foreground(hwnd: isize) {
    if hwnd == 0 { return; }
    unsafe {
        let fg = GetForegroundWindow();
        if fg == hwnd { return; }
        let fg_thread = if fg != 0 { GetWindowThreadProcessId(fg, std::ptr::null_mut()) } else { 0 };
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me {
            AttachThreadInput(fg_thread, me, 1);
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
            AttachThreadInput(fg_thread, me, 0);
        } else {
            BringWindowToTop(hwnd);
            SetForegroundWindow(hwnd);
        }
    }
}

/* 唤出：记住前台窗口，显示并聚焦 */
fn summon(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("main") else { return };
    let h = hwnd_of(&w);
    remember_foreground(app, h);
    unsafe {
        ShowWindow(h, 5 /* SW_SHOW */);
        SetWindowPos(h, 0, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    force_foreground(h);
    let _ = w.set_focus();
    let _ = w.emit("shell://summon", ());
}

/* 把"此刻挡在前面的是谁"记下来，等面板收起时还给它 */
fn remember_foreground(app: &tauri::AppHandle, me: isize) {
    let st = app.state::<ShellState>();
    let fg = unsafe { GetForegroundWindow() };
    if fg != 0 && fg != me {
        *st.prev_fg.lock().unwrap() = fg;
    }
}

/* 快捷键只做一件事：让面板在"开"和"收"之间翻一下。
   窗口不可见就先叫出来（此刻它一定没有焦点，不判断 focus，
   is_focused 在某些时序下是旧值，会把"收起"错判成"唤出"） */
fn on_hotkey(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("main") else { return };
    let h = hwnd_of(&w);
    if !is_shown(h) {
        summon(app);
        return;
    }
    /* 抢前台之前先记下谁在前面，收起时还回去。这一步不能只放在 summon 里：
       窗口本来就可见时（托盘点一下、热键开合）从不走 summon，
       prev_fg 会一直是 0，收起后就还不回去 */
    remember_foreground(app, h);
    unsafe {
        ShowWindow(h, 5);
        SetWindowPos(h, 0, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
    }
    force_foreground(h);
    let _ = w.set_focus();
    let _ = w.emit("shell://toggle", ());
}

fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let toggle = MenuItem::with_id(app, "toggle", "显示 / 隐藏", true, None::<&str>)?;
    let auto = CheckMenuItem::with_id(
        app,
        "autostart",
        "开机自启",
        true,
        app.autolaunch().is_enabled().unwrap_or(false),
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "退出 Desklet", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &auto, &quit])?;

    let auto2 = auto.clone();
    TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("Desklet · 话术抽屉")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, ev| match ev.id.as_ref() {
            "toggle" => on_hotkey(app),
            "autostart" => {
                let want = !auto2.is_checked().unwrap_or(false);
                let al = app.autolaunch();
                let _ = if want { al.enable() } else { al.disable() };
                let now = al.is_enabled().unwrap_or(false);
                let _ = auto2.set_checked(now);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                on_hotkey(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn default_shortcut() -> Shortcut {
    Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space)
}

#[tauri::command]
async fn get_shortcut(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let state = app.state::<ShellState>();
    let current = *state.summon_shortcut.lock().map_err(|_| "无法读取快捷键")?;
    Ok(current.map(Shortcut::into_string))
}

#[tauri::command]
async fn set_shortcut(app: tauri::AppHandle, shortcut: String) -> Result<String, String> {
    let next: Shortcut = shortcut.parse().map_err(|_| "不支持这组快捷键")?;
    if !next.mods.intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER)
        || (next.mods == Modifiers::CONTROL && next.key == Code::Space)
    {
        return Err("请使用 Ctrl、Alt 或 Win 与其他键组合，Ctrl + 空格留给输入法。".into());
    }

    let state = app.state::<ShellState>();
    let mut current = state.summon_shortcut.lock().map_err(|_| "无法更新快捷键")?;
    if *current == Some(next) { return Ok(next.into_string()); }

    let shortcuts = app.global_shortcut();
    shortcuts.register(next).map_err(|_| "这组快捷键可能已被系统或其他程序占用，请换一组。")?;
    if let Some(previous) = *current {
        if shortcuts.unregister(previous).is_err() {
            let _ = shortcuts.unregister(next);
            return Err("无法替换原快捷键，请重试。".into());
        }
    }
    *current = Some(next);
    Ok(next.into_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let gs = tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, _sc, event| {
            if event.state() == ShortcutState::Pressed {
                on_hotkey(app);
            }
        })
        .build();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            summon(app);
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .plugin(gs)
        .manage(ShellState::default())
        .invoke_handler(tauri::generate_handler![
            work_area_cmd,
            apply_geo,
            resize_origin,
            resize_cursor,
            get_float_pos,
            set_float_pos,
            set_topmost,
            reveal,
            restore_focus,
            hide_window,
            quit_app,
            launch,
            read_file_b64,
            file_icon,
            local_shortcut,
            shortcuts::clipboard_paths,
            shortcuts::setup_shortcut_drop,
            get_autostart,
            set_autostart,
            start_drag,
            fullscreen_app,
            backup::save_backup,
            get_shortcut,
            set_shortcut
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            build_tray(&handle)?;
            let shortcut = default_shortcut();
            match handle.global_shortcut().register(shortcut) {
                Ok(()) => *handle.state::<ShellState>().summon_shortcut.lock().unwrap() = Some(shortcut),
                Err(err) => eprintln!("[desklet] 全局快捷键注册失败: {err}"),
            }
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_always_on_top(true);
                let _ = w.set_skip_taskbar(true);
            }
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                /* 关窗 = 收进托盘，别真的退出 */
                api.prevent_close();
                hide_native(window.hwnd().map(|h| h.0 as isize).unwrap_or(0));
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                let Some(win) = window.app_handle().get_webview_window("main") else { return };
                let state = win.state::<ShellState>();
                if !state.dragging.load(Ordering::Relaxed) {
                    let (mode, w, h) = state.last_geo.lock().unwrap().clone();
                    if w > 0.0 { apply_geo_inner(&win, &state, &mode, w, h); }
                }
            }
            WindowEvent::Focused(false) => {
                /* 我们失焦的那一刻，前台就是用户切过去的程序，记下来 */
                let st = window.app_handle().state::<ShellState>();
                let me = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
                let fg = unsafe { GetForegroundWindow() };
                if fg != 0 && fg != me {
                    *st.prev_fg.lock().unwrap() = fg;
                }
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running desklet");
}
