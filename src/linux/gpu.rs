//! Frames that stay on the GPU: the desktop hands them over as DMA-BUFs, which VA-API converts
//! and encodes without the CPU touching their pixels. Only the preview (small) and screenshots
//! (once) are brought into memory.

use std::sync::OnceLock;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

use super::encoder::{play, posted_error};
use crate::i18n::tr;

/// The formats a desktop sends screens in: BGRA, BGRx, RGBA and RGBx.
const DESKTOP_FORMATS: [&str; 4] = ["AR24", "XR24", "AB24", "XB24"];

/// The DMA-BUF formats to ask the desktop for, as a caps list, if this machine can record frames
/// on the GPU (VA-API converts and encodes them): the desktop's formats in the layout VA-API reads.
pub fn formats() -> Option<String> {
    let encoder = ["vah264enc", "vah264lpenc"].iter().any(|f| gst::ElementFactory::find(f).is_some());
    let postproc = gst::ElementFactory::find("vapostproc").filter(|_| encoder)?;
    let sink = postproc.static_pad_templates().into_iter().find(|t| t.direction() == gst::PadDirection::Sink)?;
    let caps = sink.caps();
    let readable: Vec<String> = caps
        .iter_with_features()
        .filter(|(_, features)| features.contains("memory:DMABuf"))
        .flat_map(|(s, _)| match s.get::<gst::List>("drm-format") {
            Ok(list) => list.iter().filter_map(|v| v.get::<String>().ok()).collect::<Vec<_>>(),
            Err(_) => s.get::<String>("drm-format").into_iter().collect(),
        })
        .collect();
    offer(&readable)
}

/// Of the DMA-BUF formats VA-API reads (`readable`, "AR24:0x0100000000000002" or "AR24" for a
/// linear one), the desktop's, each also offered linear. COSMIC's portal fails when it is offered
/// a single layout; with two it picks the one it can make, which is VA-API's.
fn offer(readable: &[String]) -> Option<String> {
    let mut offered: Vec<&str> = Vec::new();
    for format in readable {
        let fourcc = format.split(':').next().unwrap_or(format);
        if !DESKTOP_FORMATS.contains(&fourcc) {
            continue;
        }
        for f in [format.as_str(), fourcc] {
            if !offered.contains(&f) {
                offered.push(f);
            }
        }
    }
    (!offered.is_empty()).then(|| format!("{{{}}}", offered.iter().map(|f| format!("(string){f}")).collect::<Vec<_>>().join(",")))
}

/// Makes `pipeline` use the same VA-API display as every other: Intel's driver crashed converting
/// frames while another display in the process was converting too (the preview's).
pub fn share_display(pipeline: &gst::Pipeline) {
    static DISPLAY: OnceLock<Option<gst::Context>> = OnceLock::new();
    let display = DISPLAY.get_or_init(|| {
        // A VA-API element opens the display when it gets ready, and keeps it as its context.
        let element = gst::ElementFactory::make("vapostproc").build().ok()?;
        element.set_state(gst::State::Ready).ok()?;
        let context = element.context("gst.va.display.handle");
        let _ = element.set_state(gst::State::Null);
        context
    });
    if let Some(display) = display {
        pipeline.set_context(display);
    }
}

/// Whether the frame is a DMA-BUF, which only the GPU reads.
pub fn on_gpu(sample: &gst::Sample) -> bool {
    sample.caps().and_then(|c| c.features(0)).is_some_and(|f| f.contains("memory:DMABuf"))
}

/// Scales pictures on the GPU down to RGBA pictures in memory, for the preview.
pub struct Scaler {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    size: gst::Element,
    scaled_to: Option<(u32, u32)>,
}

