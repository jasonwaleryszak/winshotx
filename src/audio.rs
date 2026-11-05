//! WASAPI capture of microphones and system audio (loopback). The audio
//! engine converts every source to 48 kHz stereo float so they can be mixed
//! by simple addition.

use std::collections::VecDeque;
use std::ffi::c_void;

use windows::core::{Result, HSTRING};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// Device-side buffer, generous so a slow encoder write can't overflow it.
const BUFFER_100NS: i64 = 10_000_000;

pub struct Device {
    pub id: String,
    pub name: String,
}

unsafe fn enumerator() -> Result<IMMDeviceEnumerator> {
    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
}

/// Active microphones (capture endpoints), in the system's order.
pub fn microphones() -> Vec<Device> {
    unsafe { list(eCapture).unwrap_or_default() }
}

unsafe fn list(flow: EDataFlow) -> Result<Vec<Device>> {
    let devices = enumerator()?.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
    let mut out = Vec::new();
    for i in 0..devices.GetCount()? {
        let device = devices.Item(i)?;
        let raw = device.GetId()?;
        let id = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const c_void));
        let Ok(id) = id else {
            continue;
        };
        let name = device
            .OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
            .map(|v| v.to_string())
            .unwrap_or_default();
        let name = if name.is_empty() {
            "Microphone".to_string()
        } else {
            name
        };
        out.push(Device { id, name });
    }
    Ok(out)
}

/// A running capture stream. Samples accumulate in `buffer` (interleaved
/// stereo) each time `read` is called.
pub struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    pub buffer: VecDeque<f32>,
}

impl Capture {
    /// Whatever is playing on the default output device.
    pub fn system() -> Result<Self> {
        unsafe {
            let device = enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?;
            Self::open(&device, AUDCLNT_STREAMFLAGS_LOOPBACK)
        }
    }

    pub fn microphone(id: &str) -> Result<Self> {
        unsafe {
            let device = enumerator()?.GetDevice(&HSTRING::from(id))?;
            Self::open(&device, 0)
        }
    }

    unsafe fn open(device: &IMMDevice, flags: u32) -> Result<Self> {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let block = (CHANNELS * 4) as u16;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_100NS,
            0,
            &format,
            None,
        )?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Self {
            client,
            capture,
            buffer: VecDeque::new(),
        })
    }

    /// Moves every packet the device has captured so far into `buffer`.
    pub fn read(&mut self) -> Result<()> {
        unsafe {
            while self.capture.GetNextPacketSize()? > 0 {
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = (frames * CHANNELS) as usize;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    self.buffer.extend(std::iter::repeat(0.0).take(n));
                } else {
                    self.buffer
                        .extend(std::slice::from_raw_parts(data as *const f32, n));
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}
