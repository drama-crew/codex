use std::io::Cursor;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_utils_cache::BlockingLruCache;
use codex_utils_cache::sha1_digest;
use image::ColorType;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageDecoder;
use image::ImageEncoder;
use image::ImageFormat;
use image::ImageReader;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;

const DATA_URL_PREFIX: &str = "data:";
pub const PROMPT_IMAGE_PATCH_SIZE: u32 = 32;
/// Maximum width or height used when resizing images before uploading.
pub const MAX_DIMENSION: u32 = 2048;
/// Maximum accepted byte length for prompt image input representations.
///
/// This is a high sanity guard against pathological inputs, not a protocol
/// requirement or target upload size.
pub const MAX_PROMPT_IMAGE_INPUT_BYTES: usize = 1024 * 1024 * 1024;
const MAX_IMAGE_CACHE_BYTES: usize = 64 * 1024 * 1024;

pub mod error;

pub use crate::error::ImageProcessingError;

#[derive(Debug, Clone)]
pub struct EncodedImage {
    pub bytes: Arc<[u8]>,
    pub mime: String,
    pub source_width: u32,
    pub source_height: u32,
    pub width: u32,
    pub height: u32,
}

impl EncodedImage {
    pub fn into_data_url(self) -> String {
        data_url_from_bytes(&self.mime, &self.bytes)
    }
}

