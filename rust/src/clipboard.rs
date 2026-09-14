//! Reading and writing the Windows clipboard.
//!
//! Everything here runs on the window thread, because the clipboard is owned by
//! the thread that opened it. Retries matter: any application holding the
//! clipboard open makes `OpenClipboard` fail transiently, and a single failed
//! attempt must never silently lose a copy.

use std::time::Duration;

use crate::clip::ClipPayload;
use crate::log;
use crate::win;

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_DIBV5: u32 = 17;

const OPEN_RETRIES: u32 = 6;
const RETRY_DELAY: Duration = Duration::from_millis(20);

/// Reads whatever is currently on the clipboard.
///
/// Priority: files, then text, then image. Explorer puts
/// both a file drop and a text form on the clipboard, and the file drop is the
/// more useful of the two; Excel puts both text and an image, and text is what
/// people expect to paste back.
pub fn read() -> Option<ClipPayload> {
    for attempt in 0..OPEN_RETRIES {
        if attempt > 0 {
            std::thread::sleep(RETRY_DELAY);
        }

        let opened = unsafe { win::OpenClipboard(0) } != 0;
        if !opened {
            continue;
        }

        let payload = read_locked();
        unsafe {
            win::CloseClipboard();
        }
        return payload;
    }

    log::warn("clipboard stayed busy through every retry; this copy was skipped");
    None
}

/// Caller must already hold the clipboard open.
fn read_locked() -> Option<ClipPayload> {
    if let Some(paths) = read_file_drop() {
        if !paths.is_empty() {
            return Some(ClipPayload::Files(paths));
        }
    }

    if let Some(text) = read_text() {
        if !text.is_empty() {
            return Some(ClipPayload::Text(text));
        }
    }

    if let Some(png) = read_image() {
        if !png.is_empty() {
            return Some(ClipPayload::Image(png));
        }
    }

    None
}

fn available(format: u32) -> bool {
    // Bound first: a bare `unsafe { .. } != 0` tail expression parses as a
    // statement followed by junk, not as a comparison.
    let present = unsafe { win::IsClipboardFormatAvailable(format) };
    present != 0
}

fn read_text() -> Option<String> {
    if !available(CF_UNICODETEXT) {
        return None;
    }

    let handle = unsafe { win::GetClipboardData(CF_UNICODETEXT) };
    let bytes = read_global(handle)?;

    // UTF-16 up to the first NUL. GlobalSize reports the whole allocation, which
    // is usually larger than the string.
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&unit| unit != 0)
        .collect();

    if units.is_empty() {
        return None;
    }

    Some(String::from_utf16_lossy(&units))
}

fn read_file_drop() -> Option<Vec<String>> {
    if !available(CF_HDROP) {
        return None;
    }

    let handle = unsafe { win::GetClipboardData(CF_HDROP) };
    if handle == 0 {
        return None;
    }

    // 0xFFFF_FFFF asks DragQueryFileW for the item count.
    let count = unsafe { win::DragQueryFileW(handle, 0xFFFF_FFFF, std::ptr::null_mut(), 0) };
    if count == 0 {
        return None;
    }

    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let length = unsafe { win::DragQueryFileW(handle, index, std::ptr::null_mut(), 0) };
        if length == 0 {
            continue;
        }

        let mut buffer = vec![0u16; length as usize + 1];
        let copied = unsafe {
            win::DragQueryFileW(handle, index, buffer.as_mut_ptr(), buffer.len() as u32)
        };
        if copied == 0 {
            continue;
        }

        buffer.truncate(copied as usize);
        paths.push(String::from_utf16_lossy(&buffer));
    }

    Some(paths)
}

fn read_image() -> Option<Vec<u8>> {
    // CF_DIBV5 is the richer format; fall back to CF_DIB for sources that only
    // publish the older one.
    let handle = if available(CF_DIBV5) {
        unsafe { win::GetClipboardData(CF_DIBV5) }
    } else if available(CF_DIB) {
        unsafe { win::GetClipboardData(CF_DIB) }
    } else {
        return None;
    };

    let bytes = read_global(handle)?;
    let image = match decode_dib(&bytes) {
        Some(image) => image,
        None => {
            log::warn(&format!(
                "unsupported DIB layout ({} bytes); image not captured",
                bytes.len()
            ));
            return None;
        }
    };

    match encode_png(image.width, image.height, &image.rgba) {
        Ok(png) => Some(png),
        Err(err) => {
            log::error(&format!("PNG encode failed: {err}"));
            None
        }
    }
}

fn read_global(handle: win::HGLOBAL) -> Option<Vec<u8>> {
    if handle == 0 {
        return None;
    }

    unsafe {
        let size = win::GlobalSize(handle);
        let pointer = win::GlobalLock(handle) as *const u8;
        if pointer.is_null() || size == 0 {
            win::GlobalUnlock(handle);
            return None;
        }

        let bytes = std::slice::from_raw_parts(pointer, size).to_vec();
        win::GlobalUnlock(handle);
        Some(bytes)
    }
}

