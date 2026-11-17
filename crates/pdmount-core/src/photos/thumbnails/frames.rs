//! The frames of a WebP thumbnail, read from their headers before the
//! decoder sees the bytes (M5.8, V-5; D115 amended).
//!
//! `image-webp` reports an extended WebP's (`VP8X`) canvas as its size, but
//! decodes the `VP8 ` frame inside it, and an animation frame's, at the size
//! that frame's own header declares (up to 16383 × 16383), allocating and
//! filling its planes before it compares that size with the canvas; its
//! memory limit covers only the metadata chunks. So [`check`] walks the RIFF
//! chunks first and reads every image frame's declared size: the `VP8 ` key
//! frame header (start code `9d 01 2a`, 14-bit width and height) and the
//! `VP8L` header (14-bit width − 1 and height − 1); an `ALPH` chunk takes its
//! frame's size. The thumbnail is refused when a frame is larger than
//! [`MAX_SIDE`] or than the canvas, when all frames together have more than
//! [`MAX_TOTAL_PIXELS`], when a chunk is malformed or truncated, or when the
//! layout is not one of WebP's: a simple `VP8 `/`VP8L` file, a `VP8X` still
//! with one image (and an `ALPH` before a `VP8 `), or a `VP8X` animation of
//! `ANMF` frames, each with one image. The walk is bounded: at most
//! [`MAX_CHUNKS`] chunks, every length checked against what is left, and no
//! nesting below an `ANMF`.

use super::{MAX_SIDE, Refused};

/// The most chunks walked, at the top level and inside animation frames
/// together.
pub const MAX_CHUNKS: usize = 1024;

/// The most pixels of all frames together: as many as one thumbnail of
/// [`MAX_SIDE`] × [`MAX_SIDE`], so that no decoder, even one that decodes
/// every frame of an animation, does more work than for the largest still.
pub const MAX_TOTAL_PIXELS: u64 = MAX_SIDE as u64 * MAX_SIDE as u64;

/// How a frame is coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// `VP8 `, with or without an `ALPH` chunk.
    Lossy,
    /// `VP8L`.
    Lossless,
}

/// One image frame and the size its own header declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
}

/// What the headers of a WebP say, once checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frames {
    /// The size the decoder reports: the `VP8X` canvas, or the only frame's.
    pub canvas: (u32, u32),
    pub animated: bool,
    pub frames: Vec<Frame>,
}

fn refused(why: impl Into<String>) -> Refused {
    Refused(why.into())
}

/// Checks `webp`'s chunks and every frame's size from the headers alone;
/// nothing is decoded and nothing is allocated by the frames' sizes.
pub fn check(webp: &[u8]) -> Result<Frames, Refused> {
    let Some((header, body)) = webp.split_first_chunk::<12>() else {
        return Err(refused("not a WebP: shorter than a RIFF header"));
    };
    let [r, i, f, f2, s0, s1, s2, s3, w, e, b, p] = *header;
    if [r, i, f, f2] != *b"RIFF" || [w, e, b, p] != *b"WEBP" {
        return Err(refused("not a WebP: no RIFF WEBP header"));
    }
    // The RIFF size counts "WEBP" and the chunks. Nothing may follow them:
    // the decoder looks for a VP8X file's chunks past the RIFF's end.
    let riff = u32::from_le_bytes([s0, s1, s2, s3]) as usize;
    if riff.checked_sub(4) != Some(body.len()) {
        return Err(refused(format!(
            "a RIFF size of {riff} bytes in a WebP of {}",
            webp.len()
        )));
    }
    let mut budget = MAX_CHUNKS;
    let top = chunks(body, &mut budget)?;
    let Some((first, rest)) = top.split_first() else {
        return Err(refused("a WebP without chunks"));
    };
    let frames = match &first.fourcc {
        b"VP8 " | b"VP8L" => {
            if let Some(extra) = rest.first() {
                return Err(refused(format!(
                    "a simple WebP with a further {} chunk",
                    extra.name()
                )));
            }
            let frame = image(first)?;
            Frames {
                canvas: (frame.width, frame.height),
                animated: false,
                frames: vec![frame],
            }
        }
        b"VP8X" => extended(first.data, rest, &mut budget)?,
        _ => {
            return Err(refused(format!(
                "a WebP whose first chunk is {}",
                first.name()
            )));
        }
    };
    bounded(&frames)?;
    Ok(frames)
}