/// Wraps image bytes in a data URL without decoding or validating them.
pub fn data_url_from_bytes(mime: &str, bytes: &[u8]) -> String {
    let encoded = BASE64_STANDARD.encode(bytes);
    format!("data:{mime};base64,{encoded}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptImageMode {
    ResizeToFit,
    Original,
    ResizeWithLimits(PromptImageResizeLimits),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptImageResizeLimits {
    pub max_dimension: u32,
    pub max_patches: usize,
}

struct ImageMetadata {
    icc_profile: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
}

/// Drama deployment knobs for prompt image re-encoding.
///
/// Controls JPEG quality, maximum image dimension, and passthrough threshold when
/// `mode == ResizeToFit` and a non-GIF source is detected.  When all three env vars
/// are absent `from_env()` returns `None` and the upstream codex behaviour is
/// preserved byte-for-byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PromptImageRecodeConfig {
    /// JPEG encode quality [1, 100].  Default 90.
    pub jpeg_quality: u8,
    /// Maximum width/height for resize.  Default 2048.  Values <64 are treated as
    /// invalid and replaced with the default.
    pub max_dim: u32,
    /// Images whose byte length is ≤ this threshold AND whose dimensions fit within
    /// `max_dim` AND whose format is preservable are passed through unchanged.
    /// Default 512 KiB (524288).
    pub passthrough_bytes: usize,
}

impl PromptImageRecodeConfig {
    /// Read configuration from environment variables.
    ///
    /// * `DRAMA_PROMPT_IMAGE_JPEG_QUALITY`  — integer [1, 100], clamped
    /// * `DRAMA_PROMPT_IMAGE_MAX_DIM`       — integer ≥ 64; <64 falls back to 2048
    /// * `DRAMA_PROMPT_IMAGE_PASSTHROUGH_BYTES` — integer ≥ 0
    ///
    /// Returns `None` iff **all three** vars are absent (upstream byte-for-byte
    /// behaviour preserved).  If any one var is present the others fall back to
    /// their defaults (90 / 2048 / 524288).
    pub fn from_env() -> Option<Self> {
        let q = std::env::var("DRAMA_PROMPT_IMAGE_JPEG_QUALITY").ok();
        let d = std::env::var("DRAMA_PROMPT_IMAGE_MAX_DIM").ok();
        let p = std::env::var("DRAMA_PROMPT_IMAGE_PASSTHROUGH_BYTES").ok();
        if q.is_none() && d.is_none() && p.is_none() {
            return None;
        }
        Some(Self {
            jpeg_quality: q
                .and_then(|v| v.parse::<u8>().ok())
                .map(|v| v.clamp(1, 100))
                .unwrap_or(90),
            max_dim: d
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| *v >= 64)
                .unwrap_or(2048),
            passthrough_bytes: p
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(512 * 1024),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ImageCacheKey {
    digest: [u8; 20],
    mode: PromptImageMode,
    /// Recode config is part of the cache key so that two calls with different
    /// configs sharing the same source bytes do not collide in the LRU cache.
    recode: Option<PromptImageRecodeConfig>,
}

type ImageCache = BlockingLruCache<ImageCacheKey, EncodedImage>;

static IMAGE_CACHE: LazyLock<ImageCache> =
    LazyLock::new(|| BlockingLruCache::new(NonZeroUsize::new(32).unwrap_or(NonZeroUsize::MIN)));

/// Thin public wrapper: reads recode config from env and delegates to
/// `load_for_prompt_bytes_with`.  Public signature is unchanged from upstream.
pub fn load_for_prompt_bytes(
    path: &Path,
    file_bytes: Vec<u8>,
    mode: PromptImageMode,
) -> Result<EncodedImage, ImageProcessingError> {
    let cfg = PromptImageRecodeConfig::from_env();
    load_for_prompt_bytes_with(path, file_bytes, mode, cfg.as_ref())
}

/// Core implementation.  `cfg = None` → byte-for-byte upstream behaviour.
pub fn load_for_prompt_bytes_with(
    path: &Path,
    file_bytes: Vec<u8>,
    mode: PromptImageMode,
    cfg: Option<&PromptImageRecodeConfig>,
) -> Result<EncodedImage, ImageProcessingError> {
    let path_buf = path.to_path_buf();

    let key = ImageCacheKey {
        digest: sha1_digest(&file_bytes),
        mode,
        recode: cfg.copied(),
    };

    if let Some(image) = IMAGE_CACHE.get(&key) {
        return Ok(image);
    }

    let image = load_for_prompt_bytes_uncached(&path_buf, file_bytes, mode, cfg)?;
    cache_image(&IMAGE_CACHE, key, image.clone(), MAX_IMAGE_CACHE_BYTES);
    Ok(image)
}

fn load_for_prompt_bytes_uncached(
    path: &Path,
    file_bytes: Vec<u8>,
    mode: PromptImageMode,
    cfg: Option<&PromptImageRecodeConfig>,
) -> Result<EncodedImage, ImageProcessingError> {
    let path_buf = path.to_path_buf();
    (move || {
        let guessed_format = image::guess_format(&file_bytes)
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;
        let format = match guessed_format {
            ImageFormat::Png => Some(ImageFormat::Png),
            ImageFormat::Jpeg => Some(ImageFormat::Jpeg),
            ImageFormat::Gif => Some(ImageFormat::Gif),
            ImageFormat::WebP => Some(ImageFormat::WebP),
            _ => None,
        };

        let mut decoder = ImageReader::with_format(Cursor::new(&file_bytes), guessed_format)
            .into_decoder()
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;
        // Preserve the metadata most important for rendering prompt images faithfully: the color
        // profile and EXIF data, including orientation. Other format-specific metadata is
        // intentionally not copied.
        let metadata = ImageMetadata {
            // Only RGB profiles are safe across every re-encoding path. For example, JPEG decoding
            // can convert CMYK/YCCK pixels to RGB while retaining the source profile; copying it
            // would mislabel the output. Bytes 16..20 are the ICC data color space signature.
            icc_profile: decoder
                .icc_profile()
                .ok()
                .flatten()
                .filter(|profile| profile.get(16..20) == Some(b"RGB ")),
            exif: decoder.exif_metadata().ok().flatten(),
        };
        let dynamic = DynamicImage::from_decoder(decoder)
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;

        let (width, height) = dynamic.dimensions();

        // Drama recode branch: only when mode==ResizeToFit, cfg present, and NOT a GIF.
        // Returns early, bypassing the upstream target-dimensions/encode path below.
        // Known limitation (accepted): re-encoding strips EXIF, so JPEGs that rely on
        // the EXIF Orientation tag lose their rotation hint. The upstream >MAX_DIMENSION
        // resize path already behaves this way; the passthrough_bytes threshold merely
        // widens the affected set.
        if mode == PromptImageMode::ResizeToFit && cfg.is_some() && format != Some(ImageFormat::Gif)
        {
            let cfg = cfg.unwrap();

            let can_pass = file_bytes.len() <= cfg.passthrough_bytes
                && width <= cfg.max_dim
                && height <= cfg.max_dim
                && format.map(can_preserve_source_bytes).unwrap_or(false);

            if can_pass {
                // Direct passthrough: original bytes unchanged.
                let mime = format_to_mime(format.unwrap());
                return Ok(EncodedImage {
                    bytes: file_bytes.into(),
                    mime,
                    source_width: width,
                    source_height: height,
                    width,
                    height,
                });
            } else {
                // Resize if needed, flatten alpha, encode as JPEG.
                let resized = if width > cfg.max_dim || height > cfg.max_dim {
                    dynamic.resize(cfg.max_dim, cfg.max_dim, FilterType::Triangle)
                } else {
                    dynamic
                };
                let rgb = flatten_onto_white(&resized);
                let mut buffer = Vec::new();
                JpegEncoder::new_with_quality(&mut buffer, cfg.jpeg_quality)
                    .encode_image(&rgb)
                    .map_err(|source| ImageProcessingError::Encode {
                        format: ImageFormat::Jpeg,
                        source,
                    })?;
                return Ok(EncodedImage {
                    bytes: buffer.into(),
                    mime: "image/jpeg".to_string(),
                    source_width: width,
                    source_height: height,
                    width: rgb.width(),
                    height: rgb.height(),
                });
            }
        }

        // ---- upstream path (unchanged) ----
        let target_dimensions = match mode {
            PromptImageMode::ResizeToFit if width > MAX_DIMENSION || height > MAX_DIMENSION => {
                let resized = dynamic.resize(MAX_DIMENSION, MAX_DIMENSION, FilterType::Triangle);
                Some((resized.width(), resized.height(), resized))
            }
            PromptImageMode::ResizeWithLimits(limits) => {
                let (target_width, target_height) =
                    prompt_image_output_dimensions_for_limits(width, height, limits);
                if (target_width, target_height) == (width, height) {
                    None
                } else {
                    let resized =
                        dynamic.resize_exact(target_width, target_height, FilterType::Triangle);
                    Some((target_width, target_height, resized))
                }
            }
            PromptImageMode::ResizeToFit | PromptImageMode::Original => None,
        };

        let encoded = if let Some((prepared_width, prepared_height, resized)) = target_dimensions {
            let target_format = format
                .filter(|format| can_preserve_source_bytes(*format))
                .unwrap_or(ImageFormat::Png);
            let (bytes, output_format) = encode_image(&resized, target_format, metadata)?;
            let mime = format_to_mime(output_format);
            EncodedImage {
                bytes: bytes.into(),
                mime,
                source_width: width,
                source_height: height,
                width: prepared_width,
                height: prepared_height,
            }
        } else {
            if let Some(format) = format.filter(|format| can_preserve_source_bytes(*format)) {
                let mime = format_to_mime(format);
                EncodedImage {
                    bytes: file_bytes.into(),
                    mime,
                    source_width: width,
                    source_height: height,
                    width,
                    height,
                }
            } else {
                let (bytes, output_format) = encode_image(&dynamic, ImageFormat::Png, metadata)?;
                let mime = format_to_mime(output_format);
                EncodedImage {
                    bytes: bytes.into(),
                    mime,
                    source_width: width,
                    source_height: height,
                    width,
                    height,
                }
            }
        };

        Ok(encoded)
    })()
}

fn cache_image(cache: &ImageCache, key: ImageCacheKey, image: EncodedImage, byte_capacity: usize) {
    if image.bytes.len() > byte_capacity {
        return;
    }

    cache.with_mut(|cache| {
        cache.put(key, image);
        let mut cached_bytes = cache
            .iter()
            .map(|(_, image)| image.bytes.len())
            .sum::<usize>();
        while cached_bytes > byte_capacity {
            let Some((_, evicted)) = cache.pop_lru() else {
                break;
            };
            cached_bytes -= evicted.bytes.len();
        }
    });
}

pub fn load_data_url_for_prompt(
    image_url: &str,
    mode: PromptImageMode,
) -> Result<EncodedImage, ImageProcessingError> {
    load_data_url_for_prompt_with(image_url, mode, load_for_prompt_bytes)
}

pub fn load_data_url_for_prompt_uncached(
    image_url: &str,
    mode: PromptImageMode,
) -> Result<EncodedImage, ImageProcessingError> {
    // Drama: mirror `load_for_prompt_bytes`'s env-gated recode config so data-URL
    // prompt images get the same treatment as file-backed ones. With no
    // `DRAMA_PROMPT_IMAGE_*` vars set this is `None` and upstream behaviour is
    // preserved byte-for-byte.
    let cfg = PromptImageRecodeConfig::from_env();
    load_data_url_for_prompt_with(image_url, mode, |path, bytes, mode| {
        load_for_prompt_bytes_uncached(path, bytes, mode, cfg.as_ref())
    })
}

fn load_data_url_for_prompt_with(
    image_url: &str,
    mode: PromptImageMode,
    load: impl FnOnce(&Path, Vec<u8>, PromptImageMode) -> Result<EncodedImage, ImageProcessingError>,
) -> Result<EncodedImage, ImageProcessingError> {
    let rest = image_url
        .get(..DATA_URL_PREFIX.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(DATA_URL_PREFIX))
        .and_then(|_| image_url.get(DATA_URL_PREFIX.len()..))
        .ok_or_else(|| ImageProcessingError::InvalidDataUrl {
            reason: "missing data: prefix".to_string(),
        })?;
    let (metadata, encoded) =
        rest.split_once(',')
            .ok_or_else(|| ImageProcessingError::InvalidDataUrl {
                reason: "missing comma separator".to_string(),
            })?;
    if !metadata
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return Err(ImageProcessingError::InvalidDataUrl {
            reason: "only base64 data URLs are supported".to_string(),
        });
    }

    if encoded.len() > MAX_PROMPT_IMAGE_INPUT_BYTES {
        return Err(ImageProcessingError::ImageTooLarge {
            representation: "base64 payload",
            size: encoded.len(),
            max: MAX_PROMPT_IMAGE_INPUT_BYTES,
        });
    }
    let file_bytes =
        BASE64_STANDARD
            .decode(encoded)
            .map_err(|source| ImageProcessingError::InvalidDataUrl {
                reason: format!("invalid base64 payload: {source}"),
            })?;
    if file_bytes.len() > MAX_PROMPT_IMAGE_INPUT_BYTES {
        return Err(ImageProcessingError::ImageTooLarge {
            representation: "decoded input",
            size: file_bytes.len(),
            max: MAX_PROMPT_IMAGE_INPUT_BYTES,
        });
    }

    load(Path::new("<data-url-image>"), file_bytes, mode)
}

fn prompt_image_output_dimensions_for_limits(
    width: u32,
    height: u32,
    limits: PromptImageResizeLimits,
) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    if prompt_image_dimensions_fit(width, height, limits) {
        return (width, height);
    }

    let max_dimension_scale =
        (f64::from(limits.max_dimension) / f64::from(width.max(height))).min(1.0);
    let width = ((f64::from(width) * max_dimension_scale).round() as u32).max(1);
    let height = ((f64::from(height) * max_dimension_scale).round() as u32).max(1);
    if prompt_image_dimensions_fit(width, height, limits) {
        return (width, height);
    }

    let width_f64 = f64::from(width);
    let height_f64 = f64::from(height);
    let patch_size = f64::from(PROMPT_IMAGE_PATCH_SIZE);
    let mut scale =
        (patch_size * patch_size * limits.max_patches as f64 / width_f64 / height_f64).sqrt();
    // Match Responses patch-budget math: shrink by area, then round the scaled
    // patch grid down so integer output dimensions remain within the budget.
    let scaled_patches_wide = width_f64 * scale / patch_size;
    let scaled_patches_high = height_f64 * scale / patch_size;
    scale *= (scaled_patches_wide.floor() / scaled_patches_wide)
        .min(scaled_patches_high.floor() / scaled_patches_high);

    (
        ((width_f64 * scale).floor() as u32).max(1),
        ((height_f64 * scale).floor() as u32).max(1),
    )
}

fn prompt_image_dimensions_fit(width: u32, height: u32, limits: PromptImageResizeLimits) -> bool {
    let patches_wide = width.div_ceil(PROMPT_IMAGE_PATCH_SIZE);
    let patches_high = height.div_ceil(PROMPT_IMAGE_PATCH_SIZE);
    let patch_count = u64::from(patches_wide) * u64::from(patches_high);
    width <= limits.max_dimension
        && height <= limits.max_dimension
        && patch_count <= limits.max_patches as u64
}

/// Composite an RGBA image onto a solid white background, producing an RGB image.
/// JPEG has no alpha channel, so transparent pixels must be composited before encoding.
fn flatten_onto_white(image: &DynamicImage) -> DynamicImage {
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = px.0[3] as u32;
        let blend = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
        rgb.put_pixel(
            x,
            y,
            image::Rgb([blend(px.0[0]), blend(px.0[1]), blend(px.0[2])]),
        );
    }
    DynamicImage::ImageRgb8(rgb)
}

fn can_preserve_source_bytes(format: ImageFormat) -> bool {
    // Public API docs explicitly call out non-animated GIF support only.
    // Preserve byte-for-byte only for formats we can safely pass through.
    matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    )
}

