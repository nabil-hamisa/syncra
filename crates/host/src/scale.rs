//! GPU downscaling of captured frames for adaptive resolution.
//!
//! On a slow link, a lower resolution at full frame rate looks far better
//! than 1080p starved of bits. The scaler renders each captured BGRA frame
//! into a smaller BGRA texture (registered with NVENC like the capture ring)
//! with a box filter sized to the scale factor: `ceil(scale)²` bilinear taps
//! spread over the output pixel's footprint, so 2× and 3× are exact box
//! filters and nothing aliases.

use anyhow::{bail, Result};
use windows::core::{s, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;

/// Scale ladder, as fractions of the capture size (1080p → 720p, 540p, 360p).
pub const LADDER: [(u32, u32); 4] = [(1, 1), (2, 3), (1, 2), (1, 3)];

const SHADER: &str = r#"
Texture2D src : register(t0);
SamplerState smp : register(s0);
cbuffer Params : register(b0) {
    float2 step;   // tap spacing in source UV
    float taps;    // taps per axis
    float pad;
};
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD; };
V vs(uint id : SV_VertexID) {
    V o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
float4 ps(V i) : SV_Target {
    int n = (int)taps;
    float2 start = i.uv - step * (taps - 1) * 0.5;
    float3 acc = 0;
    for (int y = 0; y < n; y++)
        for (int x = 0; x < n; x++)
            acc += src.SampleLevel(smp, start + step * float2(x, y), 0).rgb;
    return float4(acc / (taps * taps), 1);
}
"#;

#[repr(C)]
struct Params {
    step: [f32; 2],
    taps: f32,
    _pad: f32,
}

pub struct Level {
    pub width: u32,
    pub height: u32,
    /// One target per capture ring slot.
    pub targets: Vec<ID3D11Texture2D>,
    rtvs: Vec<ID3D11RenderTargetView>,
    params: ID3D11Buffer,
}

pub struct Scaler {
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    srvs: Vec<ID3D11ShaderResourceView>,
    /// Ladder levels below native (index 0 here = `LADDER[1]`).
    pub levels: Vec<Level>,
}

/// Even dimensions (4:2:0 needs them), at least 64 px.
fn scaled(v: u32, (n, d): (u32, u32)) -> u32 {
    ((v * n / d) & !1).max(64)
}

impl Scaler {
    pub fn new(
        device: &ID3D11Device,
        ring: &[ID3D11Texture2D],
        width: u32,
        height: u32,
    ) -> Result<Self> {
        // SAFETY: D3D11 object creation on a valid device; descriptors and
        // shader blobs outlive the calls that read them.
        unsafe {
            let vsb = compile(SHADER, s!("vs"), s!("vs_5_0"))?;
            let psb = compile(SHADER, s!("ps"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(blob(&vsb), None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(blob(&psb), None, Some(&mut ps))?;
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;
            let mut srvs = Vec::new();
            for t in ring {
                let mut v = None;
                device.CreateShaderResourceView(t, None, Some(&mut v))?;
                srvs.push(v.unwrap());
            }
            let mut levels = Vec::new();
            for &f in &LADDER[1..] {
                let (w, h) = (scaled(width, f), scaled(height, f));
                let scale = width as f32 / w as f32;
                let taps = scale.ceil().max(1.0);
                // Spread taps over the footprint of one output pixel.
                let step = [scale / taps / width as f32, scale / taps / height as f32];
                let p = Params {
                    step,
                    taps,
                    _pad: 0.0,
                };
                let bd = D3D11_BUFFER_DESC {
                    ByteWidth: std::mem::size_of::<Params>() as u32,
                    Usage: D3D11_USAGE_IMMUTABLE,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                };
                let init = D3D11_SUBRESOURCE_DATA {
                    pSysMem: &p as *const _ as *const _,
                    ..Default::default()
                };
                let mut params = None;
                device.CreateBuffer(&bd, Some(&init), Some(&mut params))?;
                let targets = crate::capture::create_ring(device, w, h, ring.len())?;
                let mut rtvs = Vec::new();
                for t in &targets {
                    let mut v = None;
                    device.CreateRenderTargetView(t, None, Some(&mut v))?;
                    rtvs.push(v.unwrap());
                }
                levels.push(Level {
                    width: w,
                    height: h,
                    targets,
                    rtvs,
                    params: params.unwrap(),
                });
            }
            Ok(Scaler {
                context: device.GetImmediateContext()?,
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                sampler: sampler.unwrap(),
                srvs,
                levels,
            })
        }
    }

    /// Downscale capture ring slot `slot` into `levels[level].targets[slot]`.
    pub fn downscale(&self, slot: usize, level: usize) {
        let l = &self.levels[level];
        // SAFETY: all bound objects belong to the same device and outlive the
        // draw; the source SRV and the target RTV are different textures.
        unsafe {
            let c = &self.context;
            c.OMSetRenderTargets(Some(&[Some(l.rtvs[slot].clone())]), None);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: l.width as f32,
                Height: l.height as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.PSSetShader(&self.ps, None);
            c.PSSetShaderResources(0, Some(&[Some(self.srvs[slot].clone())]));
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.PSSetConstantBuffers(0, Some(&[Some(l.params.clone())]));
            c.Draw(3, 0);
            // Unbind so the capture copy into this texture never conflicts.
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(None, None);
        }
    }
}

const FIT_SHADER: &str = r#"
Texture2D src : register(t0);
SamplerState smp : register(s0);
struct V { float4 pos : SV_Position; float2 uv : TEXCOORD; };
V vs(uint id : SV_VertexID) {
    V o;
    o.uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(o.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}
float4 ps(V i) : SV_Target {
    return float4(src.SampleLevel(smp, i.uv, 0).rgb, 1);
}
"#;

/// Stretches a desktop image whose size differs from the capture ring (an
/// exclusive-fullscreen game switched the display mode) onto a ring texture.
/// `CopyResource` silently does nothing on mismatched sizes, which streamed
/// stale or uninitialized pixels instead of the game.
pub struct Fit {
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    /// Views keyed by texture pointer. A view holds a reference to its
    /// texture, so a cached pointer can't be reused by another texture.
    srvs: Vec<(usize, ID3D11ShaderResourceView)>,
    rtvs: Vec<(usize, ID3D11RenderTargetView)>,
}

/// Duplication cycles through a few surfaces and the ring is small; more
/// entries than this means the source textures were recreated.
const FIT_CACHE: usize = 16;

impl Fit {
    pub fn new(context: &ID3D11DeviceContext) -> Result<Self> {
        // SAFETY: D3D11 object creation on a valid device; descriptors and
        // shader blobs outlive the calls that read them.
        unsafe {
            let device = context.GetDevice()?;
            let vsb = compile(FIT_SHADER, s!("vs"), s!("vs_5_0"))?;
            let psb = compile(FIT_SHADER, s!("ps"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(blob(&vsb), None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(blob(&psb), None, Some(&mut ps))?;
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;
            Ok(Fit {
                context: context.clone(),
                vs: vs.unwrap(),
                ps: ps.unwrap(),
                sampler: sampler.unwrap(),
                srvs: Vec::with_capacity(FIT_CACHE),
                rtvs: Vec::with_capacity(FIT_CACHE),
            })
        }
    }

    /// Drop cached source views (the duplication was recreated).
    pub fn forget_sources(&mut self) {
        self.srvs.clear();
    }

    /// Stretch `src` over all of `dst` (`dst_w`×`dst_h`).
    pub fn blit(
        &mut self,
        src: &ID3D11Texture2D,
        dst: &ID3D11Texture2D,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<()> {
        // SAFETY: src and dst are distinct textures on this context's device;
        // bound objects outlive the draw and are unbound afterwards.
        unsafe {
            let device = self.context.GetDevice()?;
            let srv = cached(&mut self.srvs, src.as_raw() as usize, || {
                let mut v = None;
                device.CreateShaderResourceView(src, None, Some(&mut v))?;
                Ok(v.unwrap())
            })?;
            let rtv = cached(&mut self.rtvs, dst.as_raw() as usize, || {
                let mut v = None;
                device.CreateRenderTargetView(dst, None, Some(&mut v))?;
                Ok(v.unwrap())
            })?;
            let c = &self.context;
            c.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            c.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: dst_w as f32,
                Height: dst_h as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            c.IASetInputLayout(None);
            c.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.PSSetShader(&self.ps, None);
            c.PSSetShaderResources(0, Some(&[Some(srv)]));
            c.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            c.Draw(3, 0);
            c.PSSetShaderResources(0, Some(&[None]));
            c.OMSetRenderTargets(None, None);
        }
        Ok(())
    }
}

fn cached<V: Clone>(
    cache: &mut Vec<(usize, V)>,
    key: usize,
    create: impl FnOnce() -> Result<V>,
) -> Result<V> {
    if let Some((_, v)) = cache.iter().find(|(k, _)| *k == key) {
        return Ok(v.clone());
    }
    if cache.len() >= FIT_CACHE {
        cache.clear();
    }
    let v = create()?;
    cache.push((key, v.clone()));
    Ok(v)
}

unsafe fn compile(src: &str, entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let (mut code, mut errors) = (None, None);
    let r = D3DCompile(
        src.as_ptr() as *const _,
        src.len(),
        None,
        None,
        None,
        entry,
        target,
        D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Err(e) = r {
        let msg = errors
            .map(|b| String::from_utf8_lossy(blob(&b)).into_owned())
            .unwrap_or_default();
        bail!("scaler shader compile failed: {e} {msg}");
    }
    Ok(code.unwrap())
}

unsafe fn blob(b: &ID3DBlob) -> &[u8] {
    std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize())
}
