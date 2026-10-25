//! Host audio: loopback capture of a playback device, delivered as 5 ms
//! 48 kHz stereo frames stamped with their capture time.
//!
//! The audio engine processes a device in periods of the smallest size any
//! client asked for (10 ms by default). Loopback data arrives once per
//! period, so the thread first opens a silent IAudioClient3 render stream at
//! the device's minimum period (typically ~3 ms). That shrinks the capture
//! latency for everything playing on the device, and doubles as the test
//! tone source. The thread wakes on that stream's event, tops it up, then
//! drains the loopback packets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use proto::audio::{CHANNELS, FRAME, RATE};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::resample::Linear;
use crate::wasapi::{self, MixFormat};

pub struct CaptureConfig {
    /// Playback device to capture (substring of its name); default device if None.
    pub device: Option<String>,
    /// Play a 440 Hz tone on the device (end-to-end tests without other audio).
    pub test_tone: bool,
}

/// Start capturing. `on_frame` gets interleaved stereo `[f32; 480]` and the
/// QPC-microsecond time its first sample went through the audio engine.
pub fn start(
    cfg: CaptureConfig,
    stop: Arc<AtomicBool>,
    on_frame: impl FnMut(&[f32], u64) + Send + 'static,
) -> Result<JoinHandle<()>> {
    Ok(std::thread::Builder::new()
        .name("audio-cap".into())
        .spawn(move || {
            wasapi::pro_audio_thread();
            if let Err(e) = run(cfg, &stop, on_frame) {
                tracing::error!("audio capture stopped: {e:#}");
            }
        })?)
}