/// The size limits on the canvas, each frame and all frames together.
fn bounded(frames: &Frames) -> Result<(), Refused> {
    let (canvas_width, canvas_height) = frames.canvas;
    let fits = |width: u32, height: u32, max_width: u32, max_height: u32| {
        width > 0 && height > 0 && width <= max_width && height <= max_height
    };
    if !fits(canvas_width, canvas_height, MAX_SIDE, MAX_SIDE) {
        return Err(refused(format!(
            "a canvas of {canvas_width} x {canvas_height} pixels, not a thumbnail of at most {MAX_SIDE}"
        )));
    }
    let mut total: u64 = 0;
    for frame in &frames.frames {
        let (width, height) = (frame.width, frame.height);
        if !fits(width, height, MAX_SIDE, MAX_SIDE) {
            return Err(refused(format!(
                "a frame of {width} x {height} pixels, not a thumbnail of at most {MAX_SIDE}"
            )));
        }
        if !fits(width, height, canvas_width, canvas_height) {
            return Err(refused(format!(
                "a frame of {width} x {height} pixels on a canvas of {canvas_width} x {canvas_height}"
            )));
        }
        total += u64::from(width) * u64::from(height);
    }
    if total > MAX_TOTAL_PIXELS {
        return Err(refused(format!(
            "{total} pixels in {} frames, more than {MAX_TOTAL_PIXELS}",
            frames.frames.len()
        )));
    }
    Ok(())
}

/// A RIFF chunk: its four-character code and its data, without the pad
/// byte.
struct Chunk<'a> {
    fourcc: [u8; 4],
    data: &'a [u8],
}

/// A chunk's code for a message: bytes that are not printable ASCII
/// escaped.
fn name(fourcc: &[u8; 4]) -> String {
    format!("'{}'", fourcc.escape_ascii())
}

impl Chunk<'_> {
    fn name(&self) -> String {
        name(&self.fourcc)
    }

    /// Whether the chunk is one of WebP's image or structure chunks, which
    /// have a fixed place; the others (ICCP, EXIF, XMP, unknown ones) are
    /// skipped wherever they are.
    fn is_structural(&self) -> bool {
        matches!(
            &self.fourcc,
            b"VP8 " | b"VP8L" | b"ALPH" | b"VP8X" | b"ANIM" | b"ANMF"
        )
    }
}

/// The chunks that fill `bytes` exactly, each padded to an even length;
/// every chunk counts against `budget`.
fn chunks<'a>(mut bytes: &'a [u8], budget: &mut usize) -> Result<Vec<Chunk<'a>>, Refused> {
    let mut found = Vec::new();
    while !bytes.is_empty() {
        if *budget == 0 {
            return Err(refused(format!("a WebP of more than {MAX_CHUNKS} chunks")));
        }
        *budget -= 1;
        let Some((header, rest)) = bytes.split_first_chunk::<8>() else {
            return Err(refused(format!(
                "a truncated chunk header ({} bytes left)",
                bytes.len()
            )));
        };
        let [a, b, c, d, l0, l1, l2, l3] = *header;
        let fourcc = [a, b, c, d];
        let len = u32::from_le_bytes([l0, l1, l2, l3]) as usize;
        let padded = len + (len & 1);
        if padded > rest.len() {
            return Err(refused(format!(
                "a {} chunk of {len} bytes with {} left",
                name(&fourcc),
                rest.len()
            )));
        }
        found.push(Chunk {
            fourcc,
            data: &rest[..len],
        });
        bytes = &rest[padded..];
    }
    Ok(found)
}

/// The frame of a `VP8 ` or `VP8L` chunk.
fn image(chunk: &Chunk<'_>) -> Result<Frame, Refused> {
    match &chunk.fourcc {
        b"VP8 " => vp8(chunk.data),
        b"VP8L" => vp8l(chunk.data),
        _ => Err(refused(format!(
            "{} where an image was expected",
            chunk.name()
        ))),
    }
}

