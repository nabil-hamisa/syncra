//! Win32 window + D3D11 flip-model swapchain tuned for latency:
//! FLIP_DISCARD, max frame latency 1, waitable object, and tearing
//! (`Present(0, ALLOW_TEARING)`) so a frame reaches scanout immediately.
//!
//! A full-screen triangle converts NV12 planes (BT.709 limited range, which is
//! what NVENC produces from RGB) to RGB, letterboxed to the window.

use anyhow::{bail, Context, Result};
use windows::core::{s, Interface, BOOL, PCSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::WaitForSingleObjectEx;

use crate::decode::Planes;
use crate::window::{self, WindowHandle};

const SHADER: &str = r#"
Texture2D<float>  texY  : register(t0);
Texture2D<float2> texUV : register(t1);
SamplerState samp : register(s0);

struct VsOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

float4 ps_main(VsOut i) : SV_Target {
    // BT.709 limited range -> full range RGB.
    float y = (texY.Sample(samp, i.uv) - 16.0 / 255.0) * (255.0 / 219.0);
    // H.264 default chroma siting is left (co-sited with even luma columns),
    // so shift half a luma pixel right relative to centered sampling.
    float lumaW, lumaH;
    texY.GetDimensions(lumaW, lumaH);
    float2 c = (texUV.Sample(samp, i.uv + float2(0.5 / lumaW, 0)) - 128.0 / 255.0) * (255.0 / 224.0);
    float3 rgb = float3(
        y + 1.5748 * c.y,
        y - 0.1873 * c.x - 0.4681 * c.y,
        y + 1.8556 * c.x);
    return float4(saturate(rgb), 1);
}
"#;

const SWAP_FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG(
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 | DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0,
);

pub struct Renderer {
    pub device: ID3D11Device,
    context: ID3D11DeviceContext,
    swapchain: IDXGISwapChain2,
    waitable: HANDLE,
    rtv: Option<ID3D11RenderTargetView>,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    srvs: Option<[ID3D11ShaderResourceView; 2]>,
    srv_source: usize,
    width: u32,
    height: u32,
    tearing: bool,
}

impl Renderer {
    /// Create the device and a swapchain for `window`. May run on any thread.
    pub fn new(window: WindowHandle, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let (device, context, factory) = create_device()?;
            let hwnd = window.hwnd();

            let mut allow_tearing = BOOL(0);
            let tearing = factory
                .cast::<IDXGIFactory5>()
                .and_then(|f5| {
                    f5.CheckFeatureSupport(
                        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                        &mut allow_tearing as *mut _ as *mut _,
                        std::mem::size_of::<BOOL>() as u32,
                    )
                })
                .is_ok()
                && allow_tearing.as_bool();

            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_NONE,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: SWAP_FLAGS.0 as u32,
                ..Default::default()
            };
            let swapchain: IDXGISwapChain2 = factory
                .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)?
                .cast()?;
            factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER)?;
            swapchain.SetMaximumFrameLatency(1)?;
            let waitable = swapchain.GetFrameLatencyWaitableObject();

            let vs_blob = compile(SHADER, s!("vs_main"), s!("vs_5_0"))?;
            let ps_blob = compile(SHADER, s!("ps_main"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps))?;

            let sdesc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sdesc, Some(&mut sampler))?;

            let mut r = Renderer {
                device,
                context,
                swapchain: swapchain.clone(),
                waitable,
                rtv: None,
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                sampler: sampler.unwrap(),
                srvs: None,
                srv_source: 0,
                width,
                height,
                tearing,
            };
            r.create_rtv()?;
            tracing::info!(
                "swapchain: flip-discard, max latency 1, tearing {}",
                if tearing { "ON" } else { "unsupported" }
            );
            Ok(r)
        }
    }

    unsafe fn create_rtv(&mut self) -> Result<()> {
        let back: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
        let mut rtv = None;
        self.device
            .CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        self.rtv = rtv;
        Ok(())
    }

    /// Apply pending window resizes. Returns false once the window is closed.
    pub fn poll(&mut self) -> Result<bool> {
        if let Some((w, h)) = window::take_resize() {
            if (w, h) != (self.width, self.height) {
                unsafe {
                    self.rtv = None;
                    self.context.ClearState();
                    self.swapchain
                        .ResizeBuffers(0, w, h, DXGI_FORMAT_UNKNOWN, SWAP_FLAGS)?;
                    self.width = w;
                    self.height = h;
                    self.create_rtv()?;
                }
            }
        }
        Ok(!window::is_closed())
    }

    /// Block until the swapchain can accept a new frame (bounded by `timeout_ms`).
    pub fn wait_ready(&self, timeout_ms: u32) {
        unsafe {
            WaitForSingleObjectEx(self.waitable, timeout_ms, true);
        }
    }

    /// Draw the decoded planes and present immediately.
    pub fn present(&mut self, planes: &Planes) -> Result<()> {
        unsafe {
            let Some(rtv) = self.rtv.clone() else {
                bail!("no render target")
            };
            let viewport = letterbox(self.width, self.height, planes.width, planes.height);
            self.draw(&rtv, viewport, planes)?;
            let flags = if self.tearing {
                DXGI_PRESENT_ALLOW_TEARING
            } else {
                DXGI_PRESENT(0)
            };
            self.swapchain.Present(0, flags).ok().context("Present")?;
        }
        Ok(())
    }

    /// Render `planes` at native size into an offscreen target and return the
    /// tightly packed BGRA pixels. Used to verify the decode + convert path.
    pub fn read_back(&mut self, planes: &Planes) -> Result<Vec<u8>> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC {
                Width: planes.width,
                Height: planes.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut target = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut target))?;
            let target = target.unwrap();
            let mut rtv = None;
            self.device
                .CreateRenderTargetView(&target, None, Some(&mut rtv))?;
            let viewport = letterbox(planes.width, planes.height, planes.width, planes.height);
            self.draw(&rtv.unwrap(), viewport, planes)?;

            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))?;
            let staging = staging.unwrap();
            self.context.CopyResource(&staging, &target);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row = planes.width as usize * 4;
            let mut out = Vec::with_capacity(row * planes.height as usize);
            for y in 0..planes.height as usize {
                let p = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize);
                out.extend_from_slice(std::slice::from_raw_parts(p, row));
            }
            self.context.Unmap(&staging, 0);
            Ok(out)
        }
    }

    unsafe fn draw(
        &mut self,
        rtv: &ID3D11RenderTargetView,
        viewport: D3D11_VIEWPORT,
        planes: &Planes,
    ) -> Result<()> {
        let key = planes.y.as_raw() as usize;
        if self.srvs.is_none() || self.srv_source != key {
            let mut y = None;
            let mut uv = None;
            self.device
                .CreateShaderResourceView(&planes.y, None, Some(&mut y))?;
            self.device
                .CreateShaderResourceView(&planes.uv, None, Some(&mut uv))?;
            self.srvs = Some([y.unwrap(), uv.unwrap()]);
            self.srv_source = key;
        }
        let ctx = &self.context;
        ctx.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 1.0]);
        ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
        ctx.RSSetViewports(Some(&[viewport]));
        ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        ctx.VSSetShader(&self.vs, None);
        ctx.PSSetShader(&self.ps, None);
        let srvs = self.srvs.as_ref().unwrap();
        ctx.PSSetShaderResources(0, Some(&[Some(srvs[0].clone()), Some(srvs[1].clone())]));
        ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        ctx.Draw(3, 0);
        Ok(())
    }
}

