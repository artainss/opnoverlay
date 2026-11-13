//! Media Foundation pieces of the encoder: colour conversion on the GPU, the H.264 and AAC
//! encoders and the MP4 writer. Encoded data travels as `Packet`s, so the same packets can be
//! written to a file or kept in memory for the replay buffer.

use std::cell::Cell;
use std::mem::ManuallyDrop;
use std::path::Path;

use windows::Win32::Foundation::{E_FAIL, RECT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_VIDEO_ENCODER, D3D11_BOX, D3D11_VIDEO_COLOR,
    D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA, ID3D11DeviceContext, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device,
    ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer, IMFActivate,
    IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample, IMFSinkWriter, IMFTransform,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NONE, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
    MF_SINK_WRITER_DISABLE_THROTTLING, MF_TRANSCODE_CONTAINERTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MFAudioFormat_AAC,
    MFAudioFormat_PCM, MFCreateAttributes, MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFCreateSinkWriterFromURL, MFMediaType_Audio, MFMediaType_Video,
    MFNominalRange_16_235, MFSampleExtension_CleanPoint, MFT_CATEGORY_AUDIO_ENCODER, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup, MFTEnum2,
    MFTEnumEx,
    MFTranscodeContainerType_FMPEG4, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709, eAVEncCommonRateControlMode_UnconstrainedVBR, eAVEncH264VProfile_High,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, HSTRING, Interface, Result};

use crate::media::{Packet, TICKS_PER_SECOND};

pub const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u32 = 2;

/// Keeps Media Foundation started while it lives (start-up is reference counted), so a file
/// can still be finished after the encoder that made it is gone.
pub struct Media(());

impl Media {
    pub fn start() -> Result<Self> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };
        Ok(Self(()))
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

// ------------------------------------------------------------------ colour conversion

/// A GPU video processor from `input`-sized to `output`-sized pictures.
struct Processor {
    video_device: ID3D11VideoDevice,
    context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

fn full_rect((w, h): (u32, u32)) -> RECT {
    RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 }
}

impl Processor {
    fn new(device: &ID3D11Device, input: (u32, u32), output: (u32, u32)) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast()?;
            let context: ID3D11VideoContext = device.GetImmediateContext()?.cast()?;
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: output.0,
                OutputHeight: output.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&full_rect(input)));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&full_rect(output)));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full_rect(output)));
            Ok(Self { video_device, context, enumerator, processor })
        }
    }

    fn input_view(&self, texture: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut view = None;
        unsafe { self.video_device.CreateVideoProcessorInputView(texture, &self.enumerator, &desc, Some(&mut view))? };
        Ok(view.expect("CreateVideoProcessorInputView returned no view"))
    }

    fn output_view(&self, texture: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorOutputView> {
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut view = None;
        unsafe { self.video_device.CreateVideoProcessorOutputView(texture, &self.enumerator, &desc, Some(&mut view))? };
        Ok(view.expect("CreateVideoProcessorOutputView returned no view"))
    }

    fn blit(&self, input: &ID3D11VideoProcessorInputView, output: &ID3D11VideoProcessorOutputView) -> Result<()> {
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let result = unsafe { self.context.VideoProcessorBlt(&self.processor, output, 0, std::slice::from_ref(&stream)) };
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        result
    }
}

/// Converts the BGRA canvas into NV12 textures at the output size, on the GPU.
pub struct Converter {
    processor: Processor,
    input: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
}

impl Converter {
    pub fn new(device: &ID3D11Device, canvas: &ID3D11Texture2D, input: (u32, u32), output: (u32, u32), count: usize) -> Result<Self> {
        let processor = Processor::new(device, input, output)?;
        // Full-range RGB in; BT.709 limited-range YCbCr out (what players expect for HD).
        let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        let bt709_limited = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
        unsafe {
            processor.context.VideoProcessorSetStreamColorSpace(&processor.processor, 0, &rgb_full);
            processor.context.VideoProcessorSetOutputColorSpace(&processor.processor, &bt709_limited);
        }
        let input = processor.input_view(canvas)?;
        let mut outputs = Vec::with_capacity(count);
        for _ in 0..count {
            let texture = nv12_texture(device, output)?;
            let view = processor.output_view(&texture)?;
            outputs.push((texture, view));
        }
        Ok(Self { processor, input, outputs })
    }

