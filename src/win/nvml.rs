use std::ffi::c_void;

use windows::core::{s, w};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};

type Device = *mut c_void;
type Init = unsafe extern "C" fn() -> i32;
type Shutdown = unsafe extern "C" fn() -> i32;
type GetDevice = unsafe extern "C" fn(u32, *mut Device) -> i32;
type GetUtilization = unsafe extern "C" fn(Device, *mut Utilization) -> i32;
type GetTemperature = unsafe extern "C" fn(Device, u32, *mut u32) -> i32;
type GetMemory = unsafe extern "C" fn(Device, *mut Memory) -> i32;

#[repr(C)]
#[derive(Default)]
struct Utilization { gpu: u32, memory: u32 }

#[repr(C)]
#[derive(Default)]
struct Memory { total: u64, free: u64, used: u64 }

#[derive(Clone, Copy, Debug, Default)]
pub struct GpuStats {
    pub utilization: Option<u32>,
    pub temperature: Option<u32>,
    pub memory: Option<(u64, u64)>,
}

pub struct Nvidia {
    library: HMODULE,
    device: Device,
    shutdown: Shutdown,
    utilization: GetUtilization,
    temperature: GetTemperature,
    memory: GetMemory,
}

impl Nvidia {
    pub fn new() -> Option<Self> {
        unsafe {
            let library = LoadLibraryExW(w!("nvml.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
            let load = || -> Option<Self> {
                let init: Init = std::mem::transmute(GetProcAddress(library, s!("nvmlInit_v2"))?);
                let shutdown: Shutdown = std::mem::transmute(GetProcAddress(library, s!("nvmlShutdown"))?);
                let get_device: GetDevice = std::mem::transmute(GetProcAddress(library, s!("nvmlDeviceGetHandleByIndex_v2"))?);
                let utilization: GetUtilization = std::mem::transmute(GetProcAddress(library, s!("nvmlDeviceGetUtilizationRates"))?);
                let temperature: GetTemperature = std::mem::transmute(GetProcAddress(library, s!("nvmlDeviceGetTemperature"))?);
                let memory: GetMemory = std::mem::transmute(GetProcAddress(library, s!("nvmlDeviceGetMemoryInfo"))?);
                if init() != 0 { return None; }
                let mut device = std::ptr::null_mut();
                if get_device(0, &mut device) != 0 || device.is_null() { let _ = shutdown(); return None; }
                Some(Self { library, device, shutdown, utilization, temperature, memory })
            };
            match load() {
                Some(nvidia) => Some(nvidia),
                None => { let _ = FreeLibrary(library); None }
            }
        }
    }

    pub fn sample(&self) -> GpuStats {
        unsafe {
            let mut utilization = Utilization::default();
            let mut temperature = 0;
            let mut memory = Memory::default();
            GpuStats {
                utilization: ((self.utilization)(self.device, &mut utilization) == 0 && utilization.gpu <= 100).then_some(utilization.gpu),
                temperature: ((self.temperature)(self.device, 0, &mut temperature) == 0).then_some(temperature),
                memory: ((self.memory)(self.device, &mut memory) == 0 && memory.total > 0 && memory.used <= memory.total).then_some((memory.used, memory.total)),
            }
        }
    }
}

impl Drop for Nvidia {
    fn drop(&mut self) {
        unsafe { let _ = (self.shutdown)(); let _ = FreeLibrary(self.library); }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore]
    fn reads_nvidia_metrics_if_driver_is_available() {
        let gpu = super::Nvidia::new().expect("NVIDIA driver unavailable");
        let stats = gpu.sample();
        assert!(stats.utilization.is_some() && stats.temperature.is_some() && stats.memory.is_some());
        if let Some(value) = stats.utilization { assert!(value <= 100); }
        if let Some((used, total)) = stats.memory { assert!(used <= total && total > 0); }
    }
}
