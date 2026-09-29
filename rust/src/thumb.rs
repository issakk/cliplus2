//! PNG 解码与缩略图缩放。
//!
//! 纯函数,不碰任何 UI 类型:egui 弹窗的后台线程在这里解码,主线程只收
//! RGBA 结果。单测用 png crate 自己编一张图再解回来,CI 上就能跑。

use crate::log;

/// 解码 PNG 为 RGBA8,尺寸为原始尺寸。
///
/// EXPAND 把调色板/灰度位深拉平,STRIP_16 砍掉 16-bit 通道——两类变换之后,
/// 剩下的输出只可能是 8-bit 的 Rgba/Rgb/Luma,逐一展开。
pub fn decode_png_rgba(bytes: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let width = info.width as usize;
    let height = info.height as usize;

    let (color, depth) = reader.output_color_type();
    let rgba = match (color, depth) {
        (png::ColorType::Rgba, png::BitDepth::Eight) => {
            buf.truncate(width * height * 4);
            buf
        }
        (png::ColorType::Rgb, png::BitDepth::Eight) => rgb_to_rgba(&buf, width, height),
        (png::ColorType::Grayscale, png::BitDepth::Eight) => gray_to_rgba(&buf),
        _ => {
            log::warn("thumbnail: unexpected PNG color type after transforms");
            return None;
        }
    };

    Some((width, height, rgba))
}

fn rgb_to_rgba(src: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 4);
    for px in src.chunks_exact(3) {
        out.extend_from_slice(px);
        out.push(255);
    }
    out
}

fn gray_to_rgba(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() * 4);
    for &g in src {
        out.extend_from_slice(&[g, g, g, 255]);
    }
    out
}

/// 盒采样均值缩放:长边收到 `target`,纵横比不变,只缩不放。
/// 返回 `(宽, 高, RGBA)`,与输入同格式。
pub fn downscale_rgba(src: &[u8], sw: usize, sh: usize, target: usize) -> (usize, usize, Vec<u8>) {
    debug_assert_eq!(src.len(), sw * sh * 4, "RGBA input expected");
    if sw == 0 || sh == 0 || src.len() != sw * sh * 4 {
        return (sw, sh, Vec::new());
    }

    let target = target.max(1);
    let (dw, dh) = if sw >= sh {
        (target, (target as f64 * sh as f64 / sw as f64).round() as usize)
    } else {
        ((target as f64 * sw as f64 / sh as f64).round() as usize, target)
    };
    let (dw, dh) = (dw.clamp(1, sw), dh.clamp(1, sh));
    if dw == sw && dh == sh {
        return (sw, sh, src.to_vec());
    }

    let mut out = vec![0u8; dw * dh * 4];
    for dy in 0..dh {
        let y0 = dy * sh / dh;
        let y1 = ((dy + 1) * sh / dh).max(y0 + 1);
        for dx in 0..dw {
            let x0 = dx * sw / dw;
            let x1 = ((dx + 1) * sw / dw).max(x0 + 1);

            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                let row = y * sw * 4;
                for x in x0..x1 {
                    let px = row + x * 4;
                    r += u32::from(src[px]);
                    g += u32::from(src[px + 1]);
                    b += u32::from(src[px + 2]);
                    a += u32::from(src[px + 3]);
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            let o = (dy * dw + dx) * 4;
            out[o] = (r / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (b / n) as u8;
            out[o + 3] = (a / n) as u8;
        }
    }

    (dw, dh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip through the png crate's own encoder, so the decoder is
    /// exercised against a real file rather than a hand-built byte soup.
    fn encode_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(pixels).expect("png data");
        writer.finish().expect("png finish");
        out.into_inner()
    }

    #[test]
    fn png_round_trip() {
        // 2×2: red, green, blue, opaque white.
        let pixels = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255];
        let file = encode_png(2, 2, &pixels);

        let (w, h, rgba) = decode_png_rgba(&file).expect("decode");
        assert_eq!((w, h), (2, 2));
        assert_eq!(rgba, pixels.to_vec());
    }

    #[test]
    fn downscale_averages_the_box() {
        // 4×4, each channel ramps 0..255 by 16 per step so every 2×2 box has an
        // exact mean.
        let mut src = Vec::with_capacity(4 * 4 * 4);
        for y in 0..4u8 {
            for x in 0..4u8 {
                let v = x * 16 + y * 16;
                src.extend_from_slice(&[v, v, v, 255]);
            }
        }

        let (w, h, out) = downscale_rgba(&src, 4, 4, 2);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out.len(), 2 * 2 * 4);

        // Every pixel is grey (same value per channel), so the mean of the
        // three channels is the box mean of that one channel.
        let mean = |px: usize| {
            let o = px * 4;
            (u32::from(out[o]) + u32::from(out[o + 1]) + u32::from(out[o + 2])) / 3
        };
        assert_eq!(mean(0), 16); // (0 + 16 + 16 + 32) / 4
        assert_eq!(mean(1), 64); // (48 + 64 + 64 + 80) / 4
        assert_eq!(mean(2), 112); // (96 + 112 + 112 + 128) / 4
        assert_eq!(mean(3), 160); // (144 + 160 + 160 + 176) / 4
        // Alpha stays opaque everywhere.
        assert!(out.chunks_exact(4).all(|px| px[3] == 255));
    }

    #[test]
    fn downscale_never_upscales() {
        let src = vec![1u8; 2 * 2 * 4];
        let (w, h, out) = downscale_rgba(&src, 2, 2, 64);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }
}
