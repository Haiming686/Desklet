use std::{cell::RefCell, ptr, slice};

use serde::Serialize;
use tauri::{Emitter, WebviewWindow};
use windows::{
    core::{implement, w, BOOL, Ref},
    Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND, LPARAM, POINTL},
        System::{
            Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL},
            DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW, SetClipboardData},
            Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE},
            Ole::{CF_DIBV5, CF_HDROP, CF_UNICODETEXT, IDropTarget, IDropTarget_Impl, DROPEFFECT,
                DROPEFFECT_COPY, DROPEFFECT_NONE, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop},
            SystemServices::MODIFIERKEYS_FLAGS,
        },
        UI::{Shell::{DragQueryFileW, HDROP}, WindowsAndMessaging::EnumChildWindows},
    },
};

#[derive(Clone, Default, Serialize)]
struct DropPayload {
    paths: Vec<String>,
    text: Option<String>,
}

fn format(id: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn url_format(wide: bool) -> u16 {
    unsafe {
        RegisterClipboardFormatW(if wide { w!("UniformResourceLocatorW") } else { w!("UniformResourceLocator") }) as u16
    }
}

fn paths(handle: HGLOBAL) -> Vec<String> {
    let drop = HDROP(handle.0);
    unsafe {
        (0..DragQueryFileW(drop, u32::MAX, None).min(256))
            .filter_map(|index| {
                let count = DragQueryFileW(drop, index, None);
                if count == 0 { return None; }
                let mut name = vec![0; count as usize + 1];
                let copied = DragQueryFileW(drop, index, Some(&mut name)) as usize;
                (copied != 0 && copied < name.len()).then(|| String::from_utf16_lossy(&name[..copied]))
            }).collect()
    }
}

fn read_text<T: Copy>(handle: HGLOBAL, decode: impl FnOnce(&[T]) -> String) -> Option<String> {
    let size = unsafe { GlobalSize(handle) } / std::mem::size_of::<T>();
    if size == 0 || size > 512 * 1024 { return None; }
    let data = unsafe { GlobalLock(handle) }.cast::<T>();
    if data.is_null() { return None; }
    let result = decode(unsafe { slice::from_raw_parts(data, size) });
    let _ = unsafe { GlobalUnlock(handle) };
    (!result.trim().is_empty()).then_some(result)
}

fn unicode_text(handle: HGLOBAL) -> Option<String> {
    read_text(handle, |chars: &[u16]| String::from_utf16_lossy(chars.split(|&c| c == 0).next().unwrap_or_default()))
}

fn ascii_url(handle: HGLOBAL) -> Option<String> {
    read_text(handle, |bytes: &[u8]| String::from_utf8_lossy(bytes.split(|&c| c == 0).next().unwrap_or_default()).into_owned())
}

fn with_format<T>(data: &IDataObject, id: u16, read: impl FnOnce(HGLOBAL) -> T) -> Option<T> {
    if id == 0 { return None; }
    let mut medium = unsafe { data.GetData(&format(id)) }.ok()?;
    let result = (medium.tymed == TYMED_HGLOBAL.0 as u32).then(|| read(unsafe { medium.u.hGlobal }));
    unsafe { ReleaseStgMedium(&mut medium) };
    result
}

#[tauri::command]
pub fn clipboard_paths(window: WebviewWindow) -> Vec<String> {
    unsafe {
        if OpenClipboard(Some(HWND(super::hwnd_of(&window) as _))).is_err() { return Vec::new(); }
        let result = GetClipboardData(u32::from(CF_HDROP.0))
            .map(|handle| paths(HGLOBAL(handle.0))).unwrap_or_default();
        let _ = CloseClipboard();
        result
    }
}

pub(crate) fn copy_file(window: &WebviewWindow, path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    // DROPFILES has five 32-bit fields. The path list is UTF-16 and double-NUL terminated.
    let mut drop = vec![0; 20];
    drop[0] = 20;
    drop[16] = 1;
    for unit in path.as_os_str().encode_wide().chain([0, 0]) {
        drop.extend(unit.to_le_bytes());
    }
    with_clipboard(window, || unsafe {
        let effect = RegisterClipboardFormatW(w!("Preferred DropEffect"));
        if effect != 0 {
            write_clipboard(effect, &DROPEFFECT_COPY.0.to_le_bytes())?;
        }
        write_clipboard(u32::from(CF_HDROP.0), &drop)
    })
}

#[tauri::command]
pub fn copy_bitmap(window: WebviewWindow, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else { return Err("图片数据无效".into()); };
    let Some(header) = bytes.get(..8) else { return Err("图片数据无效".into()); };
    let width = u32::from_le_bytes(header[..4].try_into().unwrap());
    let height = u32::from_le_bytes(header[4..].try_into().unwrap());
    let pixels = width.checked_mul(height).filter(|&n| n > 0 && n <= 24_000_000)
        .ok_or("图片尺寸过大，请使用复制文件或另存为")?;
    let size = pixels * 4;
    if bytes.len() != 8 + size as usize { return Err("图片数据无效".into()); }

    // BITMAPV5HEADER followed by top-down BGRA pixels.
    let mut bitmap = vec![0; 124];
    for (offset, field) in [
        (0, 124), (4, width), (8, (-(height as i32)) as u32),
        (12, (32 << 16) | 1), (16, 3), (20, size),
        (40, 0x00ff0000), (44, 0x0000ff00), (48, 0x000000ff),
        (52, 0xff000000), (56, 0x73524742), (108, 4),
    ] {
        bitmap[offset..offset+4].copy_from_slice(&field.to_le_bytes());
    }
    bitmap.reserve_exact(size as usize);
    for pixel in bytes[8..].chunks_exact(4) {
        bitmap.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    with_clipboard(&window, || unsafe { write_clipboard(u32::from(CF_DIBV5.0), &bitmap) })
}

fn with_clipboard(window: &WebviewWindow, write: impl FnOnce() -> windows::core::Result<()>) -> Result<(), String> {
    unsafe {
        OpenClipboard(Some(HWND(super::hwnd_of(window) as _)))
            .map_err(|_| "剪贴板正在使用，请重试")?;
        let result = EmptyClipboard().and_then(|()| write());
        let _ = CloseClipboard();
        result.map_err(|_| "无法写入剪贴板".into())
    }
}

unsafe fn write_clipboard(format: u32, bytes: &[u8]) -> windows::core::Result<()> {
    let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let target = GlobalLock(memory).cast::<u8>();
    if target.is_null() {
        let error = windows::core::Error::from_thread();
        let _ = GlobalFree(Some(memory));
        return Err(error);
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), target, bytes.len());
    let _ = GlobalUnlock(memory);
    if let Err(error) = SetClipboardData(format, Some(HANDLE(memory.0))) {
        let _ = GlobalFree(Some(memory));
        return Err(error);
    }
    Ok(())
}

fn read_drop(data: &IDataObject) -> Option<DropPayload> {
    let paths = with_format(data, CF_HDROP.0, paths).unwrap_or_default();
    let text = with_format(data, url_format(true), unicode_text).flatten()
        .or_else(|| with_format(data, url_format(false), ascii_url).flatten())
        .or_else(|| with_format(data, CF_UNICODETEXT.0, unicode_text).flatten());
    (!paths.is_empty() || text.is_some()).then_some(DropPayload { paths, text })
}

#[implement(IDropTarget)]
struct ShortcutDrop {
    window: WebviewWindow,
    payload: RefCell<Option<DropPayload>>,
}

impl IDropTarget_Impl for ShortcutDrop_Impl {
    fn DragEnter(&self, data: Ref<'_, IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let payload = data.as_ref().and_then(read_drop);
        let accepts = payload.is_some();
        self.payload.replace(payload);
        unsafe { *effect = if accepts { DROPEFFECT_COPY } else { DROPEFFECT_NONE }; }
        if accepts { let _ = self.window.emit("shell://drop-hover", true); }
        Ok(())
    }

    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        unsafe { *effect = if self.payload.borrow().is_some() { DROPEFFECT_COPY } else { DROPEFFECT_NONE }; }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if self.payload.take().is_some() { let _ = self.window.emit("shell://drop-hover", false); }
        Ok(())
    }

    fn Drop(&self, data: Ref<'_, IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let payload = self.payload.take().or_else(|| data.as_ref().and_then(read_drop));
        let _ = self.window.emit("shell://drop-hover", false);
        unsafe { *effect = if payload.is_some() { DROPEFFECT_COPY } else { DROPEFFECT_NONE }; }
        if let Some(payload) = payload { let _ = self.window.emit("shell://drop-data", payload); }
        Ok(())
    }
}

