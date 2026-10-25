//! DXGI Desktop Duplication capture. Frames never leave the GPU: each acquired
//! desktop image is `CopyResource`'d into a texture from a small ring that the
//! encoder has registered, and the duplication frame is released immediately.

use anyhow::{bail, Context, Result};
use proto::input::{mono_pitch, CursorKind, CursorShape};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

const NVIDIA_VENDOR_ID: u32 = 0x10de;

pub struct Gpu {
    pub device: ID3D11Device,
    pub adapter_name: String,
}

pub struct Capturer {
    /// None while lost and not yet re-opened.
    dup: Option<IDXGIOutputDuplication>,
    output: IDXGIOutput6,
    gpu_context: ID3D11DeviceContext,
    pub width: u32,
    pub height: u32,
    /// Monitor rectangle in virtual-desktop coordinates.
    pub desktop_rect: RECT,
    holding_frame: bool,
    pointer: Option<PointerUpdate>,
    /// Size of a new pointer shape reported by the held frame, read later by
    /// `take_pointer` (off the capture -> encode critical path).
    shape_pending: u32,
    shape_buf: Vec<u8>,
    next_shape_id: u32,
    /// Duplication was lost and re-opening it failed; retried each acquire.
    lost: bool,
    /// When the current loss started and when it was last reported.
    lost_since: Option<std::time::Instant>,
    lost_reported: Option<std::time::Instant>,
    /// GDI name of the captured monitor (`\\.\DISPLAY1`), to find it again
    /// after a mode switch invalidates `output`.
    device_name: [u16; 32],
    /// Last desktop image size, to log mode switches once.
    src_size: (u32, u32),
    /// Stretches desktop images that don't match the ring size.
    fit: Option<crate::scale::Fit>,
}

/// Pointer changes reported by desktop duplication since the last `take_pointer`.
#[derive(Debug)]
pub struct PointerUpdate {
    /// Position of the shape's top-left corner relative to the monitor.
    pub x: i32,
    pub y: i32,
    pub shape: Option<CursorShape>,
}

pub enum Acquired {
    /// New desktop image copied into the target texture. `copy_us` is the CPU
    /// time from AcquireNextFrame returning to ReleaseFrame (GPU copy is async).
    /// `present_us` is when DWM presented this desktop image (QPC timebase),
    /// the earliest point the content existed on the host.
    Frame { copy_us: u32, present_us: u64 },
    /// Only the pointer moved or nothing changed before the timeout.
    NoChange,
}

/// List every adapter/output pair so the user can pick a monitor.
pub fn list_outputs() -> Result<()> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let desc = adapter.GetDesc1()?;
            println!(
                "adapter {a}: {} (vendor {:#06x})",
                wide(&desc.Description),
                desc.VendorId
            );
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let od = output.GetDesc()?;
                let r = od.DesktopCoordinates;
                println!(
                    "  output {o}: {} {}x{} at ({},{})",
                    wide(&od.DeviceName),
                    r.right - r.left,
                    r.bottom - r.top,
                    r.left,
                    r.top
                );
                o += 1;
            }
            a += 1;
        }
    }
    Ok(())
}