fn encode_image(
    image: &DynamicImage,
    preferred_format: ImageFormat,
    metadata: ImageMetadata,
) -> Result<(Vec<u8>, ImageFormat), ImageProcessingError> {
    let target_format = match preferred_format {
        ImageFormat::Jpeg => ImageFormat::Jpeg,
        ImageFormat::WebP => ImageFormat::WebP,
        _ => ImageFormat::Png,
    };

    let mut buffer = Vec::new();
    let ImageMetadata { icc_profile, exif } = metadata;

    match target_format {
        ImageFormat::Png => {
            let rgba = image.to_rgba8();
            let mut encoder = PngEncoder::new(&mut buffer);
            apply_image_metadata(&mut encoder, icc_profile, exif, target_format)?;
            encoder
                .write_image(
                    rgba.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .map_err(|source| ImageProcessingError::Encode {
                    format: target_format,
                    source,
                })?;
        }
        ImageFormat::Jpeg => {
            let mut encoder = JpegEncoder::new_with_quality(&mut buffer, 85);
            apply_image_metadata(&mut encoder, icc_profile, exif, target_format)?;
            encoder
                .encode_image(image)
                .map_err(|source| ImageProcessingError::Encode {
                    format: target_format,
                    source,
                })?;
        }
        ImageFormat::WebP => {
            let rgba = image.to_rgba8();
            let mut encoder = WebPEncoder::new_lossless(&mut buffer);
            apply_image_metadata(&mut encoder, icc_profile, exif, target_format)?;
            encoder
                .write_image(
                    rgba.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .map_err(|source| ImageProcessingError::Encode {
                    format: target_format,
                    source,
                })?;
        }
        _ => unreachable!("unsupported target_format should have been handled earlier"),
    }

    Ok((buffer, target_format))
}

fn apply_image_metadata(
    encoder: &mut impl ImageEncoder,
    icc_profile: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
    format: ImageFormat,
) -> Result<(), ImageProcessingError> {
    if let Some(icc_profile) = icc_profile {
        encoder
            .set_icc_profile(icc_profile)
            .map_err(|source| ImageProcessingError::Encode {
                format,
                source: image::ImageError::Unsupported(source),
            })?;
    }
    if let Some(exif) = exif {
        encoder
            .set_exif_metadata(exif)
            .map_err(|source| ImageProcessingError::Encode {
                format,
                source: image::ImageError::Unsupported(source),
            })?;
    }
    Ok(())
}

fn format_to_mime(format: ImageFormat) -> String {
    match format {
        ImageFormat::Jpeg => "image/jpeg".to_string(),
        ImageFormat::Gif => "image/gif".to_string(),
        ImageFormat::WebP => "image/webp".to_string(),
        _ => "image/png".to_string(),
    }
}

#[cfg(test)]
#[path = "image_tests.rs"]
mod tests;
