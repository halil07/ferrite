#![forbid(unsafe_code)]

//! Visual regression engine: decompression-bomb-safe PNG codec, Mapbox `pixelmatch`-compatible
//! YIQ perceptual diff with 3x3 anti-aliasing detection and dynamic mask regions, and baseline store.

pub mod baseline;
pub mod pixelmatch;
pub mod png_codec;

pub use baseline::{BaselinePolicy, SnapshotStore, VisualAssertionResult};
pub use pixelmatch::{DiffOptions, DiffReport, MaskRect, diff};
pub use png_codec::{
    MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS, RgbaImage, VisualError, decode_png, encode_png,
};
