//! Synthetic worst-case content for network benchmarks: scrolling noise that
//! keeps NVENC at its full CBR target (like fast game motion), generated on
//! the GPU with one region copy per frame instead of desktop capture.

use anyhow::Result;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

pub struct TestPattern {
    noise: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
}

impl TestPattern {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        // Twice as wide as the output so a scrolling window always fits.
        let (w, h) = (width * 2, height);
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for (i, px) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let (x, y) = ((i as u32 % w), (i as u32 / w));
            // Detailed structure (rings, stripes, checker) with light grain:
            // costly at high quality like fast game motion, but compressible,
            // so the encoder can follow its bitrate target. Pure random noise
            // cannot be squeezed below ~38 KB/frame at any quantizer.
            let n = (rng & 0x1f) as u8;
            let (fx, fy) = (x as f32, y as f32);
            let rings = (((fx - 900.0).hypot(fy - 500.0) / 9.0).sin() * 90.0 + 110.0) as u8;
            let stripes = ((fx / 23.0 + fy / 37.0).sin() * 70.0 + 90.0) as u8;
            let checker = if ((x / 48) + (y / 48)) % 2 == 0 {
                60
            } else {
                0
            };
            px[0] = rings.saturating_add(n);
            px[1] = stripes.saturating_add(checker).saturating_add(n / 2);
            px[2] = ((fx / 4.0).sin() * 50.0 + 120.0) as u8 ^ n;
            px[3] = 255;
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const _,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut noise = None;
        // SAFETY: `init` points at w*h BGRA pixels that outlive the call.
        unsafe {
            device.CreateTexture2D(&desc, Some(&init), Some(&mut noise))?;
            Ok(TestPattern {
                noise: noise.unwrap(),
                context: device.GetImmediateContext()?,
                width,
                height,
            })
        }
    }

    /// Render frame `n` into `target` (scrolls 13 px per frame).
    pub fn render(&self, target: &ID3D11Texture2D, n: u64) {
        let x = ((n * 13) % self.width as u64) as u32;
        let src = D3D11_BOX {
            left: x,
            top: 0,
            front: 0,
            right: x + self.width,
            bottom: self.height,
            back: 1,
        };
        // SAFETY: both textures are BGRA8 on the same device; the box is in bounds.
        unsafe {
            self.context
                .CopySubresourceRegion(target, 0, 0, 0, 0, &self.noise, 0, Some(&src));
        }
    }
}
