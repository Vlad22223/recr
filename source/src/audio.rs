use crate::locale::trf;
use std::io;
use std::ptr;
use std::sync::OnceLock;

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eCapture, eConsole,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::core::{BSTR, GUID, PCWSTR};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 1;
pub const BLOCK_ALIGN: u32 = CHANNELS * 2;
const TICKS_PER_SECOND: u64 = 10_000_000;
const FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

#[derive(Clone)]
pub struct Microphone {
    pub id: String,
    pub name: String,
}

pub fn microphones() -> io::Result<Vec<Microphone>> {
    let _com = Com::enter()?;
    microphones_from(&enumerator()?)
}

fn microphones_from(enumerator: &IMMDeviceEnumerator) -> io::Result<Vec<Microphone>> {
    let endpoints = win("List microphone endpoints", unsafe {
        enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)
    })?;
    let mut result = Vec::new();
    for index in 0..win("Count microphone endpoints", unsafe {
        endpoints.GetCount()
    })? {
        let device = win("Read microphone endpoint", unsafe { endpoints.Item(index) })?;
        let id = endpoint_id(&device)?;
        let name = unsafe {
            device
                .OpenPropertyStore(STGM_READ)
                .and_then(|store| store.GetValue(&FRIENDLY_NAME))
                .and_then(|value| BSTR::try_from(&value))
        }
        .map(|value| String::from_utf16_lossy(value.as_wide()))
        .unwrap_or_else(|_| trf!("Микрофон {}", "Microphone {}", index + 1));
        result.push(Microphone { id, name });
    }
    result.sort_by_cached_key(|device| device.name.to_lowercase());
    Ok(result)
}

fn endpoint_id(device: &IMMDevice) -> io::Result<String> {
    let id = win("Read microphone ID", unsafe { device.GetId() })?;
    let result = unsafe { id.to_string() };
    unsafe { CoTaskMemFree(Some(id.as_ptr().cast())) };
    result.map_err(io::Error::other)
}

fn enumerator() -> io::Result<IMMDeviceEnumerator> {
    win("Open Windows audio endpoints", unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)
    })
}

fn select_device(
    enumerator: &IMMDeviceEnumerator,
    selection: Option<&str>,
) -> io::Result<IMMDevice> {
    let Some(selection) = selection else {
        return win("Open default microphone", unsafe {
            enumerator.GetDefaultAudioEndpoint(eCapture, eConsole)
        });
    };
    let owned_id;
    let id = if selection.starts_with("@device_") {
        let suffix = selection.rsplit_once('{').map(|(_, id)| id);
        owned_id = microphones_from(enumerator)?
            .into_iter()
            .find(|mic| {
                suffix.is_some_and(|id| {
                    mic.id
                        .rsplit_once('{')
                        .is_some_and(|(_, endpoint)| endpoint.eq_ignore_ascii_case(id))
                })
            })
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Selected microphone unavailable")
            })?
            .id;
        owned_id.as_str()
    } else {
        selection
    };
    let id: Vec<u16> = id.encode_utf16().chain([0]).collect();
    win("Open selected microphone", unsafe {
        enumerator.GetDevice(PCWSTR(id.as_ptr()))
    })
}

pub struct AudioBlock {
    pub samples: Vec<u8>,
    pub frames: u32,
    pub qpc_100ns: u64,
}

pub struct Capture {
    packets: IAudioCaptureClient,
    client: IAudioClient,
    last_end: Option<u64>,
    _com: Com,
}

impl Capture {
    // Call open, poll, and drop on the same thread.
    pub fn open(selection: Option<&str>) -> io::Result<Option<Self>> {
        if selection == Some("") {
            return Ok(None);
        }

        let com = Com::enter()?;
        let device = select_device(&enumerator()?, selection)?;
        let client: IAudioClient = win("Activate microphone", unsafe {
            device.Activate(CLSCTX_INPROC_SERVER, None)
        })?;
        let pcm = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * BLOCK_ALIGN,
            nBlockAlign: BLOCK_ALIGN as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        win("Configure microphone", unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                2_000_000,
                0,
                &pcm,
                None,
            )
        })?;
        let packets = win("Open microphone buffer", unsafe { client.GetService() })?;
        win("Start microphone", unsafe { client.Start() })?;
        Ok(Some(Self {
            packets,
            client,
            last_end: None,
            _com: com,
        }))
    }

    pub fn poll(&mut self) -> io::Result<Vec<AudioBlock>> {
        let mut result = Vec::new();
        while win("Check microphone buffer", unsafe {
            self.packets.GetNextPacketSize()
        })? != 0
        {
            let mut data = ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            let mut qpc = 0;
            win("Read microphone buffer", unsafe {
                self.packets
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))
            })?;
            if frames == 0 {
                break;
            }
            let len = frames as usize * BLOCK_ALIGN as usize;
            let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                vec![0; len]
            } else {
                unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
            };
            win("Release microphone buffer", unsafe {
                self.packets.ReleaseBuffer(frames)
            })?;
            let duration = u64::from(frames) * TICKS_PER_SECOND / u64::from(SAMPLE_RATE);
            let timestamp = if qpc == 0 || flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0
            {
                self.last_end
                    .unwrap_or_else(|| clock_100ns().saturating_sub(duration))
            } else {
                qpc.max(self.last_end.unwrap_or(0))
            };
            self.last_end = Some(timestamp.saturating_add(duration));
            result.push(AudioBlock {
                samples,
                frames,
                qpc_100ns: timestamp,
            });
        }
        Ok(result)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

pub fn clock_100ns() -> u64 {
    static FREQUENCY: OnceLock<u64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut value = 0;
        let _ = unsafe { QueryPerformanceFrequency(&mut value) };
        value.max(1) as u64
    });
    let mut counter = 0;
    let _ = unsafe { QueryPerformanceCounter(&mut counter) };
    ((counter.max(0) as u128 * u128::from(TICKS_PER_SECOND)) / u128::from(frequency)) as u64
}

struct Com(bool);

impl Com {
    fn enter() -> io::Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self(false));
        }
        win("Initialize Windows audio", result.ok())?;
        Ok(Self(true))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn win<T>(operation: &str, result: windows::core::Result<T>) -> io::Result<T> {
    result.map_err(|error| io::Error::other(format!("{operation}: {error}")))
}
