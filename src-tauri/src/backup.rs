use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use tauri::WebviewWindow;
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OPENFILENAMEW, OFN_EXPLORER, OFN_NOCHANGEDIR,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
};

fn choose_destination(owner: isize, filename: &str) -> Result<Option<PathBuf>, String> {
    let mut path = vec![0u16; 32768];
    let name: Vec<u16> = filename.encode_utf16().collect();
    path[..name.len()].copy_from_slice(&name);

    let filter: Vec<u16> = "JSON backup (*.json)\0*.json\0\0".encode_utf16().collect();
    let title: Vec<u16> = "导出 Desklet 备份\0".encode_utf16().collect();
    let extension = [b'j' as u16, b's' as u16, b'o' as u16, b'n' as u16, 0];
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner as _,
        lpstrFilter: filter.as_ptr(),
        nFilterIndex: 1,
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        lpstrTitle: title.as_ptr(),
        lpstrDefExt: extension.as_ptr(),
        Flags: OFN_EXPLORER | OFN_NOCHANGEDIR | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST,
        ..Default::default()
    };
    if unsafe { GetSaveFileNameW(&mut dialog) } == 0 {
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 { Ok(None) } else { Err(format!("无法打开保存对话框（{error}）")) };
    }
    let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
    Ok(Some(PathBuf::from(OsString::from_wide(&path[..end]))))
}

#[tauri::command]
pub async fn save_backup(window: WebviewWindow, filename: String, contents: String) -> Result<Option<String>, String> {
    if filename.is_empty() || filename.len() > 120 || filename.contains(['\\', '/', ':', '\0']) {
        return Err("备份文件名无效。".into());
    }
    let owner = super::hwnd_of(&window);
    let (tx, mut rx) = tauri::async_runtime::channel(1);
    window.run_on_main_thread(move || {
        let _ = tx.try_send(choose_destination(owner, &filename));
    }).map_err(|_| "无法打开保存对话框")?;
    let Some(path) = rx.recv().await.ok_or("保存对话框未返回")?? else {
        return Ok(None);
    };

    tauri::async_runtime::spawn_blocking(move || {
        let parent = path.parent().ok_or("保存位置无效")?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let temp = parent.join(format!(".desklet-{}-{stamp}.tmp", std::process::id()));
        let write_error = |err: std::io::Error| format!("无法写入备份：{err}");
        let mut file = OpenOptions::new().create_new(true).write(true).open(&temp).map_err(write_error)?;
        let write_result = file.write_all(contents.as_bytes()).and_then(|_| file.sync_all());
        drop(file);
        if let Err(err) = write_result.and_then(|_| fs::rename(&temp, &path)) {
            let _ = fs::remove_file(&temp);
            return Err(write_error(err));
        }
        Ok(Some(path.to_string_lossy().into_owned()))
    }).await.map_err(|_| "保存任务意外结束")?
}
