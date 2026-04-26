use std::{
    io::{self, Cursor, Write},
    sync::atomic::{AtomicU32, Ordering},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{codecs::gif::GifDecoder, AnimationDecoder};

use crate::{config::Config, term::Protocol};

const KITTY_CHUNK_BYTES: usize = 4096;
const DEFAULT_GIF_FRAME_DELAY_MS: i32 = 40;
static NEXT_KITTY_IMAGE_ID: AtomicU32 = AtomicU32::new(1);

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
    let frames = match decode_gif_frames(decoded) {
        Ok(frames) => frames,
        Err(_) => return Ok(()),
    };
    if frames.is_empty() {
        return Ok(());
    }

    let image_id = NEXT_KITTY_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
    let first = &frames[0];
    kitty_rgba(
        out,
        first.rgba.as_slice(),
        KittyRgbaMetadata {
            action: "T",
            image_id: Some(image_id),
            width: first.width,
            height: first.height,
            frame_delay_ms: None,
            config,
        },
    )?;
    write!(out, "\x1b_Ga=a,i={image_id},r=1,z={}\x1b\\", first.delay_ms)?;

    for frame in &frames[1..] {
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

    if frames.len() > 1 {
        write!(out, "\x1b_Ga=a,i={image_id},s=3,v=1\x1b\\")?;
    }

    Ok(())
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

#[derive(Clone, Debug)]
struct GifFrame {
    width: u32,
    height: u32,
    delay_ms: i32,
    rgba: Vec<u8>,
}

fn decode_gif_frames(decoded: &[u8]) -> io::Result<Vec<GifFrame>> {
    let decoder = GifDecoder::new(Cursor::new(decoded)).map_err(invalid_image)?;
    let frames = decoder
        .into_frames()
        .collect_frames()
        .map_err(invalid_image)?;
    Ok(frames
        .into_iter()
        .map(|frame| {
            let delay_ms = frame_delay_ms(&frame);
            let buffer = frame.into_buffer();
            let (width, height) = buffer.dimensions();
            GifFrame {
                width,
                height,
                delay_ms,
                rgba: buffer.into_raw(),
            }
        })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::write;
    use crate::{config::Config, term::Protocol};

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
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();

            let first = image::Frame::from_parts(
                image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(10, 1),
            );
            let second = image::Frame::from_parts(
                image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 255, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(20, 1),
            );

            encoder.encode_frame(first).unwrap();
            encoder.encode_frame(second).unwrap();
        }
        bytes
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
