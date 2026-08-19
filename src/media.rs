//! Everything that touches pixels: sniffing what arrived, shrinking it to a
//! size worth putting on a LAN, and turning it into something a terminal can
//! draw.
//!
//! Kept apart from the store and the TUI on purpose. The decoders here are the
//! only place in the program that parses attacker-controlled binary, since a
//! blob is whatever a peer chose to send.

use crate::store::ImageKind;
use anyhow::{anyhow, bail, Result};
use image::{DynamicImage, GenericImageView};

/// Longest side we keep. A 4K screenshot says nothing more than a 1920 one at
/// the size a terminal draws it, and costs six times the bytes.
const MAX_DIM: u32 = 1920;
/// Ceiling for one picture after re-encoding. Matches what the sync is willing
/// to move in a single round.
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Refuse absurd dimensions before allocating for them. A 30000x30000 PNG is
/// a handful of kilobytes on the wire and gigabytes once decoded.
const MAX_PIXELS: u64 = 50_000_000;

/// A picture ready to be filed and announced.
pub struct Prepared {
    pub bytes: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub kind: ImageKind,
}

/// Checks the header before decoding. `image` would work this out itself, but
/// doing it up front means an oversized picture is refused without ever being
/// expanded into memory.
fn dimensions_of(bytes: &[u8]) -> Result<(u32, u32)> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| anyhow!("unreadable picture: {e}"))?;
    reader
        .into_dimensions()
        .map_err(|e| anyhow!("unreadable picture: {e}"))
}

/// Takes raw bytes from the clipboard or a file and returns what should go on
/// the wire.
///
/// A picture already within both limits is passed through **untouched**. That
/// matters most for GIFs: re-encoding one would flatten it to a single frame,
/// which is precisely what the sender did not want.
pub fn prepare(raw: &[u8]) -> Result<Prepared> {
    let kind = ImageKind::sniff(raw).ok_or_else(|| anyhow!("not a png, jpeg or gif"))?;
    let (w, h) = dimensions_of(raw)?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        bail!("picture is {w}x{h}, too large to decode");
    }

    if raw.len() <= MAX_BYTES && w <= MAX_DIM && h <= MAX_DIM {
        return Ok(Prepared {
            bytes: raw.to_vec(),
            w,
            h,
            kind,
        });
    }

    // An oversized GIF is refused rather than converted. Shrinking it means
    // dropping the animation, and a still frame is not what was sent.
    if kind == ImageKind::Gif {
        bail!(
            "gif is {} KB, over the {} KB limit -- shrinking it would drop the animation",
            raw.len() / 1024,
            MAX_BYTES / 1024
        );
    }

    let decoded = image::load_from_memory(raw).map_err(|e| anyhow!("unreadable picture: {e}"))?;
    let resized = if w > MAX_DIM || h > MAX_DIM {
        decoded.thumbnail(MAX_DIM, MAX_DIM)
    } else {
        decoded
    };
    let (w, h) = resized.dimensions();

    // PNG first: a screenshot of text stays sharp and usually compresses well.
    // Only if that is still too big does it become a JPEG, which is small but
    // smears exactly the kind of thin text people screenshot.
    let as_png = encode(&resized, image::ImageFormat::Png)?;
    if as_png.len() <= MAX_BYTES {
        return Ok(Prepared {
            bytes: as_png,
            w,
            h,
            kind: ImageKind::Png,
        });
    }
    let as_jpeg = encode(&resized, image::ImageFormat::Jpeg)?;
    if as_jpeg.len() > MAX_BYTES {
        bail!(
            "picture is still {} KB after shrinking, over the {} KB limit",
            as_jpeg.len() / 1024,
            MAX_BYTES / 1024
        );
    }
    Ok(Prepared {
        bytes: as_jpeg,
        w,
        h,
        kind: ImageKind::Jpeg,
    })
}

fn encode(img: &DynamicImage, format: image::ImageFormat) -> Result<Vec<u8>> {
    let mut out = std::io::Cursor::new(Vec::new());
    // JPEG has no alpha; without this a screenshot with a transparent corner
    // fails to encode instead of quietly losing the corner.
    let img = if format == image::ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(img.to_rgb8())
    } else {
        img.clone()
    };
    img.write_to(&mut out, format)
        .map_err(|e| anyhow!("could not re-encode: {e}"))?;
    Ok(out.into_inner())
}

/// One frame of something drawable, with how long it should stay up.
pub struct Frame {
    pub image: DynamicImage,
    pub delay_ms: u32,
}

/// Frames of an animation, or the single frame of a still picture.
///
/// Bounded on purpose: a GIF is a list of full-size images, and a long one
/// decodes into far more memory than its file size suggests.
pub fn frames(bytes: &[u8], kind: ImageKind, max_frames: usize) -> Result<Vec<Frame>> {
    if kind != ImageKind::Gif {
        let image =
            image::load_from_memory(bytes).map_err(|e| anyhow!("unreadable picture: {e}"))?;
        return Ok(vec![Frame { image, delay_ms: 0 }]);
    }

    use image::codecs::gif::GifDecoder;
    use image::AnimationDecoder;
    let decoder =
        GifDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| anyhow!("unreadable gif: {e}"))?;
    let mut out = Vec::new();
    for frame in decoder.into_frames().take(max_frames) {
        let frame = frame.map_err(|e| anyhow!("broken gif frame: {e}"))?;
        let (num, den) = frame.delay().numer_denom_ms();
        // Browsers treat 0 and 10 ms the same way, as "about a tenth of a
        // second"; without a floor a 0 ms gif spins the redraw loop flat out.
        let delay_ms = num.checked_div(den).map_or(100, |ms| ms.max(20));
        out.push(Frame {
            image: DynamicImage::ImageRgba8(frame.into_buffer()),
            delay_ms,
        });
    }
    if out.is_empty() {
        bail!("gif has no frames");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        // Noise, so the encoder cannot collapse it to nothing and the size
        // limits actually get exercised.
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x * y) % 239) as u8, 255]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn a_small_picture_is_passed_through_untouched() {
        let raw = png_of(64, 48);
        let out = prepare(&raw).unwrap();
        assert_eq!(out.bytes, raw, "no reason to re-encode what already fits");
        assert_eq!((out.w, out.h), (64, 48));
        assert_eq!(out.kind, ImageKind::Png);
    }

    #[test]
    fn an_oversized_picture_is_shrunk_within_the_limits() {
        let raw = png_of(4000, 2200);
        let out = prepare(&raw).unwrap();
        assert!(out.w <= MAX_DIM && out.h <= MAX_DIM, "{}x{}", out.w, out.h);
        assert!(out.bytes.len() <= MAX_BYTES);
        // Aspect ratio survives: a squashed screenshot is unreadable.
        let ratio = out.w as f32 / out.h as f32;
        assert!((ratio - 4000.0 / 2200.0).abs() < 0.05, "ratio {ratio}");
    }

    #[test]
    fn what_is_not_a_picture_is_refused_by_its_header() {
        assert!(prepare(b"nao sou uma imagem, sou um texto").is_err());
        assert!(prepare(&[]).is_err());
        // A PNG header with nothing behind it must not get past the decoder.
        assert!(prepare(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).is_err());
    }

    #[test]
    fn a_still_picture_reads_as_a_single_frame() {
        let raw = png_of(32, 32);
        let frames = frames(&raw, ImageKind::Png, 64).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].delay_ms, 0, "a still picture never advances");
    }
}