    /// Converts the canvas into output texture `slot`.
    pub fn convert(&self, slot: usize) -> Result<()> {
        self.processor.blit(&self.input, &self.outputs[slot].1)
    }

    pub fn texture(&self, slot: usize) -> &ID3D11Texture2D {
        &self.outputs[slot].0
    }
}

/// Draws a picture of another size onto the canvas, scaled down to fit (never up), centred,
/// with black around it: a window resized while it is recorded.
pub struct Scaler {
    processor: Processor,
    /// The picture is copied here first: capture textures cannot be read by the video processor.
    copy: ID3D11Texture2D,
    input: ID3D11VideoProcessorInputView,
    output: ID3D11VideoProcessorOutputView,
    pub size: (u32, u32),
}

impl Scaler {
    pub fn new(device: &ID3D11Device, canvas: &ID3D11Texture2D, size: (u32, u32), canvas_size: (u32, u32), placed: RECT) -> Result<Self> {
        let processor = Processor::new(device, size, canvas_size)?;
        let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
        unsafe {
            let (context, p) = (&processor.context, &processor.processor);
            context.VideoProcessorSetStreamColorSpace(p, 0, &rgb_full);
            context.VideoProcessorSetOutputColorSpace(p, &rgb_full);
            context.VideoProcessorSetStreamDestRect(p, 0, true, Some(&placed));
            context.VideoProcessorSetOutputBackgroundColor(p, false, &black);
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut copy = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut copy))? };
        let copy = copy.expect("CreateTexture2D returned no texture");
        let input = processor.input_view(&copy)?;
        let output = processor.output_view(canvas)?;
        Ok(Self { processor, copy, input, output, size })
    }

    /// Draws `source`, a `size`-sized picture, onto the canvas.
    pub fn draw(&self, context: &ID3D11DeviceContext, source: &ID3D11Texture2D) -> Result<()> {
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: self.size.0, bottom: self.size.1, back: 1 };
        unsafe { context.CopySubresourceRegion(&self.copy, 0, 0, 0, 0, source, 0, Some(&region)) };
        self.processor.blit(&self.input, &self.output)
    }
}

fn nv12_texture(device: &ID3D11Device, (w, h): (u32, u32)) -> Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    // Older drivers do not know the video-encoder bind flag.
    if unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.is_err() {
        desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    }
    Ok(texture.expect("CreateTexture2D returned no texture"))
}

// ------------------------------------------------------------------ H.264

/// What the video encoder asks for next.
#[derive(Clone, Copy)]
pub enum VideoEvent {
    NeedInput,
    HaveOutput,
    Drained,
    Other,
}

/// An H.264 encoder: a hardware one on the GPU the frames live on (asynchronous MFT) or,
/// without one, the Windows software encoder (synchronous MFT).
pub struct H264 {
    mft: IMFTransform,
    /// `None` for the software encoder, which is driven by `next` instead.
    events: Option<IMFMediaEventGenerator>,
    next: Cell<VideoEvent>,
    draining: Cell<bool>,
    codec: Option<ICodecAPI>,
    provides_samples: bool,
    _manager: Option<IMFDXGIDeviceManager>,
    pub name: String,
    /// Last: shuts the encoder down once everything above is released.
    _activated: Activated,
}

/// An encoder activated from its `IMFActivate`, shut down when dropped. Releasing the
/// encoder's interfaces is not enough: until `ShutdownObject`, a hardware encoder keeps the D3D
/// device it was given alive, with every texture made on it (each 1080p recording left about
/// 120 MB behind).
struct Activated(IMFActivate);

impl Drop for Activated {
    fn drop(&mut self) {
        let _ = unsafe { self.0.ShutdownObject() };
    }
}

