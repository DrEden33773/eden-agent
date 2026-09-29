//! Image preparation never mutates a committed record; callers commit the returned version
//! alongside context changes only after request-wide validation succeeds.
use base64::{Engine, engine::general_purpose::STANDARD};
use eden_protocol::{coding::Block, models::ModelTarget};
use image::{AnimationDecoder, DynamicImage, ImageFormat, ImageReader, imageops::FilterType};
use serde::{Deserialize, Serialize};
use std::{fmt, io::Cursor};

/// Failures preserve the caller's draft and identify the explicit recovery needed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(missing_docs)] // Payloads describe the failed constraint.
pub enum InputError {
    InvalidLimit(String),
    InvalidImage(String),
    UnsupportedModel {
        provider: String,
        model: String,
    },
    LimitExceeded {
        constraint: String,
        actual: u64,
        maximum: u64,
    },
}
impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit(message) | Self::InvalidImage(message) => f.write_str(message),
            Self::UnsupportedModel { provider, model } => write!(
                f,
                "{provider}/{model} does not support images; explicitly omit images or choose \
                 another model"
            ),
            Self::LimitExceeded {
                constraint,
                actual,
                maximum,
            } => write!(
                f,
                "image {constraint} {actual} exceeds {maximum}; explicitly re-adapt, adjust \
                 context, or choose another model"
            ),
        }
    }
}
impl std::error::Error for InputError {}

/// These are actual model constraints. Missing limits remain unknown, never unlimited
/// claims. Auto mode additionally uses conservative preparation dimensions.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct ImageLimits {
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub max_pixels: Option<u64>,
    pub max_image_bytes: Option<u64>,
    pub max_images: Option<u64>,
    /// Limit for the complete encoded provider request; callers pass its measured body
    /// length to `validate_images`, after provider serialization.
    pub max_body_bytes: Option<u64>,
}

/// Auto applies only to a new image. Existing images keep their sent bytes unless the
/// caller supplies ReAdapt or Omit as an explicit user/context-edit choice.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ImageChoice {
    #[default]
    Auto,
    Preserve,
    Omit,
    ReAdapt,
}

/// Payload indexes avoid storing an identical original and sent image twice. Versions
/// remain append-only even when the active image is omitted for a text-only model.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[allow(missing_docs)]
pub struct ImageRecord {
    pub payloads: Vec<Block>,
    pub original: usize,
    pub versions: Vec<ImageVersion>,
    pub active: Option<usize>,
    /// Retains explicit omission even when the image has never been sent.
    #[serde(default)]
    pub omitted: bool,
}

/// The target and explanation allow history to show how a sent version was prepared.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct ImageVersion {
    pub payload: usize,
    pub provider: String,
    pub model: String,
    pub width: u32,
    pub height: u32,
    pub explanation: String,
}

impl ImageRecord {
    /// Capture the original before any processing, independent of the source file's lifetime.
    pub fn new(original: Block) -> Result<Self, InputError> {
        decode(&original)?;
        Ok(Self {
            payloads: vec![original],
            original: 0,
            versions: Vec::new(),
            active: None,
            omitted: false,
        })
    }

    /// The absent active version is an explicit omission (or an unprepared draft), not
    /// permission to drop an unsupported image silently.
    pub fn sent(&self) -> Result<Option<&Block>, InputError> {
        self.active
            .map(|index| {
                let version = self
                    .versions
                    .get(index)
                    .ok_or_else(|| invalid("invalid active image version"))?;
                self.payloads
                    .get(version.payload)
                    .ok_or_else(|| invalid("invalid image payload index"))
            })
            .transpose()
    }