fn run(cfg: CaptureConfig, stop: &AtomicBool, mut on_frame: impl FnMut(&[f32], u64)) -> Result<()> {
    // Reopen on device loss (unplugged, default changed, exclusive mode).
    while !stop.load(Ordering::Relaxed) {
        match capture(&cfg, stop, &mut on_frame) {
            Ok(()) => break,
            Err(e) if wasapi::is_device_lost(&e) => {
                tracing::warn!("audio capture device lost ({e}); reopening")
            }
            Err(e) => tracing::error!("audio capture failed: {e:#}; retrying"),
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

fn capture(
    cfg: &CaptureConfig,
    stop: &AtomicBool,
    on_frame: &mut impl FnMut(&[f32], u64),
) -> Result<()> {
    let (dev, name) = wasapi::output_device(cfg.device.as_deref())?;
    let keeper = wasapi::open_low_latency_render(&dev)?;
    let (kfmt, period) = (&keeper.fmt, keeper.period);
    let loop_event = wasapi::Event::new()?;
    // SAFETY: COM calls on valid interfaces from this (MTA) thread; buffer
    // pointers from GetBuffer are used only until the matching ReleaseBuffer,
    // within the frame counts WASAPI returned.
    unsafe {
        let render: IAudioRenderClient = keeper.client.GetService()?;
        let kbuf = keeper.client.GetBufferSize()?;

        // Event-driven loopback where supported (wakes when the engine has
        // written the packet); otherwise poll it on the keeper's event.
        let open_loopback = |flags: u32| -> Result<(IAudioClient, MixFormat)> {
            let c: IAudioClient = dev.Activate(windows::Win32::System::Com::CLSCTX_ALL, None)?;
            let fmt = MixFormat::of(&c)?;
            c.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, fmt.ptr, None)
                .context("loopback Initialize")?;
            Ok((c, fmt))
        };
        let evented =
            open_loopback(AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK)
                .and_then(|(c, f)| {
                    c.SetEventHandle(loop_event.0)?;
                    Ok((c, f))
                });
        let (loop_client, fmt, loop_evented) = match evented {
            Ok((c, f)) => (c, f, true),
            Err(_) => {
                let (c, f) = open_loopback(AUDCLNT_STREAMFLAGS_LOOPBACK)?;
                (c, f, false)
            }
        };
        let cap: IAudioCaptureClient = loop_client.GetService()?;
        tracing::info!(
            "audio: capturing \"{name}\" ({} Hz, {} ch), engine period {:.2} ms, loopback {}{}",
            fmt.rate,
            fmt.channels,
            period as f64 * 1e3 / kfmt.rate as f64,
            if loop_evented {
                "event-driven"
            } else {
                "polled"
            },
            if cfg.test_tone { ", test tone ON" } else { "" }
        );
        keeper.client.Start()?;
        loop_client.Start()?;

        let mut tone_phase = 0f64;
        let mut resampler = Linear::new(fmt.rate, RATE);
        let mut stereo: Vec<f32> = Vec::with_capacity(8192);
        let mut acc: Vec<f32> = Vec::with_capacity(16384);
        // Capture time of acc[0].
        let mut acc_start_us = 0u64;
        let events = [keeper.event.0, loop_event.0];
        let n_events = if loop_evented { 2 } else { 1 };

        let result = (|| -> Result<()> {
            while !stop.load(Ordering::Relaxed) {
                let w = WaitForMultipleObjects(&events[..n_events], false, 50);
                if w.0 >= WAIT_OBJECT_0.0 + n_events as u32 {
                    continue;
                }
                // Keep the low-latency stream fed (silence or the tone).
                let pad = keeper.client.GetCurrentPadding()?;
                let n = kbuf.saturating_sub(pad).min(period * 2);
                if n > 0 {
                    let p = render.GetBuffer(n)? as *mut f32;
                    let out = std::slice::from_raw_parts_mut(p, n as usize * kfmt.channels);
                    if cfg.test_tone {
                        for f in out.chunks_exact_mut(kfmt.channels) {
                            let v = (tone_phase * std::f64::consts::TAU).sin() as f32 * 0.2;
                            tone_phase = (tone_phase + 440.0 / kfmt.rate as f64).fract();
                            f.fill(v);
                        }
                        render.ReleaseBuffer(n, 0)?;
                    } else {
                        render.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                    }
                }

                // Drain loopback.
                while cap.GetNextPacketSize()? > 0 {
                    let (mut data, mut frames, mut flags, mut qpc) =
                        (std::ptr::null_mut(), 0u32, 0u32, 0u64);
                    cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                    stereo.clear();
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    if silent || data.is_null() {
                        stereo.resize(frames as usize * 2, 0.0);
                    } else {
                        let s = std::slice::from_raw_parts(
                            data as *const f32,
                            frames as usize * fmt.channels,
                        );
                        for f in s.chunks_exact(fmt.channels) {
                            let (l, r) = if fmt.channels == 1 {
                                (f[0], f[0])
                            } else {
                                (f[0], f[1])
                            };
                            stereo.push(l);
                            stereo.push(r);
                        }
                    }
                    cap.ReleaseBuffer(frames)?;

                    // QPC position, in 100 ns units. For loopback it is when
                    // the sample reaches the host's own output, so latency
                    // measured against it is what remote playback adds over
                    // listening locally.
                    let t_us = qpc / 10;
                    let queued_us = (acc.len() / CHANNELS) as u64 * 1_000_000 / RATE as u64;
                    let expected = acc_start_us + queued_us;
                    if acc.is_empty()
                        || flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0
                        || t_us.abs_diff(expected) > 2_000
                    {
                        acc_start_us = t_us.saturating_sub(queued_us);
                    }
                    resampler.process(&stereo, &mut acc);
                    let frame_len = FRAME * CHANNELS;
                    let mut used = 0;
                    while acc.len() - used >= frame_len {
                        on_frame(&acc[used..used + frame_len], acc_start_us);
                        used += frame_len;
                        acc_start_us += proto::audio::FRAME_US;
                    }
                    acc.drain(..used);
                }
            }
            Ok(())
        })();
        let _ = loop_client.Stop();
        let _ = keeper.client.Stop();
        drop(cap);
        drop(render);
        result
    }
}