/// A `VP8 ` key frame's header: the 3-byte frame tag, the start code, then
/// the 14-bit width and height (the top two bits are the scaling, which the
/// decoder ignores).
fn vp8(data: &[u8]) -> Result<Frame, Refused> {
    let &[tag, _, _, s0, s1, s2, w0, w1, h0, h1, ..] = data else {
        return Err(refused(format!(
            "a VP8 frame of {} bytes, shorter than its header",
            data.len()
        )));
    };
    if tag & 1 != 0 {
        return Err(refused("a VP8 frame that is not a key frame"));
    }
    if [s0, s1, s2] != [0x9d, 0x01, 0x2a] {
        return Err(refused("a VP8 frame without its start code"));
    }
    Ok(Frame {
        codec: Codec::Lossy,
        width: u32::from(u16::from_le_bytes([w0, w1]) & 0x3fff),
        height: u32::from(u16::from_le_bytes([h0, h1]) & 0x3fff),
    })
}

/// A `VP8L` header: the signature `0x2f`, then 14 bits of width − 1, 14 of
/// height − 1, the alpha bit and a 3-bit version that must be 0.
fn vp8l(data: &[u8]) -> Result<Frame, Refused> {
    let &[0x2f, b0, b1, b2, b3, ..] = data else {
        return Err(refused("a VP8L frame without its header"));
    };
    let bits = u32::from_le_bytes([b0, b1, b2, b3]);
    if bits >> 29 != 0 {
        return Err(refused("a VP8L frame of an unknown version"));
    }
    Ok(Frame {
        codec: Codec::Lossless,
        width: (bits & 0x3fff) + 1,
        height: ((bits >> 14) & 0x3fff) + 1,
    })
}