/// Create a D3D11 device on the NVIDIA adapter (NVENC requires it) and open
/// desktop duplication on that adapter's `output_index`-th monitor.
pub fn open(output_index: u32) -> Result<(Gpu, Capturer)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let (adapter, name) = find_nvidia_adapter(&factory)?;

        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
        let device = device.unwrap();
        let context = context.unwrap();

        // Capture threads must not be starved by other GPU work.
        if let Ok(dxgi_device) = device.cast::<IDXGIDevice>() {
            let _ = dxgi_device.SetGPUThreadPriority(7);
        }

        let output: IDXGIOutput6 = adapter
            .EnumOutputs(output_index)
            .with_context(|| {
                format!("output {output_index} not found on {name}; is the monitor plugged into the NVIDIA GPU? run `host --list`")
            })?
            .cast()?;
        let od = output.GetDesc()?;
        let r = od.DesktopCoordinates;

        let dup = output
            .DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
            .context("DuplicateOutput1 (process must be per-monitor DPI aware)")?;

        let capturer = Capturer {
            dup: Some(dup),
            output,
            gpu_context: context.clone(),
            width: (r.right - r.left) as u32,
            height: (r.bottom - r.top) as u32,
            desktop_rect: r,
            holding_frame: false,
            pointer: None,
            shape_pending: 0,
            shape_buf: Vec::new(),
            next_shape_id: 1,
            lost: false,
            lost_since: None,
            lost_reported: None,
            device_name: od.DeviceName,
            src_size: ((r.right - r.left) as u32, (r.bottom - r.top) as u32),
            fit: None,
        };
        Ok((
            Gpu {
                device,
                adapter_name: name,
            },
            capturer,
        ))
    }
}

unsafe fn find_nvidia_adapter(factory: &IDXGIFactory1) -> Result<(IDXGIAdapter1, String)> {
    let mut i = 0;
    while let Ok(adapter) = factory.EnumAdapters1(i) {
        let desc = adapter.GetDesc1()?;
        if desc.VendorId == NVIDIA_VENDOR_ID {
            return Ok((adapter, wide(&desc.Description)));
        }
        i += 1;
    }
    bail!("no NVIDIA adapter found")
}

/// Allocate a ring of BGRA textures matching the capture size, usable as NVENC input.
pub fn create_ring(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    n: usize,
) -> Result<Vec<ID3D11Texture2D>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // Shader resource: the adaptive-resolution scaler samples it.
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    (0..n)
        .map(|_| unsafe {
            let mut tex = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            Ok(tex.unwrap())
        })
        .collect()
}

impl Capturer {
    /// Wait up to `timeout_ms` for a new desktop image and copy it into `target`.
    pub fn acquire(&mut self, target: &ID3D11Texture2D, timeout_ms: u32) -> Result<Acquired> {
        unsafe {
            self.release_frame();
            if self.lost && !self.try_reopen(timeout_ms) {
                return Ok(Acquired::NoChange);
            }

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            let Some(dup) = &self.dup else {
                return Ok(Acquired::NoChange);
            };
            match dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Acquired::NoChange),
                Err(e)
                    if e.code() == DXGI_ERROR_DEVICE_REMOVED
                        || e.code() == DXGI_ERROR_DEVICE_RESET =>
                {
                    return Err(e).context("AcquireNextFrame");
                }
                Err(e) => {
                    // ACCESS_LOST: mode change, fullscreen app switch, or
                    // secure desktop. Anything else (INVALID_CALL after a
                    // half-finished switch, ...) also clears up with a new
                    // duplication, so never end the session over it.
                    if e.code() != DXGI_ERROR_ACCESS_LOST {
                        tracing::warn!("AcquireNextFrame: {e}; re-opening duplication");
                    }
                    self.try_reopen(timeout_ms);
                    return Ok(Acquired::NoChange);
                }
            }
            self.holding_frame = true;
            let acquired_at = std::time::Instant::now();
            if info.LastMouseUpdateTime != 0 {
                let pos = info.PointerPosition.Position;
                let p = self.pointer.get_or_insert(PointerUpdate {
                    x: 0,
                    y: 0,
                    shape: None,
                });
                p.x = pos.x;
                p.y = pos.y;
            }
            if info.PointerShapeBufferSize > 0 {
                self.shape_pending = info.PointerShapeBufferSize;
            }

            if info.LastPresentTime == 0 {
                // Pointer-only update; no new desktop image.
                return Ok(Acquired::NoChange);
            }

