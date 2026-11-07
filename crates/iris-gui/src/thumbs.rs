//! Keyframe thumbnails for the timeline: the daemon's keyframe resource (an
//! init segment plus one single-sample fragment) decoded to an RGB image by
//! a short GStreamer pipeline, off the main loop.

use anyhow::Context;
use gst::prelude::*;
use gtk::gdk;
use gtk::glib;

/// Pipeline for one keyframe: the bytes go in through `appsrc`, the one
/// decoded frame comes out of `appsink` as packed RGB.
const PIPELINE: &str = "appsrc name=src ! qtdemux ! decodebin3 ! videoconvert \
    ! video/x-raw,format=RGB ! appsink name=sink sync=false";
/// Decoding a single frame must not take longer than this.
const DECODE_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(5);
/// Largest decoded width or height accepted for a timeline thumbnail.
const MAX_THUMBNAIL_DIMENSION: u32 = 4096;
/// Largest packed RGB frame copied out of the decoder.
const MAX_THUMBNAIL_RGB_BYTES: usize = 64 << 20;

/// One decoded frame.
#[derive(Clone, Debug)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes per row.
    pub stride: usize,
    /// Packed RGB rows.
    pub rgb: Vec<u8>,
}

impl Image {
    /// The frame as a GTK texture; main loop only.
    pub fn texture(&self) -> gdk::Texture {
        let bytes = glib::Bytes::from(&self.rgb);
        gdk::MemoryTexture::new(
            i32::try_from(self.width).unwrap_or(i32::MAX),
            i32::try_from(self.height).unwrap_or(i32::MAX),
            gdk::MemoryFormat::R8g8b8,
            &bytes,
            self.stride,
        )
        .upcast()
    }
}

/// Decodes a keyframe resource to an image. Blocks for the decode, so call it
/// from a worker thread.
pub fn decode(keyframe: Vec<u8>) -> anyhow::Result<Image> {
    let pipeline = gst::parse::launch(PIPELINE)
        .context("building the thumbnail pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("not a pipeline"))?;
    let src = pipeline
        .by_name("src")
        .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
        .context("appsrc missing")?;
    let sink = pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .context("appsink missing")?;
    pipeline
        .set_state(gst::State::Playing)
        .context("starting the thumbnail pipeline")?;
    let result = (|| {
        src.push_buffer(gst::Buffer::from_slice(keyframe))
            .map_err(|flow| anyhow::anyhow!("appsrc rejected the keyframe: {flow:?}"))?;
        src.end_of_stream()
            .map_err(|flow| anyhow::anyhow!("appsrc rejected end of stream: {flow:?}"))?;
        let sample = sink
            .try_pull_sample(DECODE_TIMEOUT)
            .context("no frame decoded in time")?;
        let caps = sample.caps().context("sample without caps")?;
        let info = gst_video::VideoInfo::from_caps(caps).context("unreadable caps")?;
        let buffer = sample.buffer().context("sample without a buffer")?;
        let map = buffer.map_readable().context("unmappable frame")?;
        let stride = usize::try_from(info.stride()[0]).context("negative stride")?;
        let rgb_len = validated_rgb_len(info.width(), info.height(), stride, map.size())?;
        Ok(Image {
            width: info.width(),
            height: info.height(),
            stride,
            rgb: map.as_slice()[..rgb_len].to_vec(),
        })
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

fn validated_rgb_len(
    width: u32,
    height: u32,
    stride: usize,
    mapped_len: usize,
) -> anyhow::Result<usize> {
    anyhow::ensure!(
        (1..=MAX_THUMBNAIL_DIMENSION).contains(&width)
            && (1..=MAX_THUMBNAIL_DIMENSION).contains(&height),
        "thumbnail dimensions are outside the supported range"
    );
    let row_bytes = usize::try_from(width)
        .context("thumbnail width does not fit memory")?
        .checked_mul(3)
        .context("thumbnail row size overflows")?;
    anyhow::ensure!(stride >= row_bytes, "thumbnail stride is too small");
    let rgb_len = stride
        .checked_mul(usize::try_from(height).context("thumbnail height does not fit memory")?)
        .context("thumbnail image size overflows")?;
    anyhow::ensure!(
        rgb_len <= MAX_THUMBNAIL_RGB_BYTES,
        "thumbnail image is too large"
    );
    anyhow::ensure!(mapped_len >= rgb_len, "thumbnail buffer is truncated");
    Ok(rgb_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_thumbnail_geometry_and_rgb_budget() {
        assert_eq!(validated_rgb_len(1, 1, 3, 3).unwrap(), 3);
        assert!(validated_rgb_len(0, 1, 3, 3).is_err());
        assert!(validated_rgb_len(1, 0, 3, 3).is_err());
        assert!(validated_rgb_len(MAX_THUMBNAIL_DIMENSION + 1, 1, 3, 3).is_err());
        assert!(validated_rgb_len(2, 1, 5, 6).is_err());
        assert!(validated_rgb_len(2, 2, 6, 11).is_err());

        let exact_stride =
            MAX_THUMBNAIL_RGB_BYTES / usize::try_from(MAX_THUMBNAIL_DIMENSION).unwrap();
        assert_eq!(
            validated_rgb_len(
                MAX_THUMBNAIL_DIMENSION,
                MAX_THUMBNAIL_DIMENSION,
                exact_stride,
                MAX_THUMBNAIL_RGB_BYTES,
            )
            .unwrap(),
            MAX_THUMBNAIL_RGB_BYTES
        );
        assert!(
            validated_rgb_len(
                MAX_THUMBNAIL_DIMENSION,
                MAX_THUMBNAIL_DIMENSION,
                exact_stride + 1,
                MAX_THUMBNAIL_RGB_BYTES + MAX_THUMBNAIL_DIMENSION as usize,
            )
            .is_err()
        );
    }
}
