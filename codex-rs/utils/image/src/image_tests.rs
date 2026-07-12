use std::io::Cursor;
use std::sync::Mutex;

use super::*;
use image::GenericImageView;
use image::ImageBuffer;
use image::ImageDecoder;
use image::Rgba;
use image::metadata::Orientation;

const TEST_RGB_ICC_PROFILE: &[u8] = b"0123456789abcdefRGB ";
const TEST_CMYK_ICC_PROFILE: &[u8] = b"0123456789abcdefCMYK";
const ROTATE_90_EXIF: &[u8] = &[
    0x49, 0x49, 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x12, 0x01, 0x03, 0x00, 0x01, 0x00,
    0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

fn image_bytes(image: &ImageBuffer<Rgba<u8>, Vec<u8>>, format: ImageFormat) -> Vec<u8> {
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut encoded, format)
        .expect("encode image to bytes");
    encoded.into_inner()
}

fn image_bytes_with_metadata(
    image: &ImageBuffer<Rgba<u8>, Vec<u8>>,
    format: ImageFormat,
    icc_profile: &[u8],
) -> Vec<u8> {
    let mut encoded = Vec::new();
    match format {
        ImageFormat::Png => {
            let mut encoder = PngEncoder::new(&mut encoded);
            encoder
                .set_icc_profile(icc_profile.to_vec())
                .expect("set PNG ICC profile");
            encoder
                .set_exif_metadata(ROTATE_90_EXIF.to_vec())
                .expect("set PNG EXIF metadata");
            encoder
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .expect("encode PNG with metadata");
        }
        ImageFormat::Jpeg => {
            let mut encoder = JpegEncoder::new_with_quality(&mut encoded, 90);
            encoder
                .set_icc_profile(icc_profile.to_vec())
                .expect("set JPEG ICC profile");
            encoder
                .set_exif_metadata(ROTATE_90_EXIF.to_vec())
                .expect("set JPEG EXIF metadata");
            encoder
                .encode_image(&DynamicImage::ImageRgba8(image.clone()))
                .expect("encode JPEG with metadata");
        }
        ImageFormat::WebP => {
            let mut encoder = WebPEncoder::new_lossless(&mut encoded);
            encoder
                .set_icc_profile(icc_profile.to_vec())
                .expect("set WebP ICC profile");
            encoder
                .set_exif_metadata(ROTATE_90_EXIF.to_vec())
                .expect("set WebP EXIF metadata");
            encoder
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    ColorType::Rgba8.into(),
                )
                .expect("encode WebP with metadata");
        }
        _ => panic!("unsupported test format"),
    }
    encoded
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
        assert_eq!(encoded.bytes.as_ref(), original_bytes);
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

        let loaded =
            image::load_from_memory(&processed.bytes).expect("read resized bytes back into image");
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
async fn resizing_preserves_supported_metadata() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
        let image = ImageBuffer::from_pixel(2050, 2, Rgba([200u8, 10, 10, 255]));
        let original_bytes = image_bytes_with_metadata(&image, format, TEST_RGB_ICC_PROFILE);

        let processed = load_for_prompt_bytes(
            Path::new("in-memory-image"),
            original_bytes,
            PromptImageMode::ResizeToFit,
        )
        .expect("process image");

        assert_eq!((processed.width, processed.height), (2048, 2));

        let mut decoder = ImageReader::with_format(Cursor::new(&processed.bytes), format)
            .into_decoder()
            .expect("create decoder");
        assert_eq!(
            (
                decoder.dimensions(),
                decoder.orientation().expect("read orientation"),
                decoder.icc_profile().expect("read ICC profile"),
                decoder.exif_metadata().expect("read EXIF metadata"),
            ),
            (
                (2048, 2),
                Orientation::Rotate90,
                Some(TEST_RGB_ICC_PROFILE.to_vec()),
                Some(ROTATE_90_EXIF.to_vec()),
            )
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resizing_drops_non_rgb_icc_profile() {
    let image = ImageBuffer::from_pixel(2050, 2, Rgba([200u8, 10, 10, 255]));
    let original_bytes =
        image_bytes_with_metadata(&image, ImageFormat::Jpeg, TEST_CMYK_ICC_PROFILE);

    let processed = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        original_bytes,
        PromptImageMode::ResizeToFit,
    )
    .expect("process image");

    let mut decoder = ImageReader::with_format(Cursor::new(&processed.bytes), ImageFormat::Jpeg)
        .into_decoder()
        .expect("create decoder");
    assert_eq!(
        (
            decoder.icc_profile().expect("read ICC profile"),
            decoder.exif_metadata().expect("read EXIF metadata"),
        ),
        (None, Some(ROTATE_90_EXIF.to_vec()))
    );
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
    assert_eq!(processed.bytes.as_ref(), original_bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn data_url_processing_preserves_supported_source_bytes() {
    let image = ImageBuffer::from_pixel(64, 32, Rgba([10u8, 20, 30, 255]));
    let original_bytes = image_bytes(&image, ImageFormat::Png);
    let image_url = data_url_from_bytes("image/png", &original_bytes)
        .replacen("data:", "DATA:", 1)
        .replacen(";base64,", ";BASE64,", 1);

    let processed = load_data_url_for_prompt(&image_url, PromptImageMode::ResizeToFit)
        .expect("process data URL image");

    assert_eq!(processed.width, 64);
    assert_eq!(processed.height, 32);
    assert_eq!(processed.mime, "image/png");
    assert_eq!(processed.bytes.as_ref(), original_bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn data_url_processing_converts_gif_to_png() {
    let image = ImageBuffer::from_pixel(64, 32, Rgba([10u8, 20, 30, 255]));
    let gif_bytes = image_bytes(&image, ImageFormat::Gif);
    let image_url = data_url_from_bytes("image/gif", &gif_bytes);

    let processed = load_data_url_for_prompt(&image_url, PromptImageMode::ResizeToFit)
        .expect("process GIF data URL");

    assert_eq!(processed.mime, "image/png");
    assert_eq!(
        image::guess_format(&processed.bytes).expect("detect processed format"),
        ImageFormat::Png
    );
}

#[test]
fn data_url_processing_rejects_malformed_input() {
    for image_url in [
        "image/png;base64,AAAA",
        "data:image/png;base64",
        "data:image/png,AAAA",
        "data:image/png;base64,not base64",
    ] {
        assert!(matches!(
            load_data_url_for_prompt(image_url, PromptImageMode::ResizeToFit),
            Err(ImageProcessingError::InvalidDataUrl { .. })
        ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resize_with_limits_respects_dimension_and_patch_budgets() {
    let image = ImageBuffer::from_pixel(2048, 2048, Rgba([200u8, 10, 10, 255]));
    let original_bytes = image_bytes(&image, ImageFormat::Png);
    let limits = PromptImageResizeLimits {
        max_dimension: 2048,
        max_patches: 2_500,
    };

    let processed = load_for_prompt_bytes(
        Path::new("in-memory-image"),
        original_bytes,
        PromptImageMode::ResizeWithLimits(limits),
    )
    .expect("process image with explicit limits");

    assert_eq!((processed.width, processed.height), (1600, 1600));
}

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
        ImageProcessingError::Decode { .. } | ImageProcessingError::UnsupportedImageFormat { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn reprocesses_updated_file_contents() {
    IMAGE_CACHE.clear();

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

#[tokio::test(flavor = "multi_thread")]
async fn bounds_cache_by_encoded_byte_size() {
    let cache = ImageCache::new(NonZeroUsize::new(4).expect("non-zero cache capacity"));
    let key = |digest_byte| ImageCacheKey {
        digest: [digest_byte; 20],
        mode: PromptImageMode::Original,
        recode: None,
    };
    let image = |size| EncodedImage {
        bytes: vec![0; size].into(),
        mime: "image/png".to_string(),
        width: 1,
        height: 1,
    };

    cache_image(&cache, key(1), image(3), /*byte_capacity*/ 5);
    cache_image(&cache, key(2), image(3), /*byte_capacity*/ 5);
    cache_image(&cache, key(3), image(6), /*byte_capacity*/ 5);

    assert!(cache.get(&key(1)).is_none());
    assert!(cache.get(&key(2)).is_some());
    assert!(cache.get(&key(3)).is_none());
}

// ---- drama: DRAMA_PROMPT_IMAGE_* recode knobs (test _with variant to avoid env global races) ----

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
    assert_eq!(out.bytes.as_ref(), png.as_slice()); // 原字节直通
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
    assert_eq!(out.bytes.as_ref(), png.as_slice());
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
    assert_eq!(out.bytes.as_ref(), png.as_slice());
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

// ---- drama: from_env test (serialize env access with a global mutex) ----

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

// ---- end of drama recode tests ----
