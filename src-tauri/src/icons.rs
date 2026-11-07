/* 取文件 / 文件夹 / 快捷方式的系统图标，编码成 PNG dataURL 交给前端。
 *
 * 为什么放在 Rust 侧：WebView 里既没有真实路径（拖拽只给文件名），
 * 也拿不到 shell 关联图标。SHGetFileInfoW 是唯一直给的那条路。
 *
 * 纯 FFI，不引 windows crate：要的只是 HICON → 位图 → PNG 这一小段。
 */

use crate::base64_encode;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};

#[repr(C)]
struct ShFileInfoW {
    h_icon: isize,
    i_icon: i32,
    attrs: u32,
    display: [u16; 260],
    type_name: [u16; 80],
}

#[repr(C)]
struct IconInfo {
    f_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    hbm_mask: isize,
    hbm_color: isize,
}

#[repr(C)]
struct Bitmap {
    kind: i32,
    width: i32,
    height: i32,
    width_bytes: i32,
    planes: u16,
    bits_pixel: u16,
    bits: *mut u8,
}

#[repr(C)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels: i32,
    y_pels: i32,
    clr_used: u32,
    clr_important: u32,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    /* 1bpp 的掩码位图要两色表，32bpp 用不到，但结构得留够 */
    colors: [u32; 3],
}

#[link(name = "shell32")]
extern "system" {
    fn SHGetFileInfoW(
        path: *const u16,
        attrs: u32,
        psfi: *mut ShFileInfoW,
        cb_file_info: u32,
        flags: u32,
    ) -> usize;
}

