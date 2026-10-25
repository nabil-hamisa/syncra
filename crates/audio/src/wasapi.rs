//! WASAPI plumbing shared by capture and render: COM setup, endpoint lookup
//! and mix-format parsing.

use anyhow::{bail, Context, Result};
use windows::core::GUID;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, CreateEventW};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Initialize COM (MTA) on the calling thread. Idempotent.
pub fn com_init() {
    // SAFETY: plain COM initialization; S_FALSE / RPC_E_CHANGED_MODE just
    // mean COM is already set up on this thread, which is fine.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

/// "Pro Audio" MMCSS scheduling for the calling thread.
pub fn pro_audio_thread() {
    let mut idx = 0u32;
    // SAFETY: registers the current thread with MMCSS; `idx` is an out-param.
    if let Err(e) =
        unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut idx) }
    {
        tracing::warn!("MMCSS Pro Audio registration failed: {e}");
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: standard COM activation of the system device enumerator.
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?)
}

fn friendly_name(dev: &IMMDevice) -> Result<String> {
    // SAFETY: valid device; the returned string is freed with CoTaskMemFree.
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ)?;
        let v = store.GetValue(&PKEY_Device_FriendlyName)?;
        let s = PropVariantToStringAlloc(&v)?;
        let name = s.to_string().unwrap_or_default();
        CoTaskMemFree(Some(s.0 as _));
        Ok(name)
    }
}

/// Names of active playback devices.
pub fn list_outputs() -> Result<Vec<String>> {
    com_init();
    // SAFETY: COM calls on valid interfaces.
    unsafe {
        let coll = enumerator()?.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut out = Vec::new();
        for i in 0..coll.GetCount()? {
            let dev = coll.Item(i)?;
            let periods = engine_periods(&dev).map_or("periods unknown".into(), |(r, d, m)| {
                format!(
                    "{r} Hz, engine period default {:.2} ms, min {:.2} ms",
                    d as f64 * 1e3 / r as f64,
                    m as f64 * 1e3 / r as f64
                )
            });
            out.push(format!("{}  [{periods}]", friendly_name(&dev)?));
        }
        Ok(out)
    }
}

/// The playback device whose name contains `name` (case-insensitive), or the
/// default one.
pub fn output_device(name: Option<&str>) -> Result<(IMMDevice, String)> {
    com_init();
    let en = enumerator()?;
    // SAFETY: COM calls on valid interfaces.
    unsafe {
        let Some(want) = name else {
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let n = friendly_name(&dev)?;
            return Ok((dev, n));
        };
        let want = want.to_lowercase();
        let coll = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..coll.GetCount()? {
            let dev = coll.Item(i)?;
            let n = friendly_name(&dev)?;
            if n.to_lowercase().contains(&want) {
                return Ok((dev, n));
            }
        }
    }
    bail!("no playback device matching {name:?} (see --list-audio)")
}

/// (rate, default period, minimum period) in frames. Queries only: no
/// stream is opened, nothing plays.
fn engine_periods(dev: &IMMDevice) -> Result<(u32, u32, u32)> {
    // SAFETY: COM calls on a valid device; the format outlives the call.
    unsafe {
        let c3: IAudioClient3 = dev.Activate(CLSCTX_ALL, None)?;
        let fmt = MixFormat::of(&c3.cast()?)?;
        let (mut d, mut f, mut min, mut max) = (0, 0, 0, 0);
        c3.GetSharedModeEnginePeriod(fmt.ptr, &mut d, &mut f, &mut min, &mut max)?;
        Ok((fmt.rate, d, min))
    }
}

/// A shared-mode mix format we can process: 32-bit float, any rate/channels.
pub struct MixFormat {
    pub ptr: *mut WAVEFORMATEX,
    pub rate: u32,
    pub channels: usize,
}