// ------------------------------------------------------------------- DIB decode

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// RGBA8, top-down.
    pub rgba: Vec<u8>,
}

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let slice = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    u32_at(bytes, offset).map(|v| v as i32)
}

/// Converts a `CF_DIB` / `CF_DIBV5` blob into RGBA.
///
/// Only the two layouts that cover essentially every real clipboard image are
/// handled: 24- and 32-bit pixels, with either the default channel order or
/// explicit bit masks. Anything else returns `None` and the copy is skipped —
/// a bounded failure is much better than a half-decoded image that looks fine
/// until someone pastes it.
pub fn decode_dib(bytes: &[u8]) -> Option<DecodedImage> {
    let header_size = u32_at(bytes, 0)? as usize;
    if header_size < 40 || bytes.len() < header_size {
        return None;
    }

    let raw_width = i32_at(bytes, 4)?;
    let raw_height = i32_at(bytes, 8)?;
    let bit_count = u16_at(bytes, 14)?;
    let compression = u32_at(bytes, 16)?;

    if raw_width <= 0 || raw_height == 0 {
        return None;
    }

    let width = raw_width as u32;
    // A negative height means the rows are stored top-down.
    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs();

    // Guard against a corrupt header asking for a gigantic allocation.
    if width > 32_768 || height > 32_768 {
        return None;
    }

    let bytes_per_pixel: usize = match (bit_count, compression) {
        (32, BI_RGB) | (32, BI_BITFIELDS) => 4,
        (24, BI_RGB) | (24, BI_BITFIELDS) => 3,
        _ => return None,
    };

    // The red/green/blue masks sit at the same offset in both cases: right after
    // a 40-byte BITMAPINFOHEADER, or inside a v4/v5 header past byte 40.
    let (red_mask, green_mask, blue_mask) = if compression == BI_BITFIELDS {
        let r = u32_at(bytes, 40)?;
        let g = u32_at(bytes, 44)?;
        let b = u32_at(bytes, 48)?;
        if r == 0 || g == 0 || b == 0 {
            return None;
        }
        (r, g, b)
    } else {
        (0x00FF_0000, 0x0000_FF00, 0x0000_00FF)
    };

    let data_offset = if compression == BI_BITFIELDS && header_size == 40 {
        header_size + 12 // three DWORD masks follow the header
    } else {
        header_size
    };

    // Rows are padded out to a 4-byte boundary.
    let stride = (width as usize * bytes_per_pixel + 3) & !3;
    let needed = data_offset + stride * height as usize;
    if bytes.len() < needed {
        return None;
    }

    let red_shift = red_mask.trailing_zeros();
    let green_shift = green_mask.trailing_zeros();
    let blue_shift = blue_mask.trailing_zeros();
    let red_max = (red_mask >> red_shift).max(1);
    let green_max = (green_mask >> green_shift).max(1);
    let blue_max = (blue_mask >> blue_shift).max(1);

    let mut rgba = vec![0u8; width as usize * height as usize * 4];

    for y in 0..height as usize {
        let source_y = if top_down {
            y
        } else {
            height as usize - 1 - y
        };
        let row = data_offset + source_y * stride;
        let destination = y * width as usize * 4;

        for x in 0..width as usize {
            let pixel = row + x * bytes_per_pixel;

            let (raw, alpha) = if bytes_per_pixel == 4 {
                let value = u32_at(bytes, pixel)?;
                let alpha = (value >> 24) & 0xFF;
                // 32-bit DIBs usually leave the alpha byte zero, meaning
                // "unused" rather than "transparent".
                (value, if alpha == 0 { 255u8 } else { alpha as u8 })
            } else {
                let value = (bytes[pixel] as u32)
                    | ((bytes[pixel + 1] as u32) << 8)
                    | ((bytes[pixel + 2] as u32) << 16);
                (value, 255u8)
            };

            let out = destination + x * 4;
            rgba[out] = (((raw & red_mask) >> red_shift) * 255 / red_max) as u8;
            rgba[out + 1] = (((raw & green_mask) >> green_shift) * 255 / green_max) as u8;
            rgba[out + 2] = (((raw & blue_mask) >> blue_shift) * 255 / blue_max) as u8;
            rgba[out + 3] = alpha;
        }
    }

    Some(DecodedImage {
        width,
        height,
        rgba,
    })
}

// ------------------------------------------------------------------ writing

/// Replaces the clipboard contents with `payload`.
///
/// Every failure path frees the buffer it allocated: once `SetClipboardData`
/// succeeds the system owns the block, and only then must we keep our hands off
/// it. Getting that backwards is a memory leak on every paste.
pub fn write(payload: &ClipPayload) -> bool {
    match payload {
        ClipPayload::Text(text) => write_unicode_text(text),
        ClipPayload::Files(paths) => write_file_drop(paths),
        ClipPayload::Image(png) => write_dib(png),
    }
}