impl H264 {
    pub fn new(device: &ID3D11Device, size: (u32, u32), fps: u32, bitrate: u32) -> std::result::Result<Self, String> {
        let hardware = unsafe { hardware_encoders(device) }.unwrap_or_default();
        let software = unsafe { software_encoders() }.map_err(|e| e.to_string())?;
        let mut last_error = "no H.264 encoder".to_owned();
        let candidates = hardware.into_iter().map(|a| (a, true)).chain(software.into_iter().map(|a| (a, false)));
        for (activate, hardware) in candidates {
            match unsafe { Self::open(device, &activate, hardware, size, fps, bitrate) } {
                Ok(encoder) => return Ok(encoder),
                Err(e) => last_error = e.to_string(),
            }
        }
        Err(last_error)
    }

    unsafe fn open(
        device: &ID3D11Device,
        activate: &IMFActivate,
        hardware: bool,
        size: (u32, u32),
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        unsafe {
            let name = string_attribute(activate, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_else(|| "H.264 (software)".to_owned());
            let mft: IMFTransform = activate.ActivateObject()?;
            // Shuts it down also if setting it up fails below.
            let activated = Activated(activate.clone());
            let manager = if hardware {
                mft.GetAttributes()?.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                let mut token = 0;
                let mut manager = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                let manager = manager.expect("MFCreateDXGIDeviceManager returned no manager");
                manager.ResetDevice(device, token)?;
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
                Some(manager)
            } else {
                None
            };

            // Not every encoder supports every setting; what it rejects keeps its default.
            let codec = mft.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                let set = |key: &GUID, value: u32| codec.SetValue(key, &VARIANT::from(value));
                let _ = set(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32);
                let _ = set(&CODECAPI_AVEncCommonMeanBitRate, bitrate);
                // A keyframe every 2 s: where a replay or a new recording can start.
                let _ = set(&CODECAPI_AVEncMPVGOPSize, fps * 2);
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(size.0, size.1))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            // Signalled in the stream, so players do not have to guess the colours.
            output.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            output.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
            output.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
            output.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            mft.SetOutputType(0, &output, 0)?;

            let mut input = None;
            for i in 0.. {
                let Ok(candidate) = mft.GetInputAvailableType(0, i) else { break };
                if candidate.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    input = Some(candidate);
                    break;
                }
            }
            let input = input.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(size.0, size.1))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            mft.SetInputType(0, &input, 0)?;

            let provides_samples = mft.GetOutputStreamInfo(0)?.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self {
                events: if hardware { Some(mft.cast()?) } else { None },
                next: Cell::new(VideoEvent::NeedInput),
                draining: Cell::new(false),
                codec,
                mft,
                provides_samples,
                _manager: manager,
                name,
                _activated: activated,
            })
        }
    }

    /// Blocks until the encoder asks for something. An encoder that fails (a GPU reset, a driver
    /// error) says so with `MEError` or a failed status, and may send nothing after it: that is
    /// an error, not an event to wait past.
    pub fn next_event(&self) -> Result<VideoEvent> {
        use windows::Win32::Media::MediaFoundation::{
            MEError, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
        };
        let Some(events) = &self.events else { return Ok(self.next.get()) };
        let event = unsafe { events.GetEvent(MF_EVENT_FLAG_NONE)? };
        unsafe { event.GetStatus()? }.ok()?;
        Ok(match unsafe { event.GetType()? } {
            t if t == MEError.0 as u32 => return Err(E_FAIL.into()),
            t if t == METransformNeedInput.0 as u32 => VideoEvent::NeedInput,
            t if t == METransformHaveOutput.0 as u32 => VideoEvent::HaveOutput,
            t if t == METransformDrainComplete.0 as u32 => VideoEvent::Drained,
            _ => VideoEvent::Other,
        })
    }

    /// Hands an NV12 frame to the encoder (only after a `NeedInput` event).
    pub fn push(&self, texture: &ID3D11Texture2D, time: i64, duration: i64) -> Result<()> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            buffer.SetCurrentLength(buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.mft.ProcessInput(0, &sample, 0)?;
        }
        self.next.set(VideoEvent::HaveOutput);
        Ok(())
    }

    /// The encoded frame (only after a `HaveOutput` event); `None` if there is none yet.
    pub fn pull(&self) -> Result<Option<Packet>> {
        match unsafe { pull(&self.mft, self.provides_samples)? } {
            Pulled::Packet(packet) => Ok(Some(packet)),
            Pulled::FormatChanged => Ok(None),
            Pulled::NeedInput => {
                self.next.set(if self.draining.get() { VideoEvent::Drained } else { VideoEvent::NeedInput });
                Ok(None)
            }
        }
    }

    /// Makes the next frame a keyframe, so a recording can start right away.
    pub fn force_keyframe(&self) {
        if let Some(codec) = &self.codec {
            let _ = unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
        }
    }

    /// Asks for the remaining frames; a `Drained` event follows them.
    pub fn drain(&self) -> Result<()> {
        unsafe {
            self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        self.draining.set(true);
        self.next.set(VideoEvent::HaveOutput);
        Ok(())
    }

    /// The stream description for the MP4 writer, with the codec parameters once known.
    pub fn output_type(&self) -> Result<IMFMediaType> {
        unsafe { self.mft.GetOutputCurrentType(0) }
    }
}

