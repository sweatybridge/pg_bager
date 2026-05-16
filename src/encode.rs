use std::{
    io::{self, Cursor, Write},
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{
    codecs::gif::GifDecoder, imageops::FilterType, AnimationDecoder, DynamicImage, ImageDecoder,
    ImageFormat, Limits, RgbaImage,
};

use crate::{config::Config, term::Protocol};

const KITTY_CHUNK_BYTES: usize = 4096;
const DEFAULT_GIF_FRAME_DELAY_MS: i32 = 40;
const KITTY_GIF_MAX_FRAMES: usize = 256;
const KITTY_GIF_MAX_DIMENSION: u32 = 8192;
const KITTY_GIF_MAX_DECODER_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
const KITTY_GIF_MAX_TOTAL_RGBA_BYTES: u64 = 128 * 1024 * 1024;
const SIXEL_TRANSPARENT_ALPHA_THRESHOLD: u8 = 128;
static NEXT_KITTY_IMAGE_COUNTER: AtomicU32 = AtomicU32::new(0);

pub fn write(
    out: &mut dyn Write,
    protocol: Protocol,
    original: &str,
    decoded: &[u8],
    config: &Config,
) -> io::Result<()> {
    let mut buf = Vec::new();
    match protocol {
        Protocol::Kitty => kitty(&mut buf, decoded, config),
        Protocol::ITerm2 => iterm2(&mut buf, decoded, config),
        Protocol::Sixel => sixel(&mut buf, decoded, config),
        Protocol::None => {
            buf.extend_from_slice(original.as_bytes());
            Ok(())
        }
    }?;

    if buf.is_empty() {
        out.write_all(original.as_bytes())
    } else {
        out.write_all(&buf)
    }
}

fn kitty(out: &mut Vec<u8>, decoded: &[u8], config: &Config) -> io::Result<()> {
    if decoded.starts_with(crate::scan::PNG_MAGIC) {
        return kitty_png(out, decoded, config);
    }
    if decoded.starts_with(crate::scan::GIF87A_MAGIC)
        || decoded.starts_with(crate::scan::GIF89A_MAGIC)
    {
        return kitty_gif(out, decoded, config);
    }

    Ok(())
}

fn kitty_png(out: &mut Vec<u8>, decoded: &[u8], config: &Config) -> io::Result<()> {
    let payload = STANDARD.encode(decoded);
    if payload.len() <= KITTY_CHUNK_BYTES {
        write!(out, "\x1b_Gf=100,a=T")?;
        write_kitty_size(out, config)?;
        write!(out, ";{payload}\x1b\\")?;
        return Ok(());
    }

    for (index, chunk) in payload.as_bytes().chunks(KITTY_CHUNK_BYTES).enumerate() {
        let chunk = std::str::from_utf8(chunk).expect("base64 is valid utf-8");
        if index == 0 {
            write!(out, "\x1b_Gf=100,a=T,m=1")?;
            write_kitty_size(out, config)?;
            write!(out, ";{chunk}\x1b\\")?;
        } else if (index + 1) * KITTY_CHUNK_BYTES >= payload.len() {
            write!(out, "\x1b_Gm=0;{chunk}\x1b\\")?;
        } else {
            write!(out, "\x1b_Gm=1;{chunk}\x1b\\")?;
        }
    }

    Ok(())
}

fn kitty_gif(out: &mut Vec<u8>, decoded: &[u8], config: &Config) -> io::Result<()> {
    let stats = match gif_animation_stats(decoded, config) {
        Ok(stats) => stats,
        Err(_) => return Ok(()),
    };
    if stats.frame_count == 0 {
        return Ok(());
    }

    let image_id = next_kitty_image_id();
    let mut frame_index = 0usize;
    for_each_gif_frame(decoded, config, |frame| {
        if frame_index == 0 {
            kitty_rgba(
                out,
                frame.rgba.as_slice(),
                KittyRgbaMetadata {
                    action: "T",
                    image_id: Some(image_id),
                    width: frame.width,
                    height: frame.height,
                    frame_delay_ms: None,
                    config,
                },
            )?;
            write!(out, "\x1b_Ga=a,i={image_id},r=1,z={}\x1b\\", frame.delay_ms)?;
        } else {
            kitty_rgba(
                out,
                frame.rgba.as_slice(),
                KittyRgbaMetadata {
                    action: "f",
                    image_id: Some(image_id),
                    width: frame.width,
                    height: frame.height,
                    frame_delay_ms: Some(frame.delay_ms),
                    config,
                },
            )?;
        }

        frame_index += 1;
        Ok(())
    })?;

    if stats.frame_count > 1 {
        write!(out, "\x1b_Ga=a,i={image_id},s=3,v=1\x1b\\")?;
    }

    Ok(())
}

fn next_kitty_image_id() -> u32 {
    let counter = NEXT_KITTY_IMAGE_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = (nanos as u32)
        ^ ((nanos >> 32) as u32).rotate_left(7)
        ^ pid.rotate_left(13)
        ^ counter.rotate_left(23);

    (mixed & 0x3fff_ffff) | 0x4000_0000
}

struct KittyRgbaMetadata<'a> {
    action: &'static str,
    image_id: Option<u32>,
    width: u32,
    height: u32,
    frame_delay_ms: Option<i32>,
    config: &'a Config,
}

