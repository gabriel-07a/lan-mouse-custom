//! Conversion between Windows device-independent bitmaps (`CF_DIB`) and the
//! canonical PNG representation.
//!
//! Windows applications expose images on the clipboard either as a registered
//! `PNG` format or as `CF_DIB`/`CF_DIBV5`. The helpers here are pure Rust so
//! they can be tested on every platform, not only on Windows.

use std::io::Cursor;

use crate::Error;

const MAX_PIXELS: usize = 16 * 1024 * 1024;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

fn checked_pixels(width: u32, height: u32) -> Result<usize, Error> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| Error::Backend("image dimensions overflow".into()))?;
    if pixels == 0 || pixels > MAX_PIXELS {
        return Err(Error::Backend(format!(
            "image dimensions are too large: {width}x{height}"
        )));
    }
    Ok(pixels)
}

/// Decode a PNG into `(width, height, rgba8)`.
pub fn png_to_rgba(png: &[u8]) -> Result<(u32, u32, Vec<u8>), Error> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|e| Error::Backend(format!("invalid png: {e}")))?;
    checked_pixels(reader.info().width, reader.info().height)?;
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| Error::Backend("png image is too large".into()))?;
    let mut buf = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| Error::Backend(format!("invalid png: {e}")))?;
    let width = info.width;
    let height = info.height;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for chunk in buf.as_chunks::<3>().0 {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xff]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for gray in buf {
                out.extend_from_slice(&[gray, gray, gray, 0xff]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for chunk in buf.as_chunks::<2>().0 {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            out
        }
        png::ColorType::Indexed => {
            return Err(Error::Backend("indexed png not expanded".into()));
        }
    };
    Ok((width, height, rgba))
}

/// Encode raw RGBA bytes as a PNG.
pub fn rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, Error> {
    let pixels = checked_pixels(width, height)?;
    let expected = pixels
        .checked_mul(4)
        .ok_or_else(|| Error::Backend("image is too large".into()))?;
    if rgba.len() != expected {
        return Err(Error::Backend("invalid rgba buffer length".into()));
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| Error::Backend(format!("png encode: {e}")))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| Error::Backend(format!("png encode: {e}")))?;
    }
    Ok(out)
}

/// Convert a `CF_DIB`/`CF_DIBV5` blob to PNG.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, Error> {
    let (width, height, rgba) = dib_to_rgba(dib)?;
    rgba_to_png(width, height, &rgba)
}

/// Convert a PNG to a 32-bit bottom-up `CF_DIB` (BI_RGB) blob.
///
/// `CF_DIB` blobs have no file header: the bytes start with the
/// `BITMAPINFOHEADER`.
pub fn png_to_dib(png: &[u8]) -> Result<Vec<u8>, Error> {
    let (width, height, rgba) = png_to_rgba(png)?;
    rgba_to_dib(width, height, &rgba)
}

