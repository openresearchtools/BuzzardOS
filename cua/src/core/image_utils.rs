//! PNG resizing, dimensions, and RGBA encoding used by Linux capture.

use anyhow::{anyhow, bail, Result};
use image::{ColorType, DynamicImage, ImageBuffer, ImageDecoder, ImageFormat};

pub fn resize_png_if_needed(png_bytes: &[u8], max_dim: u32) -> Result<Vec<u8>> {
    if max_dim == 0 {
        return Ok(png_bytes.to_vec());
    }
    let (width, height) = png_dimensions(png_bytes)?;
    if width <= max_dim && height <= max_dim {
        return Ok(png_bytes.to_vec());
    }
    let scale = max_dim as f64 / width.max(height) as f64;
    let new_width = (width as f64 * scale).round() as u32;
    let new_height = (height as f64 * scale).round() as u32;
    let decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(png_bytes))?;
    let color = decoder.color_type();
    let mut pixels = vec![0; decoder.total_bytes() as usize];
    decoder.read_image(&mut pixels)?;
    let image = match color {
        ColorType::Rgba8 => DynamicImage::ImageRgba8(
            ImageBuffer::from_raw(width, height, pixels)
                .ok_or_else(|| anyhow!("invalid RGBA buffer"))?,
        ),
        ColorType::Rgb8 => DynamicImage::ImageRgb8(
            ImageBuffer::from_raw(width, height, pixels)
                .ok_or_else(|| anyhow!("invalid RGB buffer"))?,
        ),
        _ => bail!("unsupported color type for resize: {color:?}"),
    };
    let resized = image.resize(new_width, new_height, image::imageops::FilterType::Lanczos3);
    let mut output = Vec::new();
    resized.write_to(&mut std::io::Cursor::new(&mut output), ImageFormat::Png)?;
    Ok(output)
}

pub fn png_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    const PNG_SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    if data.len() < 24 || data[..8] != PNG_SIGNATURE || &data[12..16] != b"IHDR" {
        bail!("invalid PNG header");
    }
    Ok((
        u32::from_be_bytes(data[16..20].try_into().expect("four-byte width")),
        u32::from_be_bytes(data[20..24].try_into().expect("four-byte height")),
    ))
}

pub fn encode_rgba_to_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    if rgba.len() as u64 != width as u64 * height as u64 * 4 {
        bail!(
            "RGBA buffer size {} does not match {width}x{height}",
            rgba.len()
        );
    }
    let buffer: ImageBuffer<image::Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| anyhow!("invalid RGBA buffer"))?;
    let mut output = Vec::new();
    DynamicImage::ImageRgba8(buffer)
        .write_to(&mut std::io::Cursor::new(&mut output), ImageFormat::Png)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_and_resize_round_trip() {
        let png = encode_rgba_to_png(&vec![0; 200 * 100 * 4], 200, 100).unwrap();
        assert_eq!(png_dimensions(&png).unwrap(), (200, 100));
        let resized = resize_png_if_needed(&png, 50).unwrap();
        assert_eq!(png_dimensions(&resized).unwrap(), (50, 25));
    }

    #[test]
    fn rejects_invalid_headers_and_buffer_sizes() {
        assert!(png_dimensions(b"not png").is_err());
        assert!(encode_rgba_to_png(&[0; 3], 1, 1).is_err());
    }
}
