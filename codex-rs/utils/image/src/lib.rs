use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use codex_utils_cache::BlockingLruCache;
use codex_utils_cache::sha1_digest;
use image::ColorType;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageEncoder;
use image::ImageFormat;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
/// Maximum width or height used when resizing images before uploading.
pub const MAX_DIMENSION: u32 = 2048;

pub mod error;

pub use crate::error::ImageProcessingError;

#[derive(Debug, Clone)]
pub struct EncodedImage {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub width: u32,
    pub height: u32,
}

impl EncodedImage {
    pub fn into_data_url(self) -> String {
        let encoded = BASE64_STANDARD.encode(&self.bytes);
        format!("data:{};base64,{encoded}", self.mime)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PromptImageMode {
    ResizeToFit,
    Original,
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

static IMAGE_CACHE: LazyLock<BlockingLruCache<ImageCacheKey, EncodedImage>> =
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

    IMAGE_CACHE.get_or_try_insert_with(key, move || {
        let format = match image::guess_format(&file_bytes) {
            Ok(ImageFormat::Png) => Some(ImageFormat::Png),
            Ok(ImageFormat::Jpeg) => Some(ImageFormat::Jpeg),
            Ok(ImageFormat::Gif) => Some(ImageFormat::Gif),
            Ok(ImageFormat::WebP) => Some(ImageFormat::WebP),
            _ => None,
        };

        let dynamic = image::load_from_memory(&file_bytes)
            .map_err(|source| ImageProcessingError::decode_error(&path_buf, source))?;

        let (width, height) = dynamic.dimensions();

        // New recode branch: only when mode==ResizeToFit, cfg present, and NOT a GIF.
        // Known limitation (accepted): re-encoding strips EXIF, so JPEGs that rely on
        // the EXIF Orientation tag lose their rotation hint. The upstream >MAX_DIMENSION
        // resize path already behaves this way; the passthrough_bytes threshold merely
        // widens the affected set.
        if mode == PromptImageMode::ResizeToFit
            && cfg.is_some()
            && format != Some(ImageFormat::Gif)
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
                    bytes: file_bytes,
                    mime,
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
                    bytes: buffer,
                    mime: "image/jpeg".to_string(),
                    width: rgb.width(),
                    height: rgb.height(),
                });
            }
        }

        // ---- upstream path (unchanged) ----
        let encoded = if mode == PromptImageMode::Original
            || (width <= MAX_DIMENSION && height <= MAX_DIMENSION)
        {
            if let Some(format) = format.filter(|format| can_preserve_source_bytes(*format)) {
                let mime = format_to_mime(format);
                EncodedImage {
                    bytes: file_bytes,
                    mime,
                    width,
                    height,
                }
            } else {
                let (bytes, output_format) = encode_image(&dynamic, ImageFormat::Png)?;
                let mime = format_to_mime(output_format);
                EncodedImage {
                    bytes,
                    mime,
                    width,
                    height,
                }
            }
        } else {
            let resized = dynamic.resize(MAX_DIMENSION, MAX_DIMENSION, FilterType::Triangle);
            let target_format = format
                .filter(|format| can_preserve_source_bytes(*format))
                .unwrap_or(ImageFormat::Png);
            let (bytes, output_format) = encode_image(&resized, target_format)?;
            let mime = format_to_mime(output_format);
            EncodedImage {
                bytes,
                mime,
                width: resized.width(),
                height: resized.height(),
            }
        };

        Ok(encoded)
    })
}