fn kitty_rgba(out: &mut Vec<u8>, rgba: &[u8], metadata: KittyRgbaMetadata<'_>) -> io::Result<()> {
    let payload = STANDARD.encode(rgba);
    for (index, chunk) in payload.as_bytes().chunks(KITTY_CHUNK_BYTES).enumerate() {
        let chunk = std::str::from_utf8(chunk).expect("base64 is valid utf-8");
        let is_first = index == 0;
        let is_last = (index + 1) * KITTY_CHUNK_BYTES >= payload.len();

        if is_first {
            write!(
                out,
                "\x1b_Ga={},f=32,s={},v={}",
                metadata.action, metadata.width, metadata.height
            )?;
            if let Some(image_id) = metadata.image_id {
                write!(out, ",i={image_id}")?;
            }
            if let Some(delay_ms) = metadata.frame_delay_ms {
                write!(out, ",z={delay_ms}")?;
            }
            write_kitty_size(out, metadata.config)?;
            if !is_last {
                write!(out, ",m=1")?;
            }
            write!(out, ";{chunk}\x1b\\")?;
        } else if is_last {
            write!(out, "\x1b_G")?;
            if metadata.action == "f" {
                write!(out, "a=f,")?;
            }
            write!(out, "m=0;{chunk}\x1b\\")?;
        } else {
            write!(out, "\x1b_G")?;
            if metadata.action == "f" {
                write!(out, "a=f,")?;
            }
            write!(out, "m=1;{chunk}\x1b\\")?;
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GifAnimationStats {
    frame_count: usize,
}

#[derive(Clone, Debug)]
struct GifFrame {
    width: u32,
    height: u32,
    delay_ms: i32,
    rgba: Vec<u8>,
}

fn gif_animation_stats(decoded: &[u8], config: &Config) -> io::Result<GifAnimationStats> {
    let mut frame_count = 0;
    for_each_gif_frame(decoded, config, |_| {
        frame_count += 1;
        Ok(())
    })?;
    Ok(GifAnimationStats { frame_count })
}

fn for_each_gif_frame(
    decoded: &[u8],
    config: &Config,
    mut on_frame: impl FnMut(GifFrame) -> io::Result<()>,
) -> io::Result<()> {
    let decoder = limited_gif_decoder(decoded, config)?;
    let mut total_rgba_bytes = 0u64;

    for (index, frame) in decoder.into_frames().enumerate() {
        if index >= KITTY_GIF_MAX_FRAMES {
            return Err(gif_limit_error("gif has too many frames"));
        }

        let frame = frame.map_err(invalid_image)?;
        let delay_ms = frame_delay_ms(&frame);
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        let rgba = buffer.into_raw();
        let frame_bytes =
            u64::try_from(rgba.len()).map_err(|_| gif_limit_error("gif frame is too large"))?;
        total_rgba_bytes = total_rgba_bytes
            .checked_add(frame_bytes)
            .ok_or_else(|| gif_limit_error("gif animation is too large"))?;
        if total_rgba_bytes > KITTY_GIF_MAX_TOTAL_RGBA_BYTES {
            return Err(gif_limit_error("gif animation is too large"));
        }

        on_frame(GifFrame {
            width,
            height,
            delay_ms,
            rgba,
        })?;
    }

    Ok(())
}

fn limited_gif_decoder<'a>(
    decoded: &'a [u8],
    config: &Config,
) -> io::Result<GifDecoder<Cursor<&'a [u8]>>> {
    let mut decoder = GifDecoder::new(Cursor::new(decoded)).map_err(invalid_image)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(KITTY_GIF_MAX_DIMENSION);
    limits.max_image_height = Some(KITTY_GIF_MAX_DIMENSION);
    limits.max_alloc = Some(KITTY_GIF_MAX_DECODER_ALLOC_BYTES);
    if let Some(width) = config.max_pixels_w {
        limits.max_image_width = Some(width.max(KITTY_GIF_MAX_DIMENSION));
    }
    if let Some(height) = config.max_pixels_h {
        limits.max_image_height = Some(height.max(KITTY_GIF_MAX_DIMENSION));
    }
    decoder.set_limits(limits).map_err(invalid_image)?;
    Ok(decoder)
}

fn frame_delay_ms(frame: &image::Frame) -> i32 {
    let (numerator, denominator) = frame.delay().numer_denom_ms();
    if denominator == 0 {
        return DEFAULT_GIF_FRAME_DELAY_MS;
    }

    let rounded = (u64::from(numerator) + u64::from(denominator / 2)) / u64::from(denominator);
    i32::try_from(rounded)
        .ok()
        .filter(|delay| *delay > 0)
        .unwrap_or(DEFAULT_GIF_FRAME_DELAY_MS)
}

fn invalid_image(error: image::ImageError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn gif_limit_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn write_kitty_size(out: &mut Vec<u8>, config: &Config) -> io::Result<()> {
    if let Some(width) = config.max_pixels_w {
        write!(out, ",w={width}")?;
    }
    if let Some(height) = config.max_pixels_h {
        write!(out, ",h={height}")?;
    }
    Ok(())
}

fn iterm2(out: &mut Vec<u8>, decoded: &[u8], config: &Config) -> io::Result<()> {
    if !crate::scan::is_supported_image(decoded) {
        return Ok(());
    }

    let payload = STANDARD.encode(decoded);
    write!(out, "\x1b]1337;File=inline=1;size={}", decoded.len())?;
    if let Some(width) = config.max_pixels_w {
        write!(out, ";width={width}px")?;
    }
    if let Some(height) = config.max_pixels_h {
        write!(out, ";height={height}px")?;
    }
    write!(out, ";preserveAspectRatio=1:{payload}\x07")?;
    Ok(())
}

fn sixel(out: &mut Vec<u8>, decoded: &[u8], config: &Config) -> io::Result<()> {
    let rgba = match rgba_image(decoded, config) {
        Ok(rgba) => rgba,
        Err(_) => return Ok(()),
    };
    if rgba.width() == 0 || rgba.height() == 0 {
        return Ok(());
    }

    let mut used_colors = [false; 256];
    for pixel in rgba.pixels() {
        if pixel.0[3] >= SIXEL_TRANSPARENT_ALPHA_THRESHOLD {
            used_colors[palette_index(pixel.0[0], pixel.0[1], pixel.0[2]) as usize] = true;
        }
    }

    write!(out, "\x1bPq\"1;1;{};{}", rgba.width(), rgba.height())?;
    for (index, used) in used_colors.iter().enumerate() {
        if *used {
            let (red, green, blue) = palette_color(index as u8);
            write!(
                out,
                "#{index};2;{};{};{}",
                percent(red),
                percent(green),
                percent(blue)
            )?;
        }
    }

    for band_y in (0..rgba.height()).step_by(6) {
        if band_y > 0 {
            out.push(b'-');
        }

        for color in 0u16..=255 {
            if !used_colors[color as usize] {
                continue;
            }

            write!(out, "#{color}")?;
            let mut last = 63u8;
            let mut run = 0usize;
            for x in 0..rgba.width() {
                let bits = sixel_bits_for_color(&rgba, x, band_y, color as u8);
                let byte = 63 + bits;
                if run == 0 {
                    last = byte;
                    run = 1;
                } else if byte == last {
                    run += 1;
                } else {
                    write_sixel_run(out, run, last)?;
                    last = byte;
                    run = 1;
                }
            }
            write_sixel_run(out, run, last)?;
            out.push(b'$');
        }

        if out.last() == Some(&b'$') {
            out.pop();
        }
    }

    write!(out, "\x1b\\")?;
    Ok(())
}

fn rgba_image(decoded: &[u8], config: &Config) -> io::Result<RgbaImage> {
    let rgba = if decoded.starts_with(crate::scan::PNG_MAGIC) {
        image::load_from_memory_with_format(decoded, ImageFormat::Png)
            .map_err(invalid_image)?
            .to_rgba8()
    } else if decoded.starts_with(crate::scan::GIF87A_MAGIC)
        || decoded.starts_with(crate::scan::GIF89A_MAGIC)
    {
        first_gif_frame(decoded, config)?
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported image format",
        ));
    };

    let (target_width, target_height) = constrained_dimensions(rgba.width(), rgba.height(), config);
    if target_width == rgba.width() && target_height == rgba.height() {
        Ok(rgba)
    } else {
        Ok(DynamicImage::ImageRgba8(rgba)
            .resize(target_width, target_height, FilterType::Lanczos3)
            .to_rgba8())
    }
}