unsafe fn hardware_encoders(device: &ID3D11Device) -> Result<Vec<IMFActivate>> {
    unsafe {
        // Encoders of the GPU the device (and so every frame) lives on.
        let desc = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()?;
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.expect("MFCreateAttributes returned nothing");
        let luid = std::slice::from_raw_parts((&raw const desc.AdapterLuid).cast::<u8>(), size_of_val(&desc.AdapterLuid));
        attributes.SetBlob(&MFT_ENUM_ADAPTER_LUID, luid)?;
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut array = std::ptr::null_mut();
        let mut count = 0;
        let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
        MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &attributes, &mut array, &mut count)?;
        Ok(take_activates(array, count))
    }
}

unsafe fn software_encoders() -> Result<Vec<IMFActivate>> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut array = std::ptr::null_mut();
        let mut count = 0;
        let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
        MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut array, &mut count)?;
        Ok(take_activates(array, count))
    }
}

unsafe fn take_activates(array: *mut Option<IMFActivate>, count: u32) -> Vec<IMFActivate> {
    if array.is_null() {
        return Vec::new();
    }
    let list = unsafe { std::slice::from_raw_parts_mut(array, count as usize) }.iter_mut().filter_map(Option::take).collect();
    unsafe { CoTaskMemFree(Some(array as *const _)) };
    list
}

fn string_attribute(activate: &IMFActivate, key: &GUID) -> Option<String> {
    unsafe {
        let mut value = windows::core::PWSTR::null();
        let mut len = 0;
        activate.GetAllocatedString(key, &mut value, &mut len).ok()?;
        let text = value.to_string().ok();
        CoTaskMemFree(Some(value.0 as *const _));
        text
    }
}

enum Pulled {
    Packet(Packet),
    NeedInput,
    FormatChanged,
}

/// One `ProcessOutput` call.
unsafe fn pull(mft: &IMFTransform, provides_samples: bool) -> Result<Pulled> {
    unsafe {
        let sample = if provides_samples {
            None
        } else {
            let size = mft.GetOutputStreamInfo(0)?.cbSize.max(1);
            let sample = MFCreateSample()?;
            sample.AddBuffer(&MFCreateMemoryBuffer(size)?)?;
            Some(sample)
        };
        let mut output = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = mft.ProcessOutput(0, &mut output, &mut status);
        let sample = ManuallyDrop::take(&mut output[0].pSample);
        drop(ManuallyDrop::take(&mut output[0].pEvents));
        match result {
            Ok(()) => match sample {
                Some(sample) => Ok(Pulled::Packet(to_packet(&sample)?)),
                None => Ok(Pulled::NeedInput),
            },
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Pulled::NeedInput),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let format = mft.GetOutputAvailableType(0, 0)?;
                mft.SetOutputType(0, &format, 0)?;
                Ok(Pulled::FormatChanged)
            }
            Err(e) => Err(e),
        }
    }
}