impl Scaler {
    /// `on_picture` gets each picture: (size, RGBA pixels).
    pub fn new(on_picture: impl Fn((u32, u32), Vec<u8>) + Send + Sync + 'static) -> Result<Self, String> {
        let (pipeline, src, size, sink) = to_memory()?;
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some((w, h, rgba)) = rgba(&sample) {
                        on_picture((w, h), rgba);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        play(&pipeline)?;
        Ok(Self { pipeline, src, size, scaled_to: None })
    }

    /// Scales `picture` to `to`; the result comes to `on_picture`.
    pub fn push(&mut self, picture: &gst::Sample, to: (u32, u32)) {
        if self.scaled_to != Some(to) {
            self.size.set_property("caps", rgba_caps(to));
            self.scaled_to = Some(to);
        }
        let _ = self.src.push_sample(picture);
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// A picture on the GPU as RGBA pixels in memory, at its size: (width, height, pixels).
pub fn download(picture: &gst::Sample) -> Result<(u32, u32, Vec<u8>), String> {
    let s = picture.caps().and_then(|c| c.structure(0)).ok_or("no caps")?;
    let size = (s.get::<i32>("width").map_err(|e| e.to_string())? as u32, s.get::<i32>("height").map_err(|e| e.to_string())? as u32);
    let (pipeline, src, filter, sink) = to_memory()?;
    filter.set_property("caps", rgba_caps(size));
    play(&pipeline)?;
    let _ = src.push_sample(picture);
    let _ = src.end_of_stream();
    let picture = sink.try_pull_sample(gst::ClockTime::from_seconds(5));
    let error = posted_error(&pipeline);
    let _ = pipeline.set_state(gst::State::Null);
    picture.as_ref().and_then(rgba).ok_or_else(|| {
        error.unwrap_or_else(|| tr!("la GPU no devolvió la imagen", "the GPU did not give the picture back").to_owned())
    })
}

/// A pipeline that brings frames on the GPU into memory: its source, the filter that sets the
/// size, and its sink.
fn to_memory() -> Result<(gst::Pipeline, gst_app::AppSrc, gst::Element, gst_app::AppSink), String> {
    let pipeline = gst::parse::launch(
        "appsrc name=src is-live=true format=time block=false max-bytes=0 max-buffers=2 leaky-type=downstream \
         ! vapostproc name=scale ! capsfilter name=size ! appsink name=sink max-buffers=1 drop=true sync=false",
    )
    .map_err(|e| e.to_string())?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "not a pipeline".to_owned())?;
    share_display(&pipeline);
    let by_name = |name: &str| pipeline.by_name(name).ok_or_else(|| format!("no element {name}"));
    let scale = by_name("scale")?;
    // Text stays legible in a scaled-down preview (not every driver has the choice).
    if scale.has_property("scale-method") {
        scale.set_property_from_str("scale-method", "hq");
    }
    let src = by_name("src")?.downcast::<gst_app::AppSrc>().map_err(|_| "no appsrc")?;
    let sink = by_name("sink")?.downcast::<gst_app::AppSink>().map_err(|_| "no appsink")?;
    let size = by_name("size")?;
    Ok((pipeline, src, size, sink))
}

fn rgba_caps((width, height): (u32, u32)) -> gst::Caps {
    gst::Caps::builder("video/x-raw").field("format", "RGBA").field("width", width as i32).field("height", height as i32).build()
}

/// The RGBA picture in `sample`, packed and opaque: (width, height, pixels).
pub(super) fn rgba(sample: &gst::Sample) -> Option<(u32, u32, Vec<u8>)> {
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer()?, &info).ok()?;
    let (w, h) = (info.width(), info.height());
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).ok()?;
    let mut pixels = Vec::with_capacity(w as usize * h as usize * 4);
    for row in data.chunks(stride).take(h as usize) {
        pixels.extend_from_slice(row.get(..w as usize * 4)?);
    }
    for px in pixels.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }
    Some((w, h, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktops_formats_are_offered_in_va_apis_layout_and_linear() {
        let readable = ["NV12:0x0100000000000002", "AR24:0x0100000000000002", "YU12", "XB24", "AB24:0x0100000000000002"];
        let readable: Vec<String> = readable.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            offer(&readable).unwrap(),
            "{(string)AR24:0x0100000000000002,(string)AR24,(string)XB24,(string)AB24:0x0100000000000002,(string)AB24}"
        );
        assert_eq!(offer(&["NV12:0x0100000000000002".to_owned()]), None, "no RGB format: nothing to ask for");
    }
}