    /// Returns a new record; the input keeps all earlier versions for branch/history replay.
    /// Preserve selects original bytes for a new image and existing sent bytes for history.
    pub fn prepare(
        &self,
        choice: ImageChoice,
        target: &ModelTarget,
        limits: &ImageLimits,
    ) -> Result<Self, InputError> {
        validate_limits(limits)?;
        let mut next = self.clone();
        if choice == ImageChoice::Omit {
            next.active = None;
            next.omitted = true;
            return Ok(next);
        }
        // An omitted historical record stays omitted until the user explicitly re-adapts it.
        if (self.omitted || (!self.versions.is_empty() && self.active.is_none()))
            && choice != ImageChoice::ReAdapt
        {
            return Ok(next);
        }
        supports_images(target)?;
        if choice != ImageChoice::ReAdapt
            && let Some(sent) = self.sent()?
        {
            validate_image(sent, limits)?;
            return Ok(next);
        }
        let original = self
            .payloads
            .get(self.original)
            .ok_or_else(|| invalid("invalid original image index"))?;
        let (decoded, original_bytes) = decode(original)?;
        let (sent, width, height) = if choice == ImageChoice::Preserve {
            validate_image(original, limits)?;
            (original.clone(), decoded.width(), decoded.height())
        } else {
            adapt(original, decoded, original_bytes, limits)?
        };
        let explanation = if &sent == original {
            "Original image preserved"
        } else {
            "Image adapted without upscaling; original retained"
        }
        .into();
        let payload = next
            .payloads
            .iter()
            .position(|candidate| candidate == &sent)
            .unwrap_or_else(|| {
                next.payloads.push(sent);
                next.payloads.len() - 1
            });
        next.versions.push(ImageVersion {
            payload,
            provider: target.provider.clone(),
            model: target.model.clone(),
            width,
            height,
            explanation,
        });
        next.active = Some(next.versions.len() - 1);
        next.omitted = false;
        Ok(next)
    }
}

