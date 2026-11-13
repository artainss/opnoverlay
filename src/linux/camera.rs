//! Webcam capture through the Camera portal: it grants access (asking the user if needed) and
//! hands over a PipeWire connection that only shows cameras, read with GStreamer.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

use ashpd::desktop::camera::Camera;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

use crate::camera::{CameraDevice, CameraFrame, CameraShared};
use crate::i18n::tr;

/// Set once the user has let obs-lite use the cameras in this run.
static GRANTED: AtomicBool = AtomicBool::new(false);

/// Access to the cameras (asking the user if needed), or `None` if there is none.
fn open() -> Result<Option<OwnedFd>, String> {
    pollster::block_on(async {
        let camera = Camera::new().await?;
        camera.request_access(Default::default()).await?.response()?;
        GRANTED.store(true, Ordering::Relaxed);
        if !camera.is_present().await? {
            return Ok(None);
        }
        Ok(Some(camera.open_pipe_wire_remote(Default::default()).await?))
    })
    .map_err(access_error)
}

/// What to tell the user when the portal gives no access. It answers "cancelled" both when
/// the user denies access and when the desktop refuses to ask (GNOME only lets the focused
/// app ask, so a camera started while another dialog is open gets no answer).
fn access_error(e: ashpd::Error) -> String {
    match e {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => tr!(
            "no se concedió acceso a la cámara; elígela de nuevo o permítelo en los ajustes de privacidad del sistema",
            "camera access was not granted; choose the camera again, or allow it in the system's privacy settings"
        )
        .to_owned(),
        e => e.to_string(),
    }
}

/// Whether there is a camera, without asking the user for access.
fn present() -> bool {
    pollster::block_on(async { Camera::new().await?.is_present().await }).unwrap_or(false)
}

/// Cameras on a portal connection, identified by their PipeWire node name.
fn devices_on(fd: &OwnedFd) -> Vec<CameraDevice> {
    let Some(provider) = gst::DeviceProviderFactory::by_name("pipewiredeviceprovider") else { return Vec::new() };
    provider.set_property("fd", fd.as_raw_fd());
    if provider.start().is_err() {
        return Vec::new();
    }
    let devices = provider.devices();
    provider.stop();
    // The provider may report a camera more than once.
    let mut cameras: Vec<CameraDevice> = Vec::new();
    for device in devices.iter().filter(|d| d.has_classes("Video/Source")) {
        let Some(node) = device.properties().and_then(|p| p.get::<String>("node.name").ok()) else { continue };
        if !cameras.iter().any(|c| c.id == node) {
            cameras.push(CameraDevice { id: node, name: device.display_name().to_string() });
        }
    }
    cameras
}

/// Video capture devices. Until the user grants access, which the portal asks only when a
/// camera is chosen, this is a single entry for the default camera (id "").
pub fn list_devices() -> Vec<CameraDevice> {
    if gst::init().is_err() {
        return Vec::new();
    }
    if !GRANTED.load(Ordering::Relaxed) {
        let default = CameraDevice { id: String::new(), name: tr!("Cámara", "Camera").to_owned() };
        return if present() { vec![default] } else { Vec::new() };
    }
    match open() {
        Ok(Some(fd)) => devices_on(&fd),
        _ => Vec::new(),
    }
}

/// Captures from `device` (the default camera if empty) until asked to stop, handing every
/// picture to `shared`.
pub fn capture(device: &str, shared: &CameraShared, repaint: Option<&eframe::egui::Context>) -> Result<(), String> {
    gst::init().map_err(|e| e.to_string())?;
    let fd = open()?.ok_or_else(|| tr!("cámara no encontrada", "camera not found").to_owned())?;
    let pipeline = gst::parse::launch(&format!(
        // An unplugged camera ends the stream (otherwise the last picture would stay, frozen).
        "pipewiresrc name=src fd={fd} on-disconnect=error ! videoconvert ! video/x-raw,format=BGRA ! appsink name=sink max-buffers=1 drop=true sync=false",
        fd = fd.as_raw_fd()
    ))
    .map_err(|e| e.to_string())?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "not a pipeline".to_owned())?;
    if let (Some(src), false) = (pipeline.by_name("src"), device.is_empty()) {
        src.set_property("target-object", device);
    }
    let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("no sink")?;
    super::encoder::play(&pipeline)?;
    let bus = pipeline.bus().ok_or("no bus")?;
    let mut result = Ok(());
    while !shared.stopped() {
        if let Some(message) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Eos]) {
            if let gst::MessageView::Error(e) = message.view() {
                result = Err(e.error().to_string());
            }
            break;
        }
        let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) else { continue };
        if let Some(frame) = to_frame(&sample) {
            shared.deliver(frame, repaint);
        }
    }
    // Releases the camera (its light goes off).
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// A BGRA sample as a tightly packed camera picture.
fn to_frame(sample: &gst::Sample) -> Option<CameraFrame> {
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer()?, &info).ok()?;
    let (width, height) = (info.width(), info.height());
    let bgra = pack(frame.plane_data(0).ok()?, frame.plane_stride()[0] as usize, width, height)?;
    Some(CameraFrame { width, height, bgra })
}

/// Drops the padding at the end of each row of a 4-byte-per-pixel picture.
fn pack(data: &[u8], stride: usize, width: u32, height: u32) -> Option<Vec<u8>> {
    let row = width as usize * 4;
    if stride < row || data.len() < stride * (height as usize).saturating_sub(1) + row {
        return None;
    }
    Some(data.chunks(stride).take(height as usize).flat_map(|r| &r[..row]).copied().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_access_says_how_to_allow_it() {
        let message = access_error(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled));
        assert!(message.contains("privacy") || message.contains("privacidad"), "{message}");
    }

    #[test]
    fn row_padding_is_dropped() {
        // 2x2 pixels, rows padded to 12 bytes; the last row may come without its padding.
        let data = [[1u8; 8].as_slice(), &[0; 4], &[2; 8]].concat();
        assert_eq!(pack(&data, 12, 2, 2), Some([[1u8; 8], [2; 8]].concat()));
        assert_eq!(pack(&data, 8, 3, 2), None);
        assert_eq!(pack(&data[..15], 12, 2, 2), None);
    }
}