/// Convert RGBA pixels to a bottom-up 32-bit `CF_DIB`.
pub fn rgba_to_dib(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, Error> {
    let pixels = checked_pixels(width, height)?;
    let expected = pixels
        .checked_mul(4)
        .ok_or_else(|| Error::Backend("image is too large".into()))?;
    if rgba.len() != expected {
        return Err(Error::Backend("invalid rgba buffer length".into()));
    }

    const HEADER_SIZE: usize = 40;
    let stride = width as usize * 4;
    let image_size = stride
        .checked_mul(height as usize)
        .ok_or_else(|| Error::Backend("image is too large".into()))?;
    let mut out = Vec::with_capacity(HEADER_SIZE + image_size);

    out.extend_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bit count
    out.extend_from_slice(&BI_RGB.to_le_bytes());
    out.extend_from_slice(&(image_size as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // x pixels per meter
    out.extend_from_slice(&0i32.to_le_bytes()); // y pixels per meter
    out.extend_from_slice(&0u32.to_le_bytes()); // colors used
    out.extend_from_slice(&0u32.to_le_bytes()); // important colors

    for row in (0..height as usize).rev() {
        let start = row * stride;
        for pixel in rgba[start..start + stride].as_chunks::<4>().0 {
            out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    Ok(out)
}

fn dib_to_rgba(dib: &[u8]) -> Result<(u32, u32, Vec<u8>), Error> {
    if dib.len() < 40 {
        return Err(Error::Backend("truncated DIB header".into()));
    }
    let read_u32 = |offset: usize| -> Result<u32, Error> {
        dib.get(offset..offset + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
            .ok_or_else(|| Error::Backend("truncated DIB header".into()))
    };
    let read_i32 =
        |offset: usize| -> Result<i32, Error> { read_u32(offset).map(|value| value as i32) };
    let read_u16 = |offset: usize| -> Result<u16, Error> {
        dib.get(offset..offset + 2)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or_else(|| Error::Backend("truncated DIB header".into()))
    };

    let header_size = read_u32(0)? as usize;
    if header_size < 40 || header_size > dib.len() {
        return Err(Error::Backend("invalid DIB header size".into()));
    }
    let width = read_i32(4)?;
    let raw_height = read_i32(8)?;
    let top_down = raw_height < 0;
    let height = raw_height.unsigned_abs();
    let bit_count = read_u16(14)?;
    let compression = read_u32(16)?;
    if width <= 0 || height == 0 {
        return Err(Error::Backend("invalid DIB dimensions".into()));
    }
    checked_pixels(width as u32, height)?;
    if !matches!(compression, BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS) {
        return Err(Error::Backend(format!(
            "unsupported DIB compression {compression}"
        )));
    }

    // Colour masks are stored inside a BITMAPV4/V5 header (offsets 40..56),
    // otherwise right after a 40-byte BITMAPINFOHEADER: three DWORDs for
    // BI_BITFIELDS and four for BI_ALPHABITFIELDS (the fourth is alpha).
    let needs_masks = matches!(compression, BI_BITFIELDS | BI_ALPHABITFIELDS);
    let masks_in_header = needs_masks && header_size >= 52;
    let (red_mask, green_mask, blue_mask, alpha_mask) = if needs_masks {
        let base = if masks_in_header { 40 } else { header_size };
        // Only BI_ALPHABITFIELDS carries an alpha mask. For BI_BITFIELDS the
        // bytes after the three masks are pixel data, so reading a fourth mask
        // there would corrupt the alpha channel with arbitrary image bytes.
        let alpha = if compression == BI_ALPHABITFIELDS && (!masks_in_header || header_size >= 56) {
            read_u32(base + 12)?
        } else {
            0
        };
        (
            read_u32(base)?,
            read_u32(base + 4)?,
            read_u32(base + 8)?,
            alpha,
        )
    } else {
        (0, 0, 0, 0)
    };

    let palette_entries = if bit_count <= 8 {
        let used = read_u32(32)? as usize;
        if used == 0 { 1usize << bit_count } else { used }
    } else {
        0
    };
    let masks_size = if !needs_masks || masks_in_header {
        0
    } else if compression == BI_ALPHABITFIELDS {
        16 // red, green, blue and alpha masks
    } else {
        12 // red, green and blue masks
    };
    let pixel_offset = header_size + masks_size + palette_entries * 4;
    if pixel_offset > dib.len() {
        return Err(Error::Backend("DIB pixel data out of bounds".into()));
    }
    let pixels = &dib[pixel_offset..];
    let stride = (width as usize * bit_count as usize).div_ceil(32) * 4;
    let image_size = stride
        .checked_mul(height as usize)
        .ok_or_else(|| Error::Backend("DIB is too large".into()))?;
    if pixels.len() < image_size {
        return Err(Error::Backend("truncated DIB pixel data".into()));
    }

    let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for row in 0..height as usize {
        let source_row = if top_down {
            row
        } else {
            height as usize - 1 - row
        };
        let start = source_row * stride;
        for x in 0..width as usize {
            match bit_count {
                24 => {
                    let index = start + x * 3;
                    rgba.extend_from_slice(&[
                        pixels[index + 2],
                        pixels[index + 1],
                        pixels[index],
                        0xff,
                    ]);
                }
                32 => {
                    let index = start + x * 4;
                    let b = pixels[index];
                    let g = pixels[index + 1];
                    let r = pixels[index + 2];
                    let a = pixels[index + 3];
                    let (r, g, b, a) = if red_mask != 0 || green_mask != 0 || blue_mask != 0 {
                        let value = u32::from_le_bytes([b, g, r, a]);
                        (
                            extract(value, red_mask),
                            extract(value, green_mask),
                            extract(value, blue_mask),
                            if alpha_mask != 0 {
                                extract(value, alpha_mask)
                            } else {
                                0xff
                            },
                        )
                    } else {
                        (r, g, b, if a == 0 { 0xff } else { a })
                    };
                    rgba.extend_from_slice(&[r, g, b, a]);
                }
                16 => {
                    let index = start + x * 2;
                    let value = u16::from_le_bytes([pixels[index], pixels[index + 1]]) as u32;
                    let (r, g, b) = if red_mask != 0 {
                        (
                            extract(value, red_mask),
                            extract(value, green_mask),
                            extract(value, blue_mask),
                        )
                    } else {
                        // 5-5-5
                        (
                            ((value >> 10) & 0x1f) as u8 * 8,
                            ((value >> 5) & 0x1f) as u8 * 8,
                            (value & 0x1f) as u8 * 8,
                        )
                    };
                    rgba.extend_from_slice(&[r, g, b, 0xff]);
                }
                8 | 4 | 1 => {
                    let index = palette_index(pixels, start, x, bit_count);
                    let palette_offset = header_size + masks_size + index * 4;
                    let b = *dib
                        .get(palette_offset)
                        .ok_or_else(|| Error::Backend("DIB palette out of bounds".into()))?;
                    let g = *dib
                        .get(palette_offset + 1)
                        .ok_or_else(|| Error::Backend("DIB palette out of bounds".into()))?;
                    let r = *dib
                        .get(palette_offset + 2)
                        .ok_or_else(|| Error::Backend("DIB palette out of bounds".into()))?;
                    rgba.extend_from_slice(&[r, g, b, 0xff]);
                }
                other => {
                    return Err(Error::Backend(format!("unsupported DIB bit count {other}")));
                }
            }
        }
    }
    Ok((width as u32, height, rgba))
}

fn extract(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let raw = (value & mask) >> shift;
    ((raw as u64 * 255) / max as u64) as u8
}

fn palette_index(pixels: &[u8], start: usize, x: usize, bit_count: u16) -> usize {
    match bit_count {
        8 => pixels[start + x] as usize,
        4 => {
            let byte = pixels[start + x / 2];
            if x.is_multiple_of(2) {
                (byte >> 4) as usize
            } else {
                (byte & 0x0f) as usize
            }
        }
        1 => {
            let byte = pixels[start + x / 8];
            let shift = 7 - (x % 8);
            ((byte >> shift) & 1) as usize
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2x2, 24bpp, bottom-up DIB: red, green / blue, white.
    fn dib24() -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&2i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&16u32.to_le_bytes());
        dib.extend_from_slice(&[0; 16]);
        // bottom row first: blue, white
        dib.extend_from_slice(&[0xff, 0, 0, 0xff, 0xff, 0xff, 0, 0]);
        // top row: red, green
        dib.extend_from_slice(&[0, 0, 0xff, 0, 0xff, 0, 0, 0]);
        dib
    }

    #[test]
    fn decodes_a_bottom_up_24bpp_dib() {
        let (w, h, rgba) = dib_to_rgba(&dib24()).expect("dib");
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            rgba,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, // top: red, green
                0, 0, 255, 255, 255, 255, 255, 255 // bottom: blue, white
            ]
        );
    }

    #[test]
    fn dib_png_roundtrip_preserves_pixels() {
        let png = dib_to_png(&dib24()).expect("png");
        let (w, h, rgba) = png_to_rgba(&png).expect("decode png");
        assert_eq!((w, h), (2, 2));
        assert_eq!(rgba[..4], [255, 0, 0, 255]);
        assert_eq!(rgba[4..8], [0, 255, 0, 255]);
        assert_eq!(rgba[8..12], [0, 0, 255, 255]);
        assert_eq!(rgba[12..16], [255, 255, 255, 255]);
    }

    #[test]
    fn top_down_32bpp_with_alpha_works() {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&(-1i32).to_le_bytes()); // top-down
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&4u32.to_le_bytes());
        dib.extend_from_slice(&[0; 16]);
        dib.extend_from_slice(&[10, 20, 30, 40]); // BGRA
        let (_, _, rgba) = dib_to_rgba(&dib).expect("dib");
        assert_eq!(rgba, vec![30, 20, 10, 40]);
    }

    #[test]
    fn rgba_to_dib_roundtrips_through_png() {
        let rgba = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let png = rgba_to_png(2, 2, &rgba).expect("png");
        let dib = png_to_dib(&png).expect("dib");
        let (_, _, back) = dib_to_rgba(&dib).expect("rgba");
        assert_eq!(back, rgba);
    }

    #[test]
    fn rejects_truncated_or_invalid_dibs() {
        assert!(dib_to_rgba(&[]).is_err());
        assert!(dib_to_rgba(&[0; 39]).is_err());
        let mut dib = dib24();
        dib[0..4].copy_from_slice(&8u32.to_le_bytes()); // header too small
        assert!(dib_to_rgba(&dib).is_err());
    }

    #[test]
    fn rejects_absurd_pixel_counts() {
        assert!(rgba_to_png(100_000, 100_000, &[]).is_err());
        assert!(rgba_to_dib(100_000, 100_000, &[]).is_err());
        let mut dib = dib24();
        dib[4..8].copy_from_slice(&100_000i32.to_le_bytes());
        dib[8..12].copy_from_slice(&100_000i32.to_le_bytes());
        assert!(dib_to_rgba(&dib).is_err());
    }

    /// 32bpp BI_BITFIELDS with the three masks after a 40-byte header.
    /// The fourth DWORD after the header is pixel data, not an alpha mask.
    fn dib_bitfields_40(width: i32) -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&width.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&BI_BITFIELDS.to_le_bytes());
        dib.extend_from_slice(&4u32.to_le_bytes());
        dib.extend_from_slice(&[0; 16]);
        // red, green and blue masks follow the header
        dib.extend_from_slice(&0x00ff_0000u32.to_le_bytes());
        dib.extend_from_slice(&0x0000_ff00u32.to_le_bytes());
        dib.extend_from_slice(&0x0000_00ffu32.to_le_bytes());
        // one BGRA pixel: blue=30, green=20, red=10, alpha byte 0x40
        dib.extend_from_slice(&[30, 20, 10, 0x40]);
        dib
    }

    #[test]
    fn bitfields_40_byte_header_ignores_pixel_bytes_as_alpha_mask() {
        let (_, _, rgba) = dib_to_rgba(&dib_bitfields_40(1)).expect("dib");
        assert_eq!(rgba, vec![10, 20, 30, 0xff]);
    }

    /// 32bpp BI_ALPHABITFIELDS with four masks after a 40-byte header; the
    /// pixel data starts at offset 56 (40 + 16), not 52 (40 + 12).
    #[test]
    fn alphabitfields_40_byte_header_uses_all_four_masks() {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&BI_ALPHABITFIELDS.to_le_bytes());
        dib.extend_from_slice(&4u32.to_le_bytes());
        dib.extend_from_slice(&[0; 16]);
        dib.extend_from_slice(&0x00ff_0000u32.to_le_bytes()); // red
        dib.extend_from_slice(&0x0000_ff00u32.to_le_bytes()); // green
        dib.extend_from_slice(&0x0000_00ffu32.to_le_bytes()); // blue
        dib.extend_from_slice(&0xff00_0000u32.to_le_bytes()); // alpha
        dib.extend_from_slice(&[30, 20, 10, 0x40]);
        let (_, _, rgba) = dib_to_rgba(&dib).expect("dib");
        assert_eq!(rgba, vec![10, 20, 30, 0x40]);
    }

    #[test]
    fn rejects_invalid_rgba_lengths() {
        assert!(rgba_to_png(2, 2, &[0; 3]).is_err());
        assert!(rgba_to_dib(2, 2, &[0; 3]).is_err());
    }
}