fn letterbox(win_w: u32, win_h: u32, src_w: u32, src_h: u32) -> D3D11_VIEWPORT {
    let scale = (win_w as f32 / src_w as f32).min(win_h as f32 / src_h as f32);
    let (w, h) = (src_w as f32 * scale, src_h as f32 * scale);
    D3D11_VIEWPORT {
        TopLeftX: (win_w as f32 - w) / 2.0,
        TopLeftY: (win_h as f32 - h) / 2.0,
        Width: w,
        Height: h,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

unsafe fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIFactory2)> {
    let factory: IDXGIFactory2 = CreateDXGIFactory1()?;
    let mut i = 0;
    let adapter = loop {
        let a = factory
            .EnumAdapters1(i)
            .context("no NVIDIA adapter found (NVDEC required)")?;
        if a.GetDesc1()?.VendorId == 0x10de {
            break a;
        }
        i += 1;
    };
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
    )?;
    let device = device.unwrap();
    // CUDA interop and the renderer share this device; make it safe for that.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        let _ = mt.SetMultithreadProtected(true);
    }
    // Decode-to-present work must not queue behind other GPU clients.
    if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
        if let Err(e) = dxgi.SetGPUThreadPriority(7) {
            tracing::warn!("SetGPUThreadPriority failed (running without GPU priority boost): {e}");
        }
    }
    Ok((device, context.unwrap(), factory))
}

unsafe fn compile(src: &str, entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let r = D3DCompile(
        src.as_ptr() as *const _,
        src.len(),
        None,
        None,
        None,
        entry,
        target,
        windows::Win32::Graphics::Direct3D::Fxc::D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Err(e) = r {
        let msg = errors
            .map(|b| String::from_utf8_lossy(blob_bytes(&b)).into_owned())
            .unwrap_or_default();
        bail!("shader compile failed: {e} {msg}");
    }
    Ok(code.unwrap())
}

unsafe fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())
}