fn write_unicode_text(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    let mut bytes = Vec::with_capacity(text.len() * 2 + 2);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.extend_from_slice(&0u16.to_le_bytes());

    place(CF_UNICODETEXT, &bytes)
}

/// `CF_HDROP` is a DROPFILES header followed by a double-NUL-terminated list of
/// wide paths. Built byte by byte so there is no struct-packing assumption.
fn write_file_drop(paths: &[String]) -> bool {
    if paths.is_empty() {
        return false;
    }

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&20u32.to_le_bytes()); // pFiles: offset of the list
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.x
    bytes.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    bytes.extend_from_slice(&0i32.to_le_bytes()); // fNC
    bytes.extend_from_slice(&1i32.to_le_bytes()); // fWide: paths are UTF-16

    for path in paths {
        for unit in path.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    bytes.extend_from_slice(&0u16.to_le_bytes()); // terminating NUL

    place(CF_HDROP, &bytes)
}

/// Decodes our stored PNG back to pixels and re-publishes it as `CF_DIB`.
///
/// Only RGBA and RGB inputs are accepted. Our own blobs are always RGBA, so
/// anything else means a foreign file, and refusing is better than writing a
/// DIB whose channel order we guessed wrong.
fn write_dib(png_bytes: &[u8]) -> bool {
    let decoder = png::Decoder::new(png_bytes);
    let mut reader = match decoder.read_info() {
        Ok(reader) => reader,
        Err(err) => {
            log::error(&format!("PNG header unreadable: {err}"));
            return false;
        }
    };

    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let info = match reader.next_frame(&mut buffer) {
        Ok(info) => info,
        Err(err) => {
            log::error(&format!("PNG decode failed: {err}"));
            return false;
        }
    };

    let pixels = &buffer[..info.buffer_size()];
    let (width, height) = (info.width, info.height);
    if width == 0 || height == 0 {
        return false;
    }

    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    match info.color_type {
        png::ColorType::Rgba => {
            for pixel in pixels.chunks_exact(4) {
                bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        }
        png::ColorType::Rgb => {
            for pixel in pixels.chunks_exact(3) {
                bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            }
        }
        other => {
            log::warn(&format!("unsupported PNG colour type {other:?}"));
            return false;
        }
    }

    let header_size = 40u32;
    let image_size = (width * height * 4) as u32;
    let stride = (width * 4) as usize;

    let mut bytes = Vec::with_capacity(header_size as usize + image_size as usize);
    bytes.extend_from_slice(&header_size.to_le_bytes()); // biSize
    bytes.extend_from_slice(&(width as i32).to_le_bytes());
    bytes.extend_from_slice(&(height as i32).to_le_bytes()); // positive: bottom-up
    bytes.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    bytes.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    bytes.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    bytes.extend_from_slice(&image_size.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    bytes.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    bytes.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    bytes.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    // DIB rows run bottom-up unless the height is negative.
    for row in (0..height as usize).rev() {
        let start = row * stride;
        bytes.extend_from_slice(&bgra[start..start + stride]);
    }

    place(CF_DIB, &bytes)
}

/// Copies `bytes` into a moveable global block and hands it to the clipboard
/// under `format`.
fn place(format: u32, bytes: &[u8]) -> bool {
    let handle = global_from_bytes(bytes);
    if handle == 0 {
        return false;
    }

    let placed = with_open_clipboard(|| {
        let stored = unsafe { win::SetClipboardData(format, handle) };
        stored != 0
    });

    if !placed {
        // The system only takes ownership on success.
        unsafe {
            win::GlobalFree(handle);
        }
    }

    placed
}

fn global_from_bytes(bytes: &[u8]) -> win::HGLOBAL {
    unsafe {
        let handle = win::GlobalAlloc(win::GMEM_MOVEABLE, bytes.len().max(1));
        if handle == 0 {
            return 0;
        }

        let pointer = win::GlobalLock(handle) as *mut u8;
        if pointer.is_null() {
            win::GlobalFree(handle);
            return 0;
        }

        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
        win::GlobalUnlock(handle);
        handle
    }
}

/// Opens the clipboard, empties it and runs `action`, retrying while other
/// applications hold it open.
fn with_open_clipboard<F: FnOnce() -> bool>(action: F) -> bool {
    for attempt in 0..OPEN_RETRIES {
        if attempt > 0 {
            std::thread::sleep(RETRY_DELAY);
        }

        let opened = unsafe { win::OpenClipboard(0) };
        if opened == 0 {
            continue;
        }

        let cleared = unsafe { win::EmptyClipboard() } != 0;
        let ok = cleared && action();

        unsafe {
            win::CloseClipboard();
        }

        return ok;
    }

    log::warn("clipboard stayed busy; nothing was written");
    false
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);

        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}