/// Validate the complete projected request, including historical and tool-result images.
/// `body_bytes` must measure the encoded provider body, not decoded image byte lengths;
/// pass None before serialization and call again with Some immediately before transport.
pub fn validate_images(
    blocks: &[Block],
    target: &ModelTarget,
    limits: &ImageLimits,
    body_bytes: Option<u64>,
) -> Result<(), InputError> {
    validate_limits(limits)?;
    let count = blocks
        .iter()
        .filter(|block| matches!(block, Block::Image { .. }))
        .count() as u64;
    if count > 0 {
        supports_images(target)?;
    }
    check("count", count, limits.max_images)?;
    if let Some(body_bytes) = body_bytes {
        check("request body bytes", body_bytes, limits.max_body_bytes)?;
    }
    for block in blocks {
        if matches!(block, Block::Image { .. }) {
            validate_image(block, limits)?;
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> InputError {
    InputError::InvalidImage(message.into())
}
fn supports_images(target: &ModelTarget) -> Result<(), InputError> {
    if target.capabilities.images {
        Ok(())
    } else {
        Err(InputError::UnsupportedModel {
            provider: target.provider.clone(),
            model: target.model.clone(),
        })
    }
}
fn check(constraint: &str, actual: u64, maximum: Option<u64>) -> Result<(), InputError> {
    if let Some(maximum) = maximum
        && actual > maximum
    {
        return Err(InputError::LimitExceeded {
            constraint: constraint.into(),
            actual,
            maximum,
        });
    }
    Ok(())
}
fn validate_limits(limits: &ImageLimits) -> Result<(), InputError> {
    if [
        limits.max_width.map(u64::from),
        limits.max_height.map(u64::from),
        limits.max_pixels,
        limits.max_image_bytes,
        limits.max_body_bytes,
    ]
    .contains(&Some(0))
    {
        return Err(InputError::InvalidLimit(
            "image dimensions, pixels and byte limits must be positive".into(),
        ));
    }
    Ok(())
}
fn decode(block: &Block) -> Result<(DynamicImage, usize), InputError> {
    let Block::Image { media_type, data } = block else {
        return Err(invalid("expected an image block"));
    };
    let bytes = STANDARD
        .decode(data)
        .map_err(|error| invalid(format!("invalid image base64: {error}")))?;
    let format = image::guess_format(&bytes).map_err(|error| invalid(error.to_string()))?;
    if format.to_mime_type() != media_type {
        return Err(invalid("image media type does not match its bytes"));
    }
    let reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let decoded = reader
        .decode()
        .map_err(|error| invalid(error.to_string()))?;
    Ok((decoded, bytes.len()))
}
fn validate_image(block: &Block, limits: &ImageLimits) -> Result<(), InputError> {
    let (image, bytes) = decode(block)?;
    check(
        "width",
        u64::from(image.width()),
        limits.max_width.map(u64::from),
    )?;
    check(
        "height",
        u64::from(image.height()),
        limits.max_height.map(u64::from),
    )?;
    check(
        "pixels",
        u64::from(image.width()) * u64::from(image.height()),
        limits.max_pixels,
    )?;
    check("encoded image bytes", bytes as u64, limits.max_image_bytes)
}
fn adapt(
    original: &Block,
    decoded: DynamicImage,
    original_bytes: usize,
    limits: &ImageLimits,
) -> Result<(Block, u32, u32), InputError> {
    let width = decoded.width();
    let height = decoded.height();
    let pixel_limit = limits.max_pixels.unwrap_or(4_194_304);
    let scale = (f64::from(limits.max_width.unwrap_or(2048)) / f64::from(width))
        .min(f64::from(limits.max_height.unwrap_or(2048)) / f64::from(height))
        .min((pixel_limit as f64 / (f64::from(width) * f64::from(height))).sqrt())
        .min(1.0);
    let max_bytes = limits.max_image_bytes.unwrap_or(5 * 1024 * 1024);
    if scale == 1.0 && original_bytes as u64 <= max_bytes {
        return Ok((original.clone(), width, height));
    }
    reject_animated_adaptation(original)?;
    let mut scale = scale;
    loop {
        let w = (f64::from(width) * scale).floor().max(1.0) as u32;
        let h = (f64::from(height) * scale).floor().max(1.0) as u32;
        // Rounding a very thin image to one pixel can exceed the pixel budget.
        if u64::from(w) * u64::from(h) > pixel_limit {
            scale *= 0.75;
            continue;
        }
        let resized = decoded.resize_exact(w, h, FilterType::Lanczos3);
        let mut bytes = Cursor::new(Vec::new());
        resized
            .write_to(&mut bytes, ImageFormat::Png)
            .map_err(|error| invalid(error.to_string()))?;
        if bytes.get_ref().len() as u64 <= max_bytes {
            let sent = Block::Image {
                media_type: "image/png".into(),
                data: STANDARD.encode(bytes.into_inner()),
            };
            validate_image(&sent, limits)?;
            return Ok((sent, w, h));
        }
        if w == 1 && h == 1 {
            return Err(InputError::LimitExceeded {
                constraint: "encoded image bytes".into(),
                actual: bytes.get_ref().len() as u64,
                maximum: max_bytes,
            });
        }
        scale *= 0.75;
    }
}

fn reject_animated_adaptation(block: &Block) -> Result<(), InputError> {
    let Block::Image { data, .. } = block else {
        return Err(invalid("expected an image block"));
    };
    let bytes = STANDARD
        .decode(data)
        .map_err(|error| invalid(error.to_string()))?;
    let format = image::guess_format(&bytes).map_err(|error| invalid(error.to_string()))?;
    let animated = match format {
        ImageFormat::Gif => {
            let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
                .map_err(|error| invalid(error.to_string()))?;
            let frames = decoder
                .into_frames()
                .take(2)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| invalid(error.to_string()))?;
            frames.len() > 1
        }
        ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(bytes))
            .and_then(|decoder| decoder.is_apng())
            .map_err(|error| invalid(error.to_string()))?,
        ImageFormat::WebP => image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
            .map_err(|error| invalid(error.to_string()))?
            .has_animation(),
        _ => false,
    };
    if animated {
        return Err(invalid(
            "animated image adaptation would discard frames; preserve the original within model \
             limits or explicitly select a static image",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target() -> ModelTarget {
        let mut target = ModelTarget::default();
        target.capabilities.images = true;
        target.provider = "test".into();
        target.model = "vision".into();
        target
    }
    fn picture(width: u32, height: u32) -> Block {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(width, height)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        Block::Image {
            media_type: "image/png".into(),
            data: STANDARD.encode(bytes.into_inner()),
        }
    }
    #[test]
    fn adapts_proportionally_retains_original_without_upscaling() {
        let original = picture(400, 200);
        let draft = ImageRecord::new(original.clone()).unwrap();
        let limits = ImageLimits {
            max_width: Some(100),
            ..Default::default()
        };
        let sent = draft
            .prepare(ImageChoice::Auto, &target(), &limits)
            .unwrap();
        assert_eq!(sent.payloads[sent.original], original);
        assert_eq!((sent.versions[0].width, sent.versions[0].height), (100, 50));
        assert_eq!(draft.versions.len(), 0);
        let tiny = ImageRecord::new(picture(4, 2))
            .unwrap()
            .prepare(ImageChoice::Auto, &target(), &limits)
            .unwrap();
        assert_eq!(tiny.payloads.len(), 1);
        assert_eq!((tiny.versions[0].width, tiny.versions[0].height), (4, 2));
    }
    #[test]
    fn history_requires_explicit_readaptation_and_appends_versions() {
        let previous = ImageRecord::new(picture(400, 200))
            .unwrap()
            .prepare(ImageChoice::Auto, &target(), &ImageLimits::default())
            .unwrap();
        let limits = ImageLimits {
            max_width: Some(100),
            ..Default::default()
        };
        assert!(matches!(
            previous.prepare(ImageChoice::Auto, &target(), &limits),
            Err(InputError::LimitExceeded { .. })
        ));
        let changed = previous
            .prepare(ImageChoice::ReAdapt, &target(), &limits)
            .unwrap();
        assert_eq!(changed.versions.len(), 2);
        assert_eq!(changed.versions[0], previous.versions[0]);
        assert_eq!(changed.payloads[0], previous.payloads[0]);
        assert_eq!(changed.versions[1].width, 100);
    }
    #[test]
    fn text_model_requires_explicit_omission_and_keeps_history() {
        let previous = ImageRecord::new(picture(4, 2))
            .unwrap()
            .prepare(ImageChoice::Auto, &target(), &ImageLimits::default())
            .unwrap();
        let text = ModelTarget::default();
        assert!(matches!(
            previous.prepare(ImageChoice::Auto, &text, &ImageLimits::default()),
            Err(InputError::UnsupportedModel { .. })
        ));
        let omitted = previous
            .prepare(ImageChoice::Omit, &text, &ImageLimits::default())
            .unwrap();
        assert!(omitted.sent().unwrap().is_none());
        assert_eq!(omitted.versions, previous.versions);
        assert_eq!(omitted.payloads, previous.payloads);
    }
    #[test]
    fn preserve_bypasses_auto_defaults_but_respects_actual_limits() {
        let original = ImageRecord::new(picture(2100, 2)).unwrap();
        let preserved = original
            .prepare(ImageChoice::Preserve, &target(), &ImageLimits::default())
            .unwrap();
        assert_eq!(preserved.payloads.len(), 1);
        let limits = ImageLimits {
            max_width: Some(100),
            ..Default::default()
        };
        assert!(
            preserved
                .prepare(ImageChoice::Preserve, &target(), &limits)
                .is_err()
        );
    }
    #[test]
    fn checks_request_count_body_and_pixels() {
        let images = [picture(10, 10), picture(10, 10)];
        let limits = ImageLimits {
            max_images: Some(1),
            ..Default::default()
        };
        assert!(
            matches!(validate_images(&images, &target(), &limits, None), Err(InputError::LimitExceeded { constraint, .. }) if constraint == "count")
        );
        let limits = ImageLimits {
            max_body_bytes: Some(100),
            ..Default::default()
        };
        assert!(validate_images(&images, &target(), &limits, Some(101)).is_err());
        let limits = ImageLimits {
            max_pixels: Some(99),
            ..Default::default()
        };
        assert!(validate_images(&images, &target(), &limits, None).is_err());
    }
    #[test]
    fn thin_image_can_satisfy_a_single_pixel_limit() {
        let draft = ImageRecord::new(picture(100, 1)).unwrap();
        let limits = ImageLimits {
            max_pixels: Some(1),
            ..Default::default()
        };
        let result = draft
            .prepare(ImageChoice::Auto, &target(), &limits)
            .unwrap();
        assert_eq!(
            (result.versions[0].width, result.versions[0].height),
            (1, 1)
        );
    }
    #[test]
    fn animation_is_preserved_or_fails_without_silent_frame_loss() {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            for _ in 0..2 {
                encoder
                    .encode_frame(image::Frame::new(image::RgbaImage::new(4, 2)))
                    .unwrap();
            }
        }
        let original = Block::Image {
            media_type: "image/gif".into(),
            data: STANDARD.encode(bytes),
        };
        let draft = ImageRecord::new(original.clone()).unwrap();
        assert_eq!(
            draft
                .prepare(ImageChoice::Auto, &target(), &ImageLimits::default())
                .unwrap()
                .sent()
                .unwrap(),
            Some(&original)
        );
        let limits = ImageLimits {
            max_width: Some(2),
            ..Default::default()
        };
        assert!(
            matches!(draft.prepare(ImageChoice::Auto, &target(), &limits), Err(InputError::InvalidImage(message)) if message.contains("discard frames"))
        );
    }
    #[test]
    fn invalid_inputs_and_impossible_limits_fail() {
        assert!(
            ImageRecord::new(Block::Image {
                media_type: "image/png".into(),
                data: "bad".into()
            })
            .is_err()
        );
        let draft = ImageRecord::new(picture(4, 2)).unwrap();
        let limits = ImageLimits {
            max_width: Some(0),
            ..Default::default()
        };
        assert!(matches!(
            draft.prepare(ImageChoice::Auto, &target(), &limits),
            Err(InputError::InvalidLimit(_))
        ));
        let limits = ImageLimits {
            max_image_bytes: Some(1),
            ..Default::default()
        };
        assert!(matches!(
            draft.prepare(ImageChoice::Auto, &target(), &limits),
            Err(InputError::LimitExceeded { .. })
        ));
    }
}
