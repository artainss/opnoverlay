//! Audio capture through GStreamer's PulseAudio source, which PipeWire also serves: desktop
//! audio is the monitor of an output, the microphone an input; 48 kHz stereo f32.

use std::error::Error;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::audio::{CHANNELS, Device, InputKind, InputShared, SAMPLE_RATE};

/// Desktop audio comes from the monitor of an output; anything else is an input.
fn is_monitor(device: &gst::Device) -> bool {
    device.properties().and_then(|p| p.get::<String>("device.class").ok()).as_deref() == Some("monitor")
}

/// The name `pulsesrc` knows the device by.
fn source_name(device: &gst::Device) -> Option<String> {
    let element = device.create_element(None).ok()?;
    element.find_property("device")?;
    element.property::<Option<String>>("device")
}

/// Devices for an input kind.
pub fn list_devices(kind: InputKind) -> Vec<Device> {
    if gst::init().is_err() || matches!(kind, InputKind::Application(_)) {
        return Vec::new();
    }
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Source"), None);
    if monitor.start().is_err() {
        return Vec::new();
    }
    let devices = monitor.devices();
    monitor.stop();
    devices
        .into_iter()
        .filter(|d| is_monitor(d) == (kind == InputKind::System))
        .filter_map(|d| Some(Device { id: source_name(&d)?, name: d.display_name().to_string() }))
        .collect()
}

/// Captures from `device_id`, or the default output's monitor / default input when `None`.
pub fn capture(kind: InputKind, device_id: Option<&str>, shared: &InputShared) -> Result<(), Box<dyn Error>> {
    gst::init()?;
    let device = match (kind, device_id) {
        (InputKind::Application(_) | InputKind::SystemExcept(_), _) => return Err(super::not_yet().into()),
        (_, Some(id)) => Some(id.to_owned()),
        (InputKind::System, None) => Some("@DEFAULT_MONITOR@".to_owned()),
        (InputKind::Microphone, None) => None,
    };
    let pipeline = gst::parse::launch(&format!(
        "pulsesrc name=src client-name=obs-lite ! audioconvert ! audioresample \
         ! audio/x-raw,format=F32LE,layout=interleaved,rate={SAMPLE_RATE},channels={CHANNELS} \
         ! appsink name=sink sync=false"
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "not a pipeline")?;
    if let (Some(device), Some(src)) = (device, pipeline.by_name("src")) {
        src.set_property("device", device);
    }
    let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("no sink")?;
    super::encoder::play(&pipeline)?;
    let bus = pipeline.bus().ok_or("no bus")?;
    let mut result = Ok(());
    while !shared.stopped() {
        if let Some(message) = bus.pop_filtered(&[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = message.view() {
                result = Err(e.error().to_string().into());
            }
            break;
        }
        match sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
            Some(sample) => {
                let Some(map) = sample.buffer().and_then(|b| b.map_readable().ok()) else { continue };
                shared.deliver(map.as_slice().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect());
            }
            None if sink.is_eos() => break,
            None => shared.silent(),
        }
    }
    let _ = pipeline.set_state(gst::State::Null);
    result
}