/// A little-endian 24-bit number.
fn u24(bytes: [u8; 3]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

/// An extended WebP: the `VP8X` header's canvas and flags, then a still
/// with one image or an animation of `ANMF` frames.
fn extended(header: &[u8], rest: &[Chunk<'_>], budget: &mut usize) -> Result<Frames, Refused> {
    let &[flags, _, _, _, w0, w1, w2, h0, h1, h2] = header else {
        return Err(refused(format!(
            "a VP8X header of {} bytes, not 10",
            header.len()
        )));
    };
    let canvas = (u24([w0, w1, w2]) + 1, u24([h0, h1, h2]) + 1);
    let animated = flags & 0b10 != 0;
    let frames = if animated {
        animation(canvas, rest, budget)?
    } else {
        vec![still(rest)?]
    };
    Ok(Frames {
        canvas,
        animated,
        frames,
    })
}

/// A `VP8X` still: one `VP8 ` (after at most one `ALPH`) or one `VP8L`.
fn still(rest: &[Chunk<'_>]) -> Result<Frame, Refused> {
    let mut alpha = false;
    let mut frame = None;
    for chunk in rest.iter().filter(|chunk| chunk.is_structural()) {
        match (&chunk.fourcc, frame) {
            (b"ALPH", None) if !alpha => alpha = true,
            (b"VP8 ", None) => frame = Some(vp8(chunk.data)?),
            (b"VP8L", None) if !alpha => frame = Some(vp8l(chunk.data)?),
            _ => {
                return Err(refused(format!(
                    "a {} chunk out of place in a still WebP",
                    chunk.name()
                )));
            }
        }
    }
    frame.ok_or_else(|| refused("a still WebP without an image"))
}

/// A `VP8X` animation: `ANIM` and `ANMF` chunks, each frame inside the
/// canvas, its image the size of its rectangle.
fn animation(
    (canvas_width, canvas_height): (u32, u32),
    rest: &[Chunk<'_>],
    budget: &mut usize,
) -> Result<Vec<Frame>, Refused> {
    let mut frames = Vec::new();
    let mut anim = false;
    for chunk in rest.iter().filter(|chunk| chunk.is_structural()) {
        match &chunk.fourcc {
            b"ANIM" if !anim && chunk.data.len() >= 6 => anim = true,
            b"ANMF" => {
                let Placed {
                    x,
                    y,
                    width,
                    height,
                    image,
                } = animation_frame(chunk.data, budget)?;
                if x + width > canvas_width || y + height > canvas_height {
                    return Err(refused(format!(
                        "an animation frame of {width} x {height} at {x}, {y} outside the canvas of {canvas_width} x {canvas_height}"
                    )));
                }
                if (image.width, image.height) != (width, height) {
                    return Err(refused(format!(
                        "an animation frame of {width} x {height} whose image is {} x {} pixels",
                        image.width, image.height
                    )));
                }
                frames.push(image);
            }
            _ => {
                return Err(refused(format!(
                    "a {} chunk out of place in an animated WebP",
                    chunk.name()
                )));
            }
        }
    }
    if !anim || frames.is_empty() {
        return Err(refused("an animated WebP without its ANIM or ANMF chunks"));
    }
    Ok(frames)
}

/// An animation frame: its rectangle on the canvas and its image.
struct Placed {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    image: Frame,
}

/// An `ANMF` chunk: a 16-byte header (x / 2, y / 2, width − 1,
/// height − 1, duration, flags), then the frame's chunks: `ALPH` and
/// `VP8 `, `VP8 `, or `VP8L`, then only unknown ones.
fn animation_frame(data: &[u8], budget: &mut usize) -> Result<Placed, Refused> {
    let Some((header, body)) = data.split_first_chunk::<16>() else {
        return Err(refused(format!(
            "an ANMF chunk of {} bytes, shorter than its header",
            data.len()
        )));
    };
    let field = |at: usize| u24([header[at], header[at + 1], header[at + 2]]);
    let subchunks = chunks(body, budget)?;
    let mut subchunks = subchunks.iter();
    let image = match subchunks.next() {
        Some(alpha) if alpha.fourcc == *b"ALPH" => match subchunks.next() {
            Some(chunk) if chunk.fourcc == *b"VP8 " => vp8(chunk.data)?,
            _ => {
                return Err(refused(
                    "an animation frame's ALPH chunk without a VP8 image",
                ));
            }
        },
        Some(chunk) => image(chunk)?,
        None => return Err(refused("an animation frame without an image")),
    };
    if let Some(extra) = subchunks.find(|chunk| chunk.is_structural()) {
        return Err(refused(format!(
            "a further {} chunk in an animation frame",
            extra.name()
        )));
    }
    Ok(Placed {
        x: field(0) * 2,
        y: field(3) * 2,
        width: field(6) + 1,
        height: field(9) + 1,
        image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lossy 2 × 2 red key frame (the `VP8 ` chunk's data of the test
    /// image in `image-webp`'s decoder tests, made with ImageMagick).
    const RED_2X2: [u8; 48] = [
        0xd0, 0x01, 0x00, 0x9d, 0x01, 0x2a, 0x02, 0x00, 0x02, 0x00, 0x02, 0x00, 0x34, 0x25, 0xa0,
        0x02, 0x74, 0xba, 0x01, 0xf8, 0x00, 0x03, 0xb0, 0x00, 0xfe, 0xf0, 0xc4, 0x0b, 0xff, 0x20,
        0xb9, 0x61, 0x75, 0xc8, 0xd7, 0xff, 0x20, 0x3f, 0xe4, 0x07, 0xfc, 0x80, 0xff, 0xf8, 0xf2,
        0x00, 0x00, 0x00,
    ];

    fn chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = fourcc.to_vec();
        out.extend_from_slice(&u32::try_from(data.len()).expect("len").to_le_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&u32::try_from(body.len() + 4).expect("len").to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(&body);
        out
    }

    fn le24(value: u32) -> [u8; 3] {
        let [a, b, c, _] = value.to_le_bytes();
        [a, b, c]
    }

    /// A `VP8 ` key frame header declaring `width` × `height`, and a few
    /// bytes of a first partition (never decoded).
    fn vp8_header(width: u16, height: u16) -> Vec<u8> {
        let mut data = vec![0x50, 0x00, 0x00, 0x9d, 0x01, 0x2a];
        data.extend_from_slice(&width.to_le_bytes());
        data.extend_from_slice(&height.to_le_bytes());
        data.extend_from_slice(&[0; 8]);
        chunk(b"VP8 ", &data)
    }

    /// A `VP8L` header declaring `width` × `height` (1..=16384).
    fn vp8l_header(width: u32, height: u32) -> Vec<u8> {
        let bits = (width - 1) | ((height - 1) << 14);
        let mut data = vec![0x2f];
        data.extend_from_slice(&bits.to_le_bytes());
        data.extend_from_slice(&[0; 4]);
        chunk(b"VP8L", &data)
    }

    fn vp8x(flags: u8, width: u32, height: u32) -> Vec<u8> {
        let mut data = vec![flags, 0, 0, 0];
        data.extend_from_slice(&le24(width - 1));
        data.extend_from_slice(&le24(height - 1));
        chunk(b"VP8X", &data)
    }

    const ANIMATED: u8 = 0b10;

    fn anim() -> Vec<u8> {
        chunk(b"ANIM", &[0, 0, 0, 0, 0, 0])
    }

    fn anmf(x: u32, y: u32, width: u32, height: u32, inner: &[Vec<u8>]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&le24(x / 2));
        data.extend_from_slice(&le24(y / 2));
        data.extend_from_slice(&le24(width - 1));
        data.extend_from_slice(&le24(height - 1));
        data.extend_from_slice(&[100, 0, 0, 0]);
        data.extend_from_slice(&inner.concat());
        chunk(b"ANMF", &data)
    }

    fn refusal(webp: &[u8]) -> String {
        check(webp).expect_err("refused").0
    }

    fn lossy(width: u32, height: u32) -> Frame {
        Frame {
            codec: Codec::Lossy,
            width,
            height,
        }
    }

    #[test]
    fn the_layouts_of_webp_pass_with_their_frames() {
        let red = chunk(b"VP8 ", &RED_2X2);
        let simple = check(&riff(std::slice::from_ref(&red))).expect("simple");
        assert_eq!(simple.canvas, (2, 2));
        assert_eq!(simple.frames, [lossy(2, 2)]);
        // An extended still, with metadata and an alpha chunk.
        let still = riff(&[
            vp8x(0b0001_1000, 2, 2),
            chunk(b"ALPH", &[0, 255, 255, 255, 255]),
            red.clone(),
            chunk(b"EXIF", b"odd"),
        ]);
        let still = check(&still).expect("still");
        assert_eq!((still.canvas, still.animated), ((2, 2), false));
        assert_eq!(still.frames, [lossy(2, 2)]);
        let lossless = check(&riff(&[vp8x(0, 300, 200), vp8l_header(300, 200)])).expect("VP8L");
        assert_eq!(lossless.frames[0].codec, Codec::Lossless);
        // An animation of two frames, one with alpha, one smaller.
        let animation = riff(&[
            vp8x(ANIMATED, 4, 4),
            anim(),
            anmf(0, 0, 2, 2, std::slice::from_ref(&red)),
            anmf(
                2,
                2,
                2,
                2,
                &[chunk(b"ALPH", &[0]), red.clone(), chunk(b"JUNK", &[])],
            ),
        ]);
        let animation = check(&animation).expect("animation");
        assert!(animation.animated);
        assert_eq!(animation.frames, [lossy(2, 2), lossy(2, 2)]);
    }

    #[test]
    fn an_oversized_vp8_frame_under_a_small_canvas_is_refused() {
        // V-5: the canvas says 512 x 384, the frame 16383 x 16383.
        let webp = riff(&[vp8x(0, 512, 384), vp8_header(16383, 16383)]);
        assert!(webp.len() < 100);
        let why = refusal(&webp);
        assert!(why.contains("16383 x 16383 pixels"), "{why}");
        // The scaling bits are not part of the size.
        let scaled = riff(&[vp8x(0, 512, 384), vp8_header(0xc000 | 512, 0x4000 | 384)]);
        assert_eq!(check(&scaled).expect("scaled").frames, [lossy(512, 384)]);
        // Within MAX_SIDE but not the canvas's size.
        let why = refusal(&riff(&[vp8x(0, 512, 384), vp8_header(1024, 1024)]));
        assert!(why.contains("on a canvas of 512 x 384"), "{why}");
        // A simple VP8 file over MAX_SIDE, and one of zero width.
        let why = refusal(&riff(&[vp8_header(4000, 3000)]));
        assert!(why.contains("4000 x 3000 pixels"), "{why}");
        assert!(refusal(&riff(&[vp8_header(0, 10)])).contains("0 x 10"));
    }

    #[test]
    fn an_oversized_frame_of_an_animation_is_refused() {
        let frame = anmf(0, 0, 512, 384, &[vp8_header(16383, 16383)]);
        let why = refusal(&riff(&[vp8x(ANIMATED, 512, 384), anim(), frame]));
        assert!(why.contains("16383 x 16383 pixels"), "{why}");
        // The same behind an alpha chunk, and in a later frame.
        let frame = anmf(
            0,
            0,
            512,
            384,
            &[chunk(b"ALPH", &[0]), vp8_header(16383, 16383)],
        );
        let why = refusal(&riff(&[vp8x(ANIMATED, 512, 384), anim(), frame]));
        assert!(why.contains("16383 x 16383 pixels"), "{why}");
        let good = anmf(0, 0, 512, 384, &[vp8_header(512, 384)]);
        let bad = anmf(0, 0, 512, 384, &[vp8l_header(16384, 16384)]);
        let why = refusal(&riff(&[vp8x(ANIMATED, 512, 384), anim(), good, bad]));
        assert!(why.contains("16384 x 16384 pixels"), "{why}");
        // A frame outside the canvas, or larger than its rectangle.
        let outside = anmf(512, 0, 2, 2, &[chunk(b"VP8 ", &RED_2X2)]);
        let why = refusal(&riff(&[vp8x(ANIMATED, 512, 384), anim(), outside]));
        assert!(why.contains("outside the canvas"), "{why}");
        let larger = anmf(0, 0, 2, 2, &[vp8_header(4, 4)]);
        let why = refusal(&riff(&[vp8x(ANIMATED, 512, 384), anim(), larger]));
        assert!(why.contains("whose image is 4 x 4"), "{why}");
        // Each frame within bounds, but more pixels than MAX_TOTAL_PIXELS.
        let full = || anmf(0, 0, MAX_SIDE, MAX_SIDE, &[vp8l_header(MAX_SIDE, MAX_SIDE)]);
        let one = riff(&[vp8x(ANIMATED, MAX_SIDE, MAX_SIDE), anim(), full()]);
        assert!(check(&one).is_ok());
        let two = riff(&[vp8x(ANIMATED, MAX_SIDE, MAX_SIDE), anim(), full(), full()]);
        let why = refusal(&two);
        assert!(why.contains("in 2 frames"), "{why}");
    }

    #[test]
    fn an_oversized_vp8l_frame_is_refused() {
        let why = refusal(&riff(&[vp8l_header(16384, 16384)]));
        assert!(why.contains("16384 x 16384 pixels"), "{why}");
        let why = refusal(&riff(&[vp8x(0, 512, 384), vp8l_header(2000, 2000)]));
        assert!(why.contains("2000 x 2000 pixels"), "{why}");
        let why = refusal(&riff(&[vp8l_header(MAX_SIDE + 1, 2)]));
        assert!(why.contains("pixels"), "{why}");
        // A canvas over MAX_SIDE, its frame as large.
        let why = refusal(&riff(&[vp8x(0, 2000, 10), vp8l_header(2000, 10)]));
        assert!(why.contains("a canvas of 2000 x 10"), "{why}");
    }

    #[test]
    fn truncated_and_overlong_chunks_are_refused() {
        let good = riff(&[vp8x(0, 2, 2), chunk(b"VP8 ", &RED_2X2)]);
        assert!(check(&good).is_ok());
        // Cut anywhere: the RIFF size no longer matches.
        for len in [0, 4, 11, 12, 20, good.len() - 1] {
            assert!(check(&good[..len]).is_err(), "{len}");
        }
        // Bytes after the RIFF's end.
        let mut trailing = good.clone();
        trailing.extend_from_slice(&chunk(b"VP8 ", &[0; 10]));
        assert!(refusal(&trailing).contains("RIFF size"));
        // A chunk length beyond the RIFF, with the RIFF size consistent.
        let mut overlong = riff(&[vp8x(0, 2, 2), chunk(b"VP8 ", &RED_2X2)]);
        let at = overlong.len() - RED_2X2.len() - 4;
        overlong[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(refusal(&overlong).contains("'VP8 ' chunk of 4294967295 bytes"));
        // An odd length whose pad byte is missing.
        let mut unpadded = b"VP8 ".to_vec();
        unpadded.extend_from_slice(&49u32.to_le_bytes());
        unpadded.extend_from_slice(&RED_2X2);
        unpadded.push(0);
        let why = refusal(&riff(&[vp8x(0, 2, 2), unpadded]));
        assert!(why.contains("of 49 bytes with 49 left"), "{why}");
        // A chunk header cut short.
        let mut cut = vec![vp8x(0, 2, 2), chunk(b"VP8 ", &RED_2X2)];
        cut.push(b"JUNK".to_vec());
        assert!(refusal(&riff(&cut)).contains("truncated chunk header"));
        // Inside an animation frame.
        let mut inner = chunk(b"VP8 ", &RED_2X2);
        inner[4..8].copy_from_slice(&1000u32.to_le_bytes());
        let frame = anmf(0, 0, 2, 2, &[inner]);
        assert!(refusal(&riff(&[vp8x(ANIMATED, 2, 2), anim(), frame])).contains("1000 bytes"));
        let short = chunk(b"ANMF", &[0; 10]);
        assert!(refusal(&riff(&[vp8x(ANIMATED, 2, 2), anim(), short])).contains("shorter"));
        // Headers too short for their frame's size.
        assert!(refusal(&riff(&[chunk(b"VP8 ", &RED_2X2[..9])])).contains("shorter"));
        assert!(refusal(&riff(&[chunk(b"VP8L", &[0x2f, 0, 0])])).contains("VP8L"));
        assert!(refusal(&riff(&[chunk(b"VP8X", &[0; 9])])).contains("not 10"));
    }

    #[test]
    fn unknown_layouts_are_refused() {
        let red = || chunk(b"VP8 ", &RED_2X2);
        let alph = || chunk(b"ALPH", &[0]);
        let cases: Vec<(&str, Vec<Vec<u8>>)> = vec![
            ("first chunk", vec![chunk(b"JUNK", &[]), red()]),
            ("further", vec![red(), chunk(b"EXIF", &[])]),
            ("further", vec![red(), red()]),
            ("without chunks", vec![]),
            ("out of place", vec![vp8x(0, 2, 2), red(), red()]),
            (
                "out of place",
                vec![vp8x(0, 2, 2), red(), vp8l_header(2, 2)],
            ),
            ("out of place", vec![vp8x(0, 2, 2), red(), alph()]),
            (
                "out of place",
                vec![vp8x(0, 2, 2), alph(), vp8l_header(2, 2)],
            ),
            ("out of place", vec![vp8x(0, 2, 2), alph(), alph(), red()]),
            ("out of place", vec![vp8x(0, 2, 2), vp8x(0, 2, 2), red()]),
            ("out of place", vec![vp8x(0, 2, 2), anim(), red()]),
            ("without an image", vec![vp8x(0, 2, 2), alph()]),
            ("out of place", vec![vp8x(ANIMATED, 2, 2), anim(), red()]),
            ("out of place", vec![vp8x(ANIMATED, 2, 2), anim(), anim()]),
            ("without its ANIM", vec![vp8x(ANIMATED, 2, 2), anim()]),
            (
                "without its ANIM",
                vec![vp8x(ANIMATED, 2, 2), anmf(0, 0, 2, 2, &[red()])],
            ),
            (
                "without a VP8",
                vec![vp8x(ANIMATED, 2, 2), anim(), anmf(0, 0, 2, 2, &[alph()])],
            ),
            (
                "without a VP8",
                vec![
                    vp8x(ANIMATED, 2, 2),
                    anim(),
                    anmf(0, 0, 2, 2, &[alph(), vp8l_header(2, 2)]),
                ],
            ),
            (
                "further",
                vec![
                    vp8x(ANIMATED, 2, 2),
                    anim(),
                    anmf(0, 0, 2, 2, &[red(), red()]),
                ],
            ),
            (
                "without an image",
                vec![vp8x(ANIMATED, 2, 2), anim(), anmf(0, 0, 2, 2, &[])],
            ),
            (
                "where an image",
                vec![
                    vp8x(ANIMATED, 2, 2),
                    anim(),
                    anmf(0, 0, 2, 2, &[anmf(0, 0, 2, 2, &[red()])]),
                ],
            ),
        ];
        for (expected, chunks) in cases {
            let why = refusal(&riff(&chunks));
            assert!(why.contains(expected), "{expected}: {why}");
        }
        // Not a key frame; no start code; a VP8L of another version.
        let mut inter = RED_2X2;
        inter[0] |= 1;
        assert!(refusal(&riff(&[chunk(b"VP8 ", &inter)])).contains("key frame"));
        let mut magic = RED_2X2;
        magic[4] = 0;
        assert!(refusal(&riff(&[chunk(b"VP8 ", &magic)])).contains("start code"));
        let mut version = vp8l_header(2, 2);
        version[12] |= 0x20;
        assert!(refusal(&riff(&[version])).contains("version"));
        assert!(refusal(b"RIFX\x04\0\0\0WEBP").contains("no RIFF WEBP"));
        // An odd code is escaped in the message.
        let why = refusal(&riff(&[chunk(b"\x1b[2J", &[])]));
        assert!(why.contains("'\\x1b[2J'"), "{why}");
    }

    #[test]
    fn decode_refuses_before_the_decoder_and_decodes_what_passes() {
        use super::super::decode;
        use image::GenericImageView;
        // V-5's files: `decode` gives the check's refusal, not the decoder's
        // (which would come only after it had decoded 16383 x 16383).
        let still = riff(&[vp8x(0, 512, 384), vp8_header(16383, 16383)]);
        let frame = anmf(0, 0, 512, 384, &[vp8_header(16383, 16383)]);
        let animation = riff(&[vp8x(ANIMATED, 512, 384), anim(), frame]);
        for webp in [still, animation] {
            assert_eq!(
                decode(&webp).expect_err("refused"),
                check(&webp).expect_err("refused")
            );
        }
        // A real lossy frame in each layout decodes.
        let red = || chunk(b"VP8 ", &RED_2X2);
        let layouts = [
            riff(&[red()]),
            riff(&[vp8x(0, 2, 2), red()]),
            riff(&[vp8x(ANIMATED, 2, 2), anim(), anmf(0, 0, 2, 2, &[red()])]),
        ];
        for webp in layouts {
            let image = decode(&webp).expect("decodes");
            assert_eq!(image.dimensions(), (2, 2));
            let [r, g, b, _] = image.get_pixel(1, 1).0;
            assert!(r > 200 && g < 50 && b < 50, "{:?}", (r, g, b));
        }
    }

    #[test]
    fn the_walk_is_bounded_in_chunks() {
        let junk = chunk(b"JUNK", &[]);
        let mut chunks = vec![vp8x(0, 2, 2), chunk(b"VP8 ", &RED_2X2)];
        chunks.resize(MAX_CHUNKS, junk.clone());
        assert!(check(&riff(&chunks)).is_ok());
        chunks.push(junk.clone());
        assert!(refusal(&riff(&chunks)).contains("more than 1024 chunks"));
        // Chunks inside animation frames count too.
        let frame = anmf(
            0,
            0,
            2,
            2,
            &[chunk(b"VP8 ", &RED_2X2), junk.clone(), junk.clone()],
        );
        let mut chunks = vec![vp8x(ANIMATED, 2, 2), anim()];
        chunks.resize(2 + MAX_CHUNKS / 4, frame);
        assert!(refusal(&riff(&chunks)).contains("chunks"));
    }
}