impl MixFormat {
    pub fn of(client: &IAudioClient) -> Result<Self> {
        // SAFETY: GetMixFormat returns a CoTaskMemAlloc'd WAVEFORMATEX(-
        // EXTENSIBLE), freed in Drop; the fields are read within its size.
        unsafe {
            let ptr = client.GetMixFormat()?;
            // Packed structs: copy them out instead of referencing fields.
            let f = std::ptr::read_unaligned(ptr);
            let (tag, cb, bits) = (f.wFormatTag, f.cbSize, f.wBitsPerSample);
            let float = match tag {
                WAVE_FORMAT_IEEE_FLOAT => true,
                WAVE_FORMAT_EXTENSIBLE if cb >= 22 => {
                    let ext = std::ptr::read_unaligned(ptr as *const WAVEFORMATEXTENSIBLE);
                    let sub = ext.SubFormat;
                    sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                }
                _ => false,
            };
            let m = MixFormat {
                ptr,
                rate: f.nSamplesPerSec,
                channels: f.nChannels as usize,
            };
            if !float || bits != 32 || m.channels == 0 {
                bail!(
                    "unsupported mix format (tag {tag:#x}, {bits} bits, {} ch)",
                    m.channels
                );
            }
            Ok(m)
        }
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        // SAFETY: allocated by GetMixFormat with CoTaskMemAlloc.
        unsafe { CoTaskMemFree(Some(self.ptr as _)) }
    }
}

/// An auto-reset event, closed on drop.
pub struct Event(pub HANDLE);

impl Event {
    pub fn new() -> Result<Self> {
        // SAFETY: unnamed auto-reset event with default security.
        Ok(Event(unsafe { CreateEventW(None, false, false, None) }?))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed once.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// An event-driven shared-mode render stream. Fields drop in order, so the
/// client is released before the event it signals is closed.
pub struct LowLatencyRender {
    pub client: IAudioClient,
    pub fmt: MixFormat,
    /// Engine period in frames.
    pub period: u32,
    pub event: Event,
}

/// Open `dev` for event-driven shared-mode rendering at the smallest engine
/// period it supports (IAudioClient3), falling back to the default period
/// (plain IAudioClient) when that is unavailable or refused.
pub fn open_low_latency_render(dev: &IMMDevice) -> Result<LowLatencyRender> {
    let event = Event::new()?;
    // SAFETY: COM calls on a valid device; the format pointer outlives the calls.
    unsafe {
        if let Ok(c3) = dev.Activate::<IAudioClient3>(CLSCTX_ALL, None) {
            let client: IAudioClient = c3.cast()?;
            let fmt = MixFormat::of(&client)?;
            let (mut def, mut fund, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
            if c3
                .GetSharedModeEnginePeriod(fmt.ptr, &mut def, &mut fund, &mut min, &mut max)
                .is_ok()
                && c3
                    .InitializeSharedAudioStream(
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                        min,
                        fmt.ptr,
                        None,
                    )
                    .is_ok()
            {
                client.SetEventHandle(event.0)?;
                return Ok(LowLatencyRender {
                    client,
                    fmt,
                    period: min,
                    event,
                });
            }
        }
        // Fresh client: a failed Initialize* leaves the old one unusable.
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let fmt = MixFormat::of(&client)?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                fmt.ptr,
                None,
            )
            .context("IAudioClient::Initialize")?;
        let (mut dp, mut mp) = (0i64, 0i64);
        client.GetDevicePeriod(Some(&mut dp), Some(&mut mp))?;
        client.SetEventHandle(event.0)?;
        let period = (dp as u64 * fmt.rate as u64 / 10_000_000) as u32;
        Ok(LowLatencyRender {
            client,
            fmt,
            period,
            event,
        })
    }
}

/// Device removed, disabled, reconfigured or taken in exclusive mode: the
/// stream must be reopened.
pub fn is_device_lost(e: &anyhow::Error) -> bool {
    e.downcast_ref::<windows::core::Error>().is_some_and(|w| {
        w.code() == AUDCLNT_E_DEVICE_INVALIDATED || w.code() == AUDCLNT_E_DEVICE_IN_USE
    })
}

use windows::core::Interface;