fn to_packet(sample: &IMFSample) -> Result<Packet> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut len = 0;
        buffer.Lock(&mut data, None, Some(&mut len))?;
        let bytes = std::slice::from_raw_parts(data, len as usize).to_vec();
        buffer.Unlock()?;
        Ok(Packet {
            data: bytes.into(),
            time: sample.GetSampleTime()?,
            duration: sample.GetSampleDuration().unwrap_or(0),
            key: sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0,
        })
    }
}

// ------------------------------------------------------------------ AAC

/// The Windows AAC encoder: 48 kHz stereo 16-bit PCM in, 192 kbit/s AAC-LC out.
pub struct Aac {
    mft: IMFTransform,
    frames: u64,
    _activated: Activated,
}

impl Aac {
    pub fn new() -> Result<Self> {
        unsafe {
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_PCM };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
            let mut array = std::ptr::null_mut();
            let mut count = 0;
            let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
            MFTEnumEx(MFT_CATEGORY_AUDIO_ENCODER, flags, Some(&input), Some(&output), &mut array, &mut count)?;
            let activate = take_activates(array, count).into_iter().next();
            let activate = activate.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))?;
            let mft: IMFTransform = activate.ActivateObject()?;
            let activated = Activated(activate);

            let audio = |subtype: &GUID| -> Result<IMFMediaType> {
                let t = MFCreateMediaType()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
                t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS)?;
                Ok(t)
            };
            let aac = audio(&MFAudioFormat_AAC)?;
            aac.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 192_000 / 8)?;
            aac.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            mft.SetOutputType(0, &aac, 0)?;
            let pcm = audio(&MFAudioFormat_PCM)?;
            pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, AUDIO_CHANNELS * 2)?;
            pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * AUDIO_CHANNELS * 2)?;
            mft.SetInputType(0, &pcm, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { mft, frames: 0, _activated: activated })
        }
    }

    /// Encodes interleaved 16-bit PCM; timestamps follow the samples given so far.
    pub fn encode(&mut self, pcm: &[u8]) -> Result<Vec<Packet>> {
        let frames = pcm.len() as u64 / (AUDIO_CHANNELS as u64 * 2);
        unsafe {
            let buffer = MFCreateMemoryBuffer(pcm.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr(), data, pcm.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(pcm.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.frames as i64 * TICKS_PER_SECOND / AUDIO_RATE as i64)?;
            sample.SetSampleDuration(frames as i64 * TICKS_PER_SECOND / AUDIO_RATE as i64)?;
            self.frames += frames;
            self.mft.ProcessInput(0, &sample, 0)?;
        }
        self.collect()
    }

    /// The audio still inside the encoder.
    pub fn drain(&mut self) -> Result<Vec<Packet>> {
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)? };
        self.collect()
    }

    fn collect(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        loop {
            match unsafe { pull(&self.mft, false)? } {
                Pulled::Packet(packet) => packets.push(Packet { key: true, ..packet }),
                Pulled::FormatChanged => {}
                Pulled::NeedInput => return Ok(packets),
            }
        }
    }

    pub fn output_type(&self) -> Result<IMFMediaType> {
        unsafe { self.mft.GetOutputCurrentType(0) }
    }
}

// ------------------------------------------------------------------ MP4

/// Writes already encoded packets into an MP4 file. Times are shifted so the file starts at
/// `start` (a video keyframe). The file is fragmented while it is written, so it plays up to its
/// last fragment (about 0.3 s) if the process dies, and becomes a regular MP4 when finished.
pub struct Mp4 {
    writer: IMFSinkWriter,
    path: std::path::PathBuf,
    audio: bool,
    start: i64,
    _media: Media,
}