/// Composite an RGBA image onto a solid white background, producing an RGB image.
/// JPEG has no alpha channel, so transparent pixels must be composited before encoding.
fn flatten_onto_white(image: &DynamicImage) -> DynamicImage {
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = px.0[3] as u32;
        let blend = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
        rgb.put_pixel(x, y, image::Rgb([blend(px.0[0]), blend(px.0[1]), blend(px.0[2])]));
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
) -> Result<(Vec<u8>, ImageFormat), ImageProcessingError> {
    let target_format = match preferred_format {
        ImageFormat::Jpeg => ImageFormat::Jpeg,
        ImageFormat::WebP => ImageFormat::WebP,
        _ => ImageFormat::Png,
    };

    let mut buffer = Vec::new();

    match target_format {
        ImageFormat::Png => {
            let rgba = image.to_rgba8();
            let encoder = PngEncoder::new(&mut buffer);
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
            encoder
                .encode_image(image)
                .map_err(|source| ImageProcessingError::Encode {
                    format: target_format,
                    source,
                })?;
        }
        ImageFormat::WebP => {
            let rgba = image.to_rgba8();
            let encoder = WebPEncoder::new_lossless(&mut buffer);
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

fn format_to_mime(format: ImageFormat) -> String {
    match format {
        ImageFormat::Jpeg => "image/jpeg".to_string(),
        ImageFormat::Gif => "image/gif".to_string(),
        ImageFormat::WebP => "image/webp".to_string(),
        _ => "image/png".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Mutex;

    use super::*;
    use image::GenericImageView;
    use image::ImageBuffer;
    use image::Rgba;

    fn image_bytes(image: &ImageBuffer<Rgba<u8>, Vec<u8>>, format: ImageFormat) -> Vec<u8> {
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image.clone())
            .write_to(&mut encoded, format)
            .expect("encode image to bytes");
        encoded.into_inner()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn returns_original_image_when_within_bounds() {
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let image = ImageBuffer::from_pixel(64, 32, Rgba([10u8, 20, 30, 255]));
            let original_bytes = image_bytes(&image, format);

            let encoded = load_for_prompt_bytes(
                Path::new("in-memory-image"),
                original_bytes.clone(),
                PromptImageMode::ResizeToFit,
            )
            .expect("process image");

            assert_eq!(encoded.width, 64);
            assert_eq!(encoded.height, 32);
            assert_eq!(encoded.mime, mime);
            assert_eq!(encoded.bytes, original_bytes);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn downscales_large_image() {
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let image = ImageBuffer::from_pixel(4096, 2048, Rgba([200u8, 10, 10, 255]));
            let original_bytes = image_bytes(&image, format);

            let processed = load_for_prompt_bytes(
                Path::new("in-memory-image"),
                original_bytes,
                PromptImageMode::ResizeToFit,
            )
            .expect("process image");

            assert!(processed.width <= MAX_DIMENSION);
            assert!(processed.height <= MAX_DIMENSION);
            assert_eq!(processed.mime, mime);

            let detected_format =
                image::guess_format(&processed.bytes).expect("detect resized output format");
            assert_eq!(detected_format, format);

            let loaded = image::load_from_memory(&processed.bytes)
                .expect("read resized bytes back into image");
            assert_eq!(loaded.dimensions(), (processed.width, processed.height));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn downscales_tall_image_to_fit_square_bounds() {
        let image = ImageBuffer::from_pixel(1024, 4096, Rgba([200u8, 10, 10, 255]));
        let original_bytes = image_bytes(&image, ImageFormat::Png);

        let processed = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            original_bytes,
            PromptImageMode::ResizeToFit,
        )
        .expect("process image");

        assert_eq!(processed.width, 512);
        assert_eq!(processed.height, MAX_DIMENSION);
        assert_eq!(processed.mime, "image/png");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preserves_large_image_in_original_mode() {
        let image = ImageBuffer::from_pixel(4096, 2048, Rgba([180u8, 30, 30, 255]));
        let original_bytes = image_bytes(&image, ImageFormat::Png);

        let processed = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            original_bytes.clone(),
            PromptImageMode::Original,
        )
        .expect("process image");

        assert_eq!(processed.width, 4096);
        assert_eq!(processed.height, 2048);
        assert_eq!(processed.mime, "image/png");
        assert_eq!(processed.bytes, original_bytes);
    }

    // ---- new recode tests (test _with variant to avoid env global races) ----

    #[test]
    fn recode_large_png_to_jpeg_within_max_dim() {
        // 1200x900 不可直通（bytes > passthrough 阈值），期望输出 JPEG 且尺寸不变（≤max_dim）
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(1200, 900, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x + y) % 253) as u8, 255])
        }));
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).unwrap();
        let cfg = PromptImageRecodeConfig { jpeg_quality: 90, max_dim: 2048, passthrough_bytes: 1024 };
        let out = load_for_prompt_bytes_with(Path::new("t.png"), png.clone(), PromptImageMode::ResizeToFit, Some(&cfg)).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        assert!(out.bytes.len() < png.len());
        assert_eq!((out.width, out.height), (1200, 900));
    }

    #[test]
    fn passthrough_small_image_untouched() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(64, 64, image::Rgba([1, 2, 3, 255])));
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).unwrap();
        let cfg = PromptImageRecodeConfig { jpeg_quality: 90, max_dim: 2048, passthrough_bytes: 512 * 1024 };
        let out = load_for_prompt_bytes_with(Path::new("t.png"), png.clone(), PromptImageMode::ResizeToFit, Some(&cfg)).unwrap();
        assert_eq!(out.mime, "image/png");
        assert_eq!(out.bytes, png); // 原字节直通
    }

    #[test]
    fn recode_resizes_above_max_dim() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(3000, 1500, |x, _| {
            image::Rgba([(x % 255) as u8, 0, 0, 255])
        }));
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).unwrap();
        let cfg = PromptImageRecodeConfig { jpeg_quality: 90, max_dim: 1536, passthrough_bytes: 1024 };
        let out = load_for_prompt_bytes_with(Path::new("t.png"), png, PromptImageMode::ResizeToFit, Some(&cfg)).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        assert!(out.width <= 1536 && out.height <= 1536);
    }

    #[test]
    fn original_mode_ignores_recode_config() {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(1200, 900, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 241) as u8, 7, 255])
        }));
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).unwrap();
        let cfg = PromptImageRecodeConfig { jpeg_quality: 90, max_dim: 512, passthrough_bytes: 1 };
        let out = load_for_prompt_bytes_with(Path::new("t.png"), png.clone(), PromptImageMode::Original, Some(&cfg)).unwrap();
        assert_eq!(out.mime, "image/png");
        assert_eq!(out.bytes, png);
    }

    #[test]
    fn no_config_matches_upstream_behavior() {
        // cfg=None 时 2048 内 PNG 原样直通（上游行为）
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(1200, 900, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 241) as u8, 9, 255])
        }));
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).unwrap();
        let out = load_for_prompt_bytes_with(Path::new("t.png"), png.clone(), PromptImageMode::ResizeToFit, None).unwrap();
        assert_eq!(out.bytes, png);
    }

    #[test]
    fn alpha_flattened_onto_white() {
        // 全透明像素 → 重编码后应为白色（JPEG 无 alpha）
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(900, 900, image::Rgba([255, 0, 0, 0])));
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).unwrap();
        let cfg = PromptImageRecodeConfig { jpeg_quality: 90, max_dim: 2048, passthrough_bytes: 1 };
        let out = load_for_prompt_bytes_with(Path::new("t.png"), png, PromptImageMode::ResizeToFit, Some(&cfg)).unwrap();
        let decoded = image::load_from_memory(&out.bytes).unwrap().to_rgb8();
        let p = decoded.get_pixel(450, 450);
        assert!(p.0[0] > 245 && p.0[1] > 245 && p.0[2] > 245, "expected white, got {:?}", p);
    }

    #[test]
    fn gif_bypasses_recode_even_with_config() {
        // GIF is excluded from the recode branch: with cfg=Some the output must be
        // byte-identical to the upstream (cfg=None) path, never JPEG.
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(300, 200, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 199) as u8, 33, 255])
        }));
        let mut gif = Vec::new();
        img.write_to(&mut Cursor::new(&mut gif), ImageFormat::Gif)
            .unwrap();
        let cfg = PromptImageRecodeConfig {
            jpeg_quality: 90,
            max_dim: 2048,
            passthrough_bytes: 1, // would force recode for non-GIF inputs
        };
        let with_cfg = load_for_prompt_bytes_with(
            Path::new("t.gif"),
            gif.clone(),
            PromptImageMode::ResizeToFit,
            Some(&cfg),
        )
        .unwrap();
        let upstream = load_for_prompt_bytes_with(
            Path::new("t2.gif"),
            gif,
            PromptImageMode::ResizeToFit,
            None,
        )
        .unwrap();
        assert_ne!(with_cfg.mime, "image/jpeg");
        assert_eq!(with_cfg.mime, upstream.mime);
        assert_eq!(with_cfg.bytes, upstream.bytes);
    }

    // ---- from_env test (serialize env access with a global mutex) ----

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn from_env_all_absent_returns_none() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: single-threaded access guaranteed by ENV_MUTEX.
        unsafe {
            std::env::remove_var("DRAMA_PROMPT_IMAGE_JPEG_QUALITY");
            std::env::remove_var("DRAMA_PROMPT_IMAGE_MAX_DIM");
            std::env::remove_var("DRAMA_PROMPT_IMAGE_PASSTHROUGH_BYTES");
        }
        assert!(PromptImageRecodeConfig::from_env().is_none());
    }

    #[test]
    fn from_env_single_var_returns_some_with_defaults() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // SAFETY: single-threaded access guaranteed by ENV_MUTEX.
        unsafe {
            std::env::remove_var("DRAMA_PROMPT_IMAGE_JPEG_QUALITY");
            std::env::remove_var("DRAMA_PROMPT_IMAGE_MAX_DIM");
            std::env::remove_var("DRAMA_PROMPT_IMAGE_PASSTHROUGH_BYTES");
            std::env::set_var("DRAMA_PROMPT_IMAGE_JPEG_QUALITY", "75");
        }
        let cfg = PromptImageRecodeConfig::from_env().expect("should be Some when any var set");
        assert_eq!(cfg.jpeg_quality, 75);
        assert_eq!(cfg.max_dim, 2048);          // default
        assert_eq!(cfg.passthrough_bytes, 512 * 1024); // default
        // SAFETY: cleanup.
        unsafe { std::env::remove_var("DRAMA_PROMPT_IMAGE_JPEG_QUALITY"); }
    }

    // ---- end of new tests ----

    #[tokio::test(flavor = "multi_thread")]
    async fn fails_cleanly_for_invalid_images() {
        let err = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            b"not an image".to_vec(),
            PromptImageMode::ResizeToFit,
        )
        .expect_err("invalid image should fail");
        assert!(matches!(
            err,
            ImageProcessingError::Decode { .. }
                | ImageProcessingError::UnsupportedImageFormat { .. }
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reprocesses_updated_file_contents() {
        {
            IMAGE_CACHE.clear();
        }

        let first_image = ImageBuffer::from_pixel(32, 16, Rgba([20u8, 120, 220, 255]));
        let first_bytes = image_bytes(&first_image, ImageFormat::Png);

        let first = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            first_bytes,
            PromptImageMode::ResizeToFit,
        )
        .expect("process first image");

        let second_image = ImageBuffer::from_pixel(96, 48, Rgba([50u8, 60, 70, 255]));
        let second_bytes = image_bytes(&second_image, ImageFormat::Png);

        let second = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            second_bytes,
            PromptImageMode::ResizeToFit,
        )
        .expect("process updated image");

        assert_eq!(first.width, 32);
        assert_eq!(first.height, 16);
        assert_eq!(second.width, 96);
        assert_eq!(second.height, 48);
        assert_ne!(second.bytes, first.bytes);
    }
}
