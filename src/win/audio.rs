

use std::collections::VecDeque;
use std::error::Error;
use std::thread;
use std::time::{Duration, Instant};

use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

use crate::audio::{CHANNELS, Device, InputKind, InputShared, SAMPLE_RATE};

fn direction(kind: InputKind) -> Direction {
    match kind {
        
        InputKind::System | InputKind::SystemExcept(_) | InputKind::Application(_) => Direction::Render,
        InputKind::Microphone => Direction::Capture,
    }
}


pub fn list_devices(kind: InputKind) -> Vec<Device> {
    let list = move || -> Result<Vec<Device>, Box<dyn Error>> {
        wasapi::initialize_mta().ok()?;
        let collection = DeviceEnumerator::new()?.get_device_collection(&direction(kind))?;
        let mut out = Vec::new();
        for device in &collection {
            let device = device?;
            out.push(Device { id: device.get_id()?, name: device.get_friendlyname()? });
        }
        Ok(out)
    };
    
    thread::spawn(move || list().map_err(|e| e.to_string())).join().ok().and_then(Result::ok).unwrap_or_default()
}




pub fn capture(kind: InputKind, device_id: Option<&str>, shared: &InputShared) -> Result<(), Box<dyn Error>> {
    wasapi::initialize_mta().ok()?;
    let follows_default = device_id.is_none() && matches!(kind, InputKind::System | InputKind::Microphone);
    let mut opened = false;
    while !shared.stopped() {
        match stream(kind, device_id, shared, follows_default, &mut opened) {
            Ok(()) => {}
            Err(_) if follows_default && opened => {
                
                
                shared.silent();
                for _ in 0..20 {
                    if shared.stopped() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}


fn stream(kind: InputKind, device_id: Option<&str>, shared: &InputShared, follows_default: bool, opened: &mut bool) -> Result<(), Box<dyn Error>> {
    let format = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None);
    let enumerator = DeviceEnumerator::new()?;
    let mut device_in_use = None;
    let (mut client, mode) = match kind {
        
        
        InputKind::Application(pid) | InputKind::SystemExcept(pid) => (
            AudioClient::new_application_loopback_client(pid, matches!(kind, InputKind::Application(_)))?,
            StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 },
        ),
        _ => {
            let device = match device_id {
                Some(id) => enumerator.get_device(id)?,
                None => enumerator.get_default_device(&direction(kind))?,
            };
            device_in_use = Some(device.get_id()?);
            (device.get_iaudioclient()?, StreamMode::PollingShared { autoconvert: true, buffer_duration_hns: 1_000_000 })
        }
    };
    client.initialize_client(&format, &Direction::Capture, &mode)?;
    let event = match mode {
        StreamMode::EventsShared { .. } => Some(client.set_get_eventhandle()?),
        _ => None,
    };
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    *opened = true;

    let mut raw: VecDeque<u8> = VecDeque::new();
    let mut checked = Instant::now();
    let mut last_sound = Instant::now();
    while !shared.stopped() {
        
        loop {
            let before = raw.len();
            let info = capture.read_from_device_to_deque(&mut raw)?;
            if raw.len() == before {
                break;
            }
            if info.flags.silent {
                raw.range_mut(before..).for_each(|b| *b = 0);
            }
        }
        let usable = raw.len() / 4 * 4;
        if usable > 0 {
            let bytes: Vec<u8> = raw.drain(..usable).collect();
            shared.deliver(bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect());
            last_sound = Instant::now();
        } else if last_sound.elapsed() >= Duration::from_millis(50) {
            
            
            
            shared.silent();
        }
        if follows_default && checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            let default = enumerator.get_default_device(&direction(kind)).and_then(|d| d.get_id());
            if default.ok() != device_in_use {
                break;
            }
        }
        match &event {
            
            Some(event) => {
                let _ = event.wait_for_event(20);
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
    client.stop_stream()?;
    Ok(())
}