impl Mp4 {
    pub fn create(path: &Path, video: &IMFMediaType, audio: Option<&IMFMediaType>, start: i64) -> Result<Self> {
        let media = Media::start()?;
        unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 3)?;
            let attributes = attributes.expect("MFCreateAttributes returned nothing");
            attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_FMPEG4)?;
            attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0)?;
            attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1)?;
            let path = std::path::absolute(path).map_err(|_| windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG))?;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attributes)?;
            // Same type in and out: the writer only packages what is already encoded.
            let video_stream = writer.AddStream(video)?;
            writer.SetInputMediaType(video_stream, video, None)?;
            if let Some(audio) = audio {
                let audio_stream = writer.AddStream(audio)?;
                writer.SetInputMediaType(audio_stream, audio, None)?;
            }
            writer.BeginWriting()?;
            Ok(Self { writer, path, audio: audio.is_some(), start, _media: media })
        }
    }

    /// Writes a packet; audio from before the file's start is skipped.
    pub fn write(&self, video: bool, packet: &Packet) -> Result<()> {
        let time = packet.time - self.start;
        if time < 0 || (!video && !self.audio) {
            return Ok(());
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(packet.data.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(packet.data.as_ptr(), data, packet.data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(packet.data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(packet.duration)?;
            if packet.key {
                sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
            }
            self.writer.WriteSample(if video { 0 } else { 1 }, &sample)
        }
    }

    pub fn finish(self) -> Result<()> {
        unsafe { self.writer.Finalize() }?;
        // If this fails the file stays fragmented, which still plays.
        let _ = crate::mp4::defragment(&self.path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
        D3D11CreateDevice,
    };
    use windows::Win32::Graphics::Dxgi::{DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, IDXGIAdapter3};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    #[test]
    #[ignore]
    fn decode_captured_video_from_file() {
        use windows::core::HSTRING;
        use windows::Win32::Media::MediaFoundation::{MFCreateSourceReaderFromURL, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM};
        let path = std::env::var_os("OPNOVERLAY_CHECK_FILE").expect("OPNOVERLAY_CHECK_FILE is required");
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _media = Media::start().unwrap();
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path), None).unwrap();
            let output = MFCreateMediaType().unwrap();
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).unwrap();
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader.SetCurrentMediaType(stream, None, &output).unwrap();
            let mut frames = 0usize;
            let mut previous = -1i64;
            let mut luma_levels = std::collections::HashSet::new();
            let mut first_luma = Vec::new();
            let mut last_luma = Vec::new();
            let mut max_changed = 0usize;
            let check_changes = std::env::var_os("OPNOVERLAY_CHECK_VIDEO_CHANGES").is_some();
            loop {
                let (mut flags, mut timestamp, mut sample) = (0u32, 0i64, None);
                reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut timestamp), Some(&mut sample)).unwrap();
                if let Some(sample) = sample {
                    assert!(timestamp >= previous);
                    assert!(sample.GetTotalLength().unwrap() > 0);
                    if check_changes || (frames == 0 && std::env::var_os("OPNOVERLAY_CHECK_VIDEO_CONTENT").is_some()) {
                        let buffer = sample.ConvertToContiguousBuffer().unwrap();
                        let (mut data, mut len) = (std::ptr::null_mut(), 0);
                        buffer.Lock(&mut data, None, Some(&mut len)).unwrap();
                        let samples: Vec<u8> = std::slice::from_raw_parts(data, len as usize * 2 / 3).iter().step_by(64).copied().collect();
                        if frames == 0 {
                            luma_levels.extend(samples.iter().copied());
                            first_luma.clone_from(&samples);
                        } else {
                            let changed = first_luma.iter().zip(&samples).filter(|(first, current)| first.abs_diff(**current) > 20).count();
                            max_changed = max_changed.max(changed);
                        }
                        last_luma = samples;
                        buffer.Unlock().unwrap();
                    }
                    previous = timestamp;
                    frames += 1;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 { break; }
            }
            assert!(frames >= 30, "only {frames} frames decoded");
            if std::env::var_os("OPNOVERLAY_CHECK_VIDEO_CONTENT").is_some() {
                assert!(luma_levels.len() >= 8, "first frame appears blank: {} luminance levels", luma_levels.len());
                println!("first frame: {} sampled luminance levels", luma_levels.len());
            }
            if check_changes {
                let changed = first_luma.iter().zip(&last_luma).filter(|(first, last)| first.abs_diff(**last) > 20).count();
                println!("first-to-last changed samples: {changed}/{}", first_luma.len());
                println!("maximum changed samples compared to first frame: {max_changed}/{}", first_luma.len());
                if std::env::var_os("OPNOVERLAY_EXPECT_VIDEO_MOTION").is_some() {
                    assert!(max_changed > first_luma.len() / 4, "test pattern did not visibly change");
                }
            }
            println!("decoded {frames} frames, last timestamp {previous}");
        }
    }

    #[test]
    #[ignore]
    fn decode_captured_audio_from_file() {
        use windows::core::HSTRING;
        use windows::Win32::Media::MediaFoundation::{MFCreateSourceReaderFromURL, MFAudioFormat_PCM, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_FIRST_AUDIO_STREAM};
        let path = std::env::var_os("OPNOVERLAY_CHECK_FILE").expect("OPNOVERLAY_CHECK_FILE is required");
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _media = Media::start().unwrap();
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path), None).unwrap();
            let output = MFCreateMediaType().unwrap();
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).unwrap();
            let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
            reader.SetCurrentMediaType(stream, None, &output).unwrap();
            let (mut samples, mut bytes) = (0usize, 0u64);
            let format = reader.GetCurrentMediaType(stream).unwrap();
            let bits = format.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap();
            assert_eq!(bits, 16);
            assert_eq!(format.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap(), 2);
            assert_eq!(format.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap(), 48_000);
            let mut peak = 0i32;
            let mut samples_440 = 0u64;
            let (mut sine, mut cosine) = (0.0f64, 0.0f64);
            loop {
                let (mut flags, mut sample) = (0u32, None);
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)).unwrap();
                if let Some(sample) = sample {
                    bytes += u64::from(sample.GetTotalLength().unwrap());
                    samples += 1;
                    let buffer = sample.ConvertToContiguousBuffer().unwrap();
                    let (mut data, mut len) = (std::ptr::null_mut(), 0);
                    buffer.Lock(&mut data, None, Some(&mut len)).unwrap();
                    for chunk in std::slice::from_raw_parts(data, len as usize).chunks_exact(2) {
                        peak = peak.max(i32::from(i16::from_le_bytes([chunk[0], chunk[1]])).abs());
                    }
                    for frame in std::slice::from_raw_parts(data, len as usize).chunks_exact(4) {
                        let value = f64::from(i16::from_le_bytes([frame[0], frame[1]]));
                        let phase = samples_440 as f64 * 440.0 * std::f64::consts::TAU / 48_000.0;
                        sine += value * phase.sin();
                        cosine += value * phase.cos();
                        samples_440 += 1;
                    }
                    buffer.Unlock().unwrap();
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 { break; }
            }
            assert!(samples > 0 && bytes > 0);
            let tone = 2.0 * sine.hypot(cosine) / samples_440.max(1) as f64;
            if std::env::var_os("OPNOVERLAY_EXPECT_SOUND").is_some() { assert!(tone > 50.0, "test tone too quiet: {tone}"); }
            println!("decoded {samples} audio samples, {bytes} bytes, peak {peak}, 440 Hz amplitude {tone:.1}");
        }
    }

    /// A hardware D3D11 device and a BGRA canvas with its converter, or `None` without a GPU
    /// video processor (a CI runner, for one).
    unsafe fn gpu(size: (u32, u32)) -> Option<(ID3D11Device, Converter)> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let mut device = None;
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;
            D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, Default::default(), flags, None, D3D11_SDK_VERSION, Some(&mut device), None, None)
                .ok()?;
            let device = device?;
            let desc = D3D11_TEXTURE2D_DESC {
                Width: size.0,
                Height: size.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut canvas = None;
            device.CreateTexture2D(&desc, None, Some(&mut canvas)).ok()?;
            match Converter::new(&device, &canvas?, size, size, 1) {
                Ok(converter) => Some((device, converter)),
                Err(e) => {
                    eprintln!("skipped, no GPU video processor: {e}");
                    None
                }
            }
        }
    }

    /// Encodes `frames` frames of the canvas and drains the encoder; returns the packets.
    fn encode(h264: &H264, converter: &Converter, frames: i64, fps: i64) -> Vec<Packet> {
        let (mut sent, mut packets) = (0, Vec::new());
        loop {
            match h264.next_event().unwrap() {
                VideoEvent::NeedInput if sent < frames => {
                    converter.convert(0).unwrap();
                    h264.push(converter.texture(0), sent * TICKS_PER_SECOND / fps, TICKS_PER_SECOND / fps).unwrap();
                    sent += 1;
                }
                VideoEvent::NeedInput => h264.drain().unwrap(),
                VideoEvent::HaveOutput => packets.extend(h264.pull().unwrap()),
                VideoEvent::Drained => return packets,
                VideoEvent::Other => {}
            }
        }
    }

    /// A GPU without a usable hardware encoder: the software encoder must still turn the canvas
    /// into H.264. Skipped where there is no GPU video processor (a CI runner, for one).
    #[test]
    fn software_encoder_encodes_the_canvas() {
        const SIZE: (u32, u32) = (320, 180);
        const FRAMES: i64 = 10;
        unsafe {
            let media = Media::start().unwrap();
            let Some((device, converter)) = gpu(SIZE) else { return };
            let activate = software_encoders().unwrap().into_iter().next().expect("no software H.264 encoder");
            let h264 = H264::open(&device, &activate, false, SIZE, 30, 1_000_000).unwrap();
            let packets = encode(&h264, &converter, FRAMES, 30);
            assert_eq!(packets.len(), FRAMES as usize);
            assert!(packets[0].key);
            assert!(h264.output_type().is_ok());
            drop((h264, converter, media));
        }
    }

    /// GPU memory this process uses, local and shared, in bytes.
    unsafe fn gpu_memory(device: &ID3D11Device) -> u64 {
        unsafe {
            let adapter: IDXGIAdapter3 = device.cast::<IDXGIDevice>().unwrap().GetAdapter().unwrap().cast().unwrap();
            [DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL]
                .iter()
                .map(|&group| {
                    let mut info = Default::default();
                    adapter.QueryVideoMemoryInfo(0, group, &mut info).unwrap();
                    info.CurrentUsage
                })
                .sum()
        }
    }

    /// Every recording makes a new D3D device (its capture session's) and a new hardware
    /// encoder on it: once a recording is over, the encoder must let the device go, and with it
    /// every texture made on it. Needs a GPU with a hardware H.264 encoder:
    /// `cargo test --release -- --ignored`.
    #[test]
    #[ignore]
    fn hardware_encoders_give_their_memory_back() {
        const SIZE: (u32, u32) = (1920, 1080);
        unsafe {
            let media = Media::start().unwrap();
            // Measures; the devices below come and go.
            let Some((probe, _)) = gpu((64, 64)) else { return };
            let mut used = Vec::new();
            for _ in 0..5 {
                let (device, converter) = gpu(SIZE).unwrap();
                let Some(activate) = hardware_encoders(&device).unwrap_or_default().into_iter().next() else {
                    eprintln!("skipped, no hardware H.264 encoder");
                    return;
                };
                let h264 = H264::open(&device, &activate, true, SIZE, 60, 12_000_000).unwrap();
                let aac = Aac::new().unwrap();
                assert_eq!(encode(&h264, &converter, 30, 60).len(), 30);
                drop((h264, aac, converter, activate, device));
                used.push(gpu_memory(&probe));
            }
            let grown = used[4].saturating_sub(used[0]) as f64 / (1 << 20) as f64;
            assert!(grown < 10.0, "GPU memory grew {grown:.0} MB over four more recordings: {used:?}");
            drop(media);
        }
    }
}