fn first_gif_frame(decoded: &[u8], config: &Config) -> io::Result<RgbaImage> {
    let decoder = limited_gif_decoder(decoded, config)?;
    let frame = decoder
        .into_frames()
        .next()
        .ok_or_else(|| gif_limit_error("gif has no frames"))?
        .map_err(invalid_image)?;
    Ok(frame.into_buffer())
}

fn constrained_dimensions(width: u32, height: u32, config: &Config) -> (u32, u32) {
    let width_limit = config.max_pixels_w.unwrap_or(width).max(1);
    let height_limit = config.max_pixels_h.unwrap_or(height).max(1);
    let scale_w = width_limit as f64 / width.max(1) as f64;
    let scale_h = height_limit as f64 / height.max(1) as f64;
    let scale = scale_w.min(scale_h).min(1.0);

    (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    )
}

fn palette_index(red: u8, green: u8, blue: u8) -> u8 {
    let red = red >> 5;
    let green = green >> 5;
    let blue = blue >> 6;
    (red << 5) | (green << 2) | blue
}

fn palette_color(index: u8) -> (u8, u8, u8) {
    let red = (index >> 5) & 0x07;
    let green = (index >> 2) & 0x07;
    let blue = index & 0x03;
    (
        ((u16::from(red) * 255) / 7) as u8,
        ((u16::from(green) * 255) / 7) as u8,
        ((u16::from(blue) * 255) / 3) as u8,
    )
}