#[link(name = "user32")]
extern "system" {
    fn GetIconInfo(hicon: isize, pinfo: *mut IconInfo) -> i32;
    fn DestroyIcon(hicon: isize) -> i32;
    fn GetDC(hwnd: isize) -> isize;
    fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn GetObjectW(h: isize, c: i32, pv: *mut c_void) -> i32;
    fn GetDIBits(
        hdc: isize,
        hbm: isize,
        start: u32,
        lines: u32,
        bits: *mut c_void,
        bi: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
    fn DeleteObject(h: isize) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn SearchPathW(
        path: *const u16,
        file: *const u16,
        ext: *const u16,
        buflen: u32,
        buf: *mut u16,
        file_part: *mut *mut u16,
    ) -> u32;
}

const SHGFI_ICON: u32 = 0x0000_0100;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/* 卡片里存的可能只是个文件名（calc.exe / wt.exe）。
   这类目标 SHGetFileInfo 直接吃会失败，先丢给 SearchPathW 在
   System32 和 PATH 里找一遍——wt.exe 在 WindowsApps 别名目录里，正好在 PATH 上。 */
fn resolve(path: &str) -> String {
    if path.contains('\\') || path.contains('/') {
        return path.to_string();
    }
    let name = wide(path);
    let ext = if path.contains('.') { None } else { Some(wide(".exe")) };
    let mut buf = vec![0u16; 1024];
    let n = unsafe {
        SearchPathW(
            std::ptr::null(),
            name.as_ptr(),
            ext.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null()),
            buf.len() as u32,
            buf.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if n > 0 && (n as usize) < buf.len() {
        buf.truncate(n as usize);
        String::from_utf16_lossy(&buf)
    } else {
        path.to_string()
    }
}

pub fn data_url(path: &str) -> Result<String, String> {
    let abs = resolve(path.trim().trim_matches('"'));
    if !std::path::Path::new(&abs).exists() { return Err("目标不存在".into()); }
    let png = unsafe { grab(&abs) }?;
    Ok(format!("data:image/png;base64,{}", base64_encode(&png)))
}

unsafe fn grab(path: &str) -> Result<Vec<u8>, String> {
    let w = wide(path);
    let mut shfi: ShFileInfoW = zeroed();
    let ok = SHGetFileInfoW(
        w.as_ptr(),
        0,
        &mut shfi,
        size_of::<ShFileInfoW>() as u32,
        SHGFI_ICON,
    );
    if ok == 0 || shfi.h_icon == 0 {
        return Err("取不到图标".into());
    }
    let hicon = shfi.h_icon;

    let mut ii: IconInfo = zeroed();
    if GetIconInfo(hicon, &mut ii) == 0 {
        DestroyIcon(hicon);
        return Err("图标解析失败".into());
    }

    /* 逐段收尾：任何一条失败都要把已经拿到的句柄还回去 */
    let result = (|| -> Result<Vec<u8>, String> {
        if ii.hbm_color == 0 {
            return Err("单色图标".into());
        }
        let mut bm: Bitmap = zeroed();
        if GetObjectW(
            ii.hbm_color,
            size_of::<Bitmap>() as i32,
            &mut bm as *mut Bitmap as *mut c_void,
        ) == 0
        {
            return Err("取位图失败".into());
        }
        let (bw, bh) = (bm.width, bm.height);
        if bw <= 0 || bh <= 0 || bw > 512 || bh > 512 {
            return Err("图标尺寸异常".into());
        }

        let hdc = GetDC(0);
        if hdc == 0 {
            return Err("拿不到设备上下文".into());
        }

        let mut buf = vec![0u8; bw as usize * bh as usize * 4];
        let mut bi: BitmapInfo = zeroed();
        bi.header.size = size_of::<BitmapInfoHeader>() as u32;
        bi.header.width = bw;
        bi.header.height = -bh; /* 负值 = 自上而下，省一次翻转 */
        bi.header.planes = 1;
        bi.header.bit_count = 32;
        bi.header.compression = 0; /* BI_RGB */
        bi.header.size_image = buf.len() as u32;

        let lines = GetDIBits(
            hdc,
            ii.hbm_color,
            0,
            bh as u32,
            buf.as_mut_ptr() as *mut c_void,
            &mut bi,
            0,
        );
        if lines == 0 {
            ReleaseDC(0, hdc);
            return Err("读取像素失败".into());
        }

        /* 32bpp 图标通常自带 alpha；老的 16 色图标 alpha 全 0，
           这时只能回头查掩码位图：bit=1 是透明区。 */
        let has_alpha = buf.chunks_exact(4).any(|p| p[3] != 0);
        if !has_alpha {
            let stride = (((bw + 31) / 32) * 4) as usize;
            let mut mask = vec![0u8; stride * bh as usize];
            let mut mbi: BitmapInfo = zeroed();
            mbi.header.size = size_of::<BitmapInfoHeader>() as u32;
            mbi.header.width = bw;
            mbi.header.height = -bh;
            mbi.header.planes = 1;
            mbi.header.bit_count = 1;
            mbi.header.size_image = mask.len() as u32;
            mbi.colors[0] = 0x0000_0000;
            mbi.colors[1] = 0x00ff_ffff;
            let got = GetDIBits(
                hdc,
                ii.hbm_mask,
                0,
                bh as u32,
                mask.as_mut_ptr() as *mut c_void,
                &mut mbi,
                0,
            );
            for y in 0..bh as usize {
                for x in 0..bw as usize {
                    let a = if got != 0 {
                        let byte = mask[y * stride + x / 8];
                        if (byte >> (7 - (x % 8))) & 1 == 1 { 0 } else { 255 }
                    } else {
                        255
                    };
                    buf[(y * bw as usize + x) * 4 + 3] = a;
                }
            }
        }
        ReleaseDC(0, hdc);

        /* GDI 给的是 BGRA，PNG 要 RGBA */
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Ok(encode_png(bw as u32, bh as u32, &buf))
    })();

    if ii.hbm_color != 0 {
        DeleteObject(ii.hbm_color);
    }
    if ii.hbm_mask != 0 {
        DeleteObject(ii.hbm_mask);
    }
    DestroyIcon(hicon);
    result
}

/* ── PNG 编码 ─────────────────────────────────────────────
   只写「存储」块（不压缩）。图标就几十像素，压缩省不了多少，
   却能省掉一整条 flate 依赖链（那条链的 rustc 在本机崩过一次）。
   格式本身很固定：签名 + IHDR + IDAT + IEND，每块带 CRC32。 */

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    c ^ 0xffff_ffff
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut out = Vec::with_capacity(rgba.len() + 256);
    out.extend_from_slice(&SIG);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); /* 8bit / RGBA / deflate / 不滤波 / 不隔行 */
    chunk(&mut out, b"IHDR", &ihdr);

    /* 每行前面补一个 filter 字节（0 = None） */
    let stride = w as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * stride..(y + 1) * stride]);
    }

    /* zlib 容器：2 字节头 + 存储块（最大 65535）+ adler32 */
    let mut z = Vec::with_capacity(raw.len() + 16);
    z.extend_from_slice(&[0x78, 0x01]);
    let mut off = 0;
    while off < raw.len() {
        let n = (raw.len() - off).min(65535);
        z.push(if off + n == raw.len() { 1 } else { 0 });
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes());
        z.extend_from_slice(&raw[off..off + n]);
        off += n;
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}
