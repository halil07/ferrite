#![forbid(unsafe_code)]

use std::io::Cursor;

use thiserror::Error;

pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 16_777_216;

#[derive(Debug, Error)]
pub enum VisualError {
    #[error("PNG image must be valid and at most {MAX_IMAGE_BYTES} bytes")]
    InvalidImageHeader,

    #[error("PNG image {width}x{height} exceeds maximum {MAX_IMAGE_PIXELS} pixels")]
    ImageTooLarge { width: u32, height: u32 },

    #[error("screenshot dimensions differ: baseline {bw}x{bh}, actual {aw}x{ah}")]
    DimensionMismatch { bw: u32, bh: u32, aw: u32, ah: u32 },

    #[error("invalid threshold {0}; expected 0.0..=1.0")]
    InvalidThreshold(f64),

    #[error("unsafe or invalid snapshot name {0:?}")]
    InvalidSnapshotName(String),

    #[error("PNG decoding error: {0}")]
    PngDecode(#[from] png::DecodingError),

    #[error("PNG encoding error: {0}")]
    PngEncode(#[from] png::EncodingError),

    #[error("filesystem I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Decodes a PNG byte buffer into 8-bit RGBA, inspecting the IHDR header dimensions *before*
/// allocating pixel buffers to prevent PNG decompression bombs.
pub fn decode_png(bytes: &[u8]) -> Result<RgbaImage, VisualError> {
    if bytes.len() > MAX_IMAGE_BYTES || bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(VisualError::InvalidImageHeader);
    }

    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width == 0 || height == 0 || (width as u64) * (height as u64) > MAX_IMAGE_PIXELS {
        return Err(VisualError::ImageTooLarge { width, height });
    }

    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info()?;
    let buf_size = reader
        .output_buffer_size()
        .ok_or(VisualError::ImageTooLarge { width, height })?;
    let mut buffer = vec![0u8; buf_size];
    let info = reader.next_frame(&mut buffer)?;
    buffer.truncate(info.buffer_size());

    let pixels = (info.width as usize) * (info.height as usize);
    let data = match info.color_type {
        png::ColorType::Rgba => buffer,
        png::ColorType::Rgb => buffer
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buffer
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buffer.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err(VisualError::InvalidImageHeader),
    };

    if data.len() != pixels * 4 {
        return Err(VisualError::InvalidImageHeader);
    }

    Ok(RgbaImage {
        width: info.width,
        height: info.height,
        data,
    })
}

pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>, VisualError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&image.data)?;
    }
    Ok(out)
}