#[tauri::command]
pub async fn setup_shortcut_drop(window: WebviewWindow) -> Result<usize, String> {
    let target = window.clone();
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    window.with_webview(move |webview| {
        let root = HWND(super::hwnd_of(&target) as _);
        let mut container = HWND::default();
        let _ = unsafe { webview.controller().ParentWindow(&mut container) };
        let count = install_drop_target(&target, root, container);
        let _ = tx.try_send(count);
    }).map_err(|err| err.to_string())?;
    rx.recv().await.ok_or_else(|| "无法注册 Windows 拖放".into())
}

fn install_drop_target(window: &WebviewWindow, root: HWND, container: HWND) -> usize {
    struct Install<'a> { window: &'a WebviewWindow, count: usize }
    unsafe extern "system" fn install(hwnd: HWND, data: LPARAM) -> BOOL {
        let info = unsafe { &mut *(data.0 as *mut Install<'_>) };
        let target: IDropTarget = ShortcutDrop { window: info.window.clone(), payload: RefCell::new(None) }.into();
        let _ = unsafe { RevokeDragDrop(hwnd) };
        if unsafe { RegisterDragDrop(hwnd, &target) }.is_ok() { info.count += 1; }
        true.into()
    }
    let mut info = Install { window, count: 0 };
    let param = LPARAM(&mut info as *mut _ as isize);
    unsafe {
        let _ = install(root, param);
        if !container.is_invalid() && container != root {
            let _ = install(container, param);
            let _ = EnumChildWindows(Some(container), Some(install), param);
        } else {
            let _ = EnumChildWindows(Some(root), Some(install), param);
        }
    }
    info.count
}