fn percent(value: u8) -> u8 {
    ((u16::from(value) * 100 + 127) / 255) as u8
}

fn sixel_bits_for_color(rgba: &RgbaImage, x: u32, band_y: u32, color: u8) -> u8 {
    let mut bits = 0u8;
    for offset in 0..6 {
        let y = band_y + offset;
        if y >= rgba.height() {
            continue;
        }

        let pixel = rgba.get_pixel(x, y).0;
        if pixel[3] >= SIXEL_TRANSPARENT_ALPHA_THRESHOLD
            && palette_index(pixel[0], pixel[1], pixel[2]) == color
        {
            bits |= 1 << offset;
        }
    }
    bits
}

fn write_sixel_run(out: &mut Vec<u8>, run: usize, byte: u8) -> io::Result<()> {
    if run > 3 {
        write!(out, "!{run}")?;
        out.push(byte);
        return Ok(());
    }

    for _ in 0..run {
        out.push(byte);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{write, KITTY_GIF_MAX_FRAMES};
    use crate::{config::Config, term::Protocol};
    use image::ImageEncoder;

    fn config() -> Config {
        Config {
            max_row_bytes: 1024,
            max_pixels_w: None,
            max_pixels_h: None,
            disable: false,
            fallback: None,
        }
    }

    fn animated_gif() -> Vec<u8> {
        animated_gif_frames(2)
    }

    fn png() -> Vec<u8> {
        let mut bytes = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder
            .write_image(
                &[255, 0, 0, 255, 0, 0, 255, 255],
                2,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        bytes
    }

    fn red_png(width: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        let pixels = [255, 0, 0, 255].repeat(width as usize);
        let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
        encoder
            .write_image(&pixels, width, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }

    fn animated_gif_frames(frame_count: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();

            for index in 0..frame_count {
                let color = if index % 2 == 0 {
                    image::Rgba([255, 0, 0, 255])
                } else {
                    image::Rgba([0, 0, 255, 255])
                };
                let frame = image::Frame::from_parts(
                    image::RgbaImage::from_pixel(1, 1, color),
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(10 * (index as u32 + 1), 1),
                );
                encoder.encode_frame(frame).unwrap();
            }
        }
        bytes
    }

    fn first_kitty_image_id(output: &str) -> u32 {
        let start = output.find(",i=").unwrap() + 3;
        let end = output[start..]
            .find(|byte: char| !byte.is_ascii_digit())
            .map_or(output.len(), |offset| start + offset);
        output[start..end].parse().unwrap()
    }

    #[test]
    fn kitty_single_chunk_omits_more_flag() {
        let mut out = Vec::new();
        write(
            &mut out,
            Protocol::Kitty,
            "orig",
            crate::scan::PNG_MAGIC,
            &config(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b_Gf=100,a=T;"));
        assert!(!text.contains("m=1"));
        assert!(text.ends_with("\x1b\\"));
    }

    #[test]
    fn kitty_multi_chunk_terminates_with_m_zero() {
        let mut png = crate::scan::PNG_MAGIC.to_vec();
        png.resize(5000, 1);
        let mut out = Vec::new();
        write(&mut out, Protocol::Kitty, "orig", &png, &config()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("f=100,a=T,m=1"));
        assert!(text.contains("\x1b_Gm=0;"));
    }

    #[test]
    fn kitty_writes_gif_animation_escape() {
        let gif = animated_gif();
        let mut out = Vec::new();
        write(&mut out, Protocol::Kitty, "orig", &gif, &config()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\x1b_Ga=T,f=32,s=1,v=1,i="));
        assert!(text.contains("\x1b_Ga=a,i="));
        assert!(text.contains(",r=1,z=10\x1b\\"));
        assert!(text.contains("\x1b_Ga=f,f=32,s=1,v=1,i="));
        assert!(text.contains(",z=20;"));
        assert!(text.contains(",s=3,v=1\x1b\\"));
    }

    #[test]
    fn kitty_gif_ids_do_not_restart_at_one() {
        let gif = animated_gif();
        let mut first = Vec::new();
        let mut second = Vec::new();
        write(&mut first, Protocol::Kitty, "orig", &gif, &config()).unwrap();
        write(&mut second, Protocol::Kitty, "orig", &gif, &config()).unwrap();

        let first_id = first_kitty_image_id(&String::from_utf8(first).unwrap());
        let second_id = first_kitty_image_id(&String::from_utf8(second).unwrap());
        assert_ne!(first_id, 1);
        assert_ne!(second_id, 1);
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn kitty_gif_over_frame_limit_writes_original() {
        let gif = animated_gif_frames(KITTY_GIF_MAX_FRAMES + 1);
        let mut out = Vec::new();
        write(&mut out, Protocol::Kitty, "orig", &gif, &config()).unwrap();
        assert_eq!(out, b"orig");
    }

    #[test]
    fn kitty_malformed_gif_writes_original() {
        let mut out = Vec::new();
        write(
            &mut out,
            Protocol::Kitty,
            "orig",
            crate::scan::GIF89A_MAGIC,
            &config(),
        )
        .unwrap();
        assert_eq!(out, b"orig");
    }

    #[test]
    fn iterm2_writes_gif_inline_file_escape() {
        let mut out = Vec::new();
        write(
            &mut out,
            Protocol::ITerm2,
            "orig",
            crate::scan::GIF89A_MAGIC,
            &config(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b]1337;File=inline=1;size=6;preserveAspectRatio=1:"));
        assert!(text.ends_with('\x07'));
    }

    #[test]
    fn iterm2_writes_inline_file_escape() {
        let mut out = Vec::new();
        write(
            &mut out,
            Protocol::ITerm2,
            "orig",
            crate::scan::PNG_MAGIC,
            &config(),
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1b]1337;File=inline=1;size=8;preserveAspectRatio=1:"));
        assert!(text.ends_with('\x07'));
    }

    #[test]
    fn sixel_writes_png_escape() {
        let mut out = Vec::new();
        write(&mut out, Protocol::Sixel, "orig", &png(), &config()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("\x1bPq\"1;1;2;1"));
        assert!(text.contains("#224;2;100;0;0"));
        assert!(text.contains("#3;2;0;0;100"));
        assert!(text.ends_with("\x1b\\"));
    }

    #[test]
    fn sixel_malformed_image_writes_original() {
        let mut out = Vec::new();
        write(
            &mut out,
            Protocol::Sixel,
            "orig",
            crate::scan::PNG_MAGIC,
            &config(),
        )
        .unwrap();
        assert_eq!(out, b"orig");
    }

    #[test]
    fn sixel_uses_repeat_introducer_for_long_runs() {
        let mut out = Vec::new();
        write(&mut out, Protocol::Sixel, "orig", &red_png(4), &config()).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("#224!4@"));
        assert!(!text.contains("#224!4@@@@"));
    }

    #[test]
    fn none_writes_original() {
        let mut out = Vec::new();
        write(
            &mut out,
            Protocol::None,
            "orig",
            crate::scan::PNG_MAGIC,
            &config(),
        )
        .unwrap();
        assert_eq!(out, b"orig");
    }
}
