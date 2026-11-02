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
pub fn decode(keyframe: &[u8]) -> anyhow::Result<Image> {
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
        src.push_buffer(gst::Buffer::from_slice(keyframe.to_vec()))
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
        Ok(Image {
            width: info.width(),
            height: info.height(),
            stride: usize::try_from(info.stride()[0]).context("negative stride")?,
            rgb: map.as_slice().to_vec(),
        })
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}