            let tex: ID3D11Texture2D = resource.unwrap().cast()?;
            let mut d = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut d);
            if (d.Width, d.Height) != self.src_size {
                tracing::info!(
                    "desktop image is {}x{} (stream stays {}x{})",
                    d.Width,
                    d.Height,
                    self.width,
                    self.height
                );
                self.src_size = (d.Width, d.Height);
            }
            if (d.Width, d.Height) == (self.width, self.height) {
                self.gpu_context.CopyResource(target, &tex);
            } else {
                // An exclusive-fullscreen game changed the display mode.
                if self.fit.is_none() {
                    self.fit = Some(crate::scale::Fit::new(&self.gpu_context)?);
                }
                if let Some(fit) = self.fit.as_mut() {
                    fit.blit(&tex, target, self.width, self.height)?;
                }
            }
            // Release right away so duplication can move on while we encode,
            // unless a new pointer shape must still be read from this frame
            // (take_pointer does that after the encode is submitted).
            if self.shape_pending == 0 {
                self.release_frame();
            }
            Ok(Acquired::Frame {
                copy_us: acquired_at.elapsed().as_micros() as u32,
                present_us: proto::clock::qpc_to_us(info.LastPresentTime),
            })
        }
    }

    /// Pointer changes since the previous call, if any. Reads a pending
    /// pointer shape from the held frame and releases it; call this after
    /// the frame has been handed to the encoder.
    pub fn take_pointer(&mut self) -> Option<PointerUpdate> {
        if self.shape_pending > 0 && self.holding_frame {
            let size = std::mem::take(&mut self.shape_pending);
            // SAFETY: the frame that reported the shape is still held.
            match unsafe { self.read_shape(size) } {
                Ok(Some(shape)) => {
                    self.pointer
                        .get_or_insert(PointerUpdate {
                            x: 0,
                            y: 0,
                            shape: None,
                        })
                        .shape = Some(shape);
                }
                Ok(None) => {}
                Err(e) => {
                    let lost = e
                        .root_cause()
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|w| w.code() == DXGI_ERROR_ACCESS_LOST);
                    tracing::warn!("pointer shape unavailable: {e:#}");
                    if lost {
                        self.release_frame();
                        // SAFETY: no frame is held; recreating the duplication is allowed.
                        if let Err(e) = unsafe { self.reopen() } {
                            tracing::warn!("{e:#}");
                        }
                    }
                }
            }
            self.release_frame();
        }
        self.shape_pending = 0;
        self.pointer.take()
    }

    fn release_frame(&mut self) {
        if std::mem::take(&mut self.holding_frame) {
            // SAFETY: called exactly once per successful AcquireNextFrame.
            if let Some(dup) = &self.dup {
                let _ = unsafe { dup.ReleaseFrame() };
            }
        }
    }

    /// Read the current pointer shape and convert it to the wire layout.
    unsafe fn read_shape(&mut self, size: u32) -> Result<Option<CursorShape>> {
        self.shape_buf.resize(size as usize, 0);
        let mut required = 0;
        let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        self.dup
            .as_ref()
            .context("duplication lost")?
            .GetFramePointerShape(
                size,
                self.shape_buf.as_mut_ptr() as *mut _,
                &mut required,
                &mut si,
            )
            .context("GetFramePointerShape")?;
        let (w, pitch) = (si.Width as usize, si.Pitch as usize);
        let buf = &self.shape_buf;
        let (kind, height, data) = match DXGI_OUTDUPL_POINTER_SHAPE_TYPE(si.Type as i32) {
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME => {
                // AND mask rows then XOR mask rows; repack to WORD-aligned rows.
                let h = si.Height as usize / 2;
                let mp = mono_pitch(w as u16);
                let mut d = vec![0u8; 2 * h * mp];
                for row in 0..2 * h {
                    let n = mp.min(pitch);
                    d[row * mp..row * mp + n].copy_from_slice(&buf[row * pitch..row * pitch + n]);
                }
                (CursorKind::Monochrome, h, d)
            }
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR => {
                let h = si.Height as usize;
                let mut d = Vec::with_capacity(w * h * 4);
                for row in 0..h {
                    d.extend_from_slice(&buf[row * pitch..row * pitch + w * 4]);
                }
                (CursorKind::Color, h, d)
            }
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR => {
                // Alpha 0xFF means "XOR with the screen", 0 means "replace".
                let h = si.Height as usize;
                let mp = mono_pitch(w as u16);
                let mut d = vec![0u8; w * h * 4 + h * mp];
                let (colors, mask) = d.split_at_mut(w * h * 4);
                for row in 0..h {
                    for col in 0..w {
                        let px = &buf[row * pitch + col * 4..row * pitch + col * 4 + 4];
                        let o = (row * w + col) * 4;
                        colors[o..o + 3].copy_from_slice(&px[..3]);
                        if px[3] == 0xff {
                            mask[row * mp + col / 8] |= 0x80 >> (col % 8);
                        }
                    }
                }
                (CursorKind::MaskedColor, h, d)
            }
            other => {
                tracing::warn!("unknown pointer shape type {other:?}");
                return Ok(None);
            }
        };
        let id = self.next_shape_id;
        self.next_shape_id += 1;
        Ok(Some(CursorShape {
            shape_id: id,
            kind,
            width: w as u16,
            height: height as u16,
            hot_x: si.HotSpot.x as u16,
            hot_y: si.HotSpot.y as u16,
            data,
        }))
    }

    unsafe fn reopen(&mut self) -> Result<()> {
        let device: ID3D11Device = self.gpu_context.GetDevice()?;
        // The old duplication must be released before creating a new one.
        self.dup = None;
        // An exclusive-fullscreen mode switch can leave the old output object
        // stale (every DuplicateOutput1 on it fails), so look it up again.
        self.output = self.find_output(&device)?;
        let dup = self
            .output
            .DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
            .context("re-DuplicateOutput1 after ACCESS_LOST")?;
        if let Some(fit) = self.fit.as_mut() {
            fit.forget_sources();
        }
        let desc = dup.GetDesc();
        self.dup = Some(dup);
        tracing::info!(
            "desktop duplication re-opened: {}x{}",
            desc.ModeDesc.Width,
            desc.ModeDesc.Height
        );
        Ok(())
    }

    /// Enumerate the device's adapter afresh and return the output named
    /// `device_name`.
    unsafe fn find_output(&self, device: &ID3D11Device) -> Result<IDXGIOutput6> {
        let luid = device
            .cast::<IDXGIDevice>()?
            .GetAdapter()?
            .GetDesc()?
            .AdapterLuid;
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let l = adapter.GetDesc1()?.AdapterLuid;
            if (l.LowPart, l.HighPart) != (luid.LowPart, luid.HighPart) {
                continue;
            }
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                if output.GetDesc()?.DeviceName == self.device_name {
                    return Ok(output.cast()?);
                }
            }
        }
        bail!("monitor {} is gone", wide(&self.device_name))
    }

    /// Re-open a lost duplication. During a fullscreen or mode switch this
    /// fails for a while (E_ACCESSDENIED, DXGI_ERROR_UNSUPPORTED); keep the
    /// session alive and retry on the next acquire instead of erroring out.
    unsafe fn try_reopen(&mut self, timeout_ms: u32) -> bool {
        match self.reopen() {
            Ok(()) => {
                if let Some(t) = self.lost_since.take() {
                    tracing::info!("capture recovered after {:.1} s", t.elapsed().as_secs_f64());
                }
                self.lost = false;
                self.lost_reported = None;
                true
            }
            Err(e) => {
                let now = std::time::Instant::now();
                let since = *self.lost_since.get_or_insert(now);
                if self
                    .lost_reported
                    .is_none_or(|t| now - t >= std::time::Duration::from_secs(5))
                {
                    tracing::warn!(
                        "{e:#}; retrying (capture lost for {:.1} s)",
                        (now - since).as_secs_f64()
                    );
                    self.lost_reported = Some(now);
                }
                self.lost = true;
                // Don't spin while the switch completes.
                std::thread::sleep(std::time::Duration::from_millis(
                    timeout_ms.clamp(1, 16) as u64
                ));
                false
            }
        }
    }
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}
