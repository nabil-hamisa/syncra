//! Turn the host's cursor shape into a native Windows cursor. The client
//! shows it as its own hardware cursor, so cursor motion has zero network
//! latency, and monochrome/masked shapes keep their exact screen-inverting
//! behavior.

use anyhow::{bail, Result};
use proto::input::{mono_pitch, CursorKind, CursorShape};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyCursor, HCURSOR, ICONINFO,
};

pub struct NativeCursor(pub HCURSOR);

// SAFETY: HCURSOR is a process-global handle usable from any thread.
unsafe impl Send for NativeCursor {}

impl Drop for NativeCursor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyCursor(self.0);
        }
    }
}

pub fn create(shape: &CursorShape) -> Result<NativeCursor> {
    let (w, h) = (shape.width as i32, shape.height as i32);
    if w == 0 || h == 0 {
        bail!("empty cursor shape");
    }
    // SAFETY: bitmaps are owned by `Bitmap` guards and freed on every path;
    // `CursorShape::decode` guarantees `data` matches the size of `kind`.
    unsafe {
        let (mask, color) = match shape.kind {
            CursorKind::Monochrome => (mono_bitmap(w, 2 * h, &shape.data)?, None),
            CursorKind::Color => {
                // All-zero AND mask: the alpha channel of the color bitmap decides.
                let color = color_bitmap(w, h, &shape.data)?;
                let zeros = vec![0u8; h as usize * mono_pitch(shape.width)];
                (mono_bitmap(w, h, &zeros)?, Some(color))
            }
            CursorKind::MaskedColor => {
                let (colors, mask) = shape.data.split_at(w as usize * h as usize * 4);
                let color = color_bitmap(w, h, colors)?;
                (mono_bitmap(w, h, mask)?, Some(color))
            }
        };
        let info = ICONINFO {
            fIcon: false.into(),
            xHotspot: shape.hot_x as u32,
            yHotspot: shape.hot_y as u32,
            hbmMask: mask.0,
            hbmColor: color.as_ref().map(|c| c.0).unwrap_or_default(),
        };
        // CreateIconIndirect copies the bitmaps; the guards free ours.
        let icon = CreateIconIndirect(&info)?;
        Ok(NativeCursor(HCURSOR(icon.0)))
    }
}

/// Owns an HBITMAP and deletes it on drop.
struct Bitmap(HBITMAP);

impl Drop for Bitmap {
    fn drop(&mut self) {
        // SAFETY: the bitmap was created by us and is not selected into any DC.
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}

unsafe fn mono_bitmap(w: i32, h: i32, bits: &[u8]) -> Result<Bitmap> {
    let bmp = CreateBitmap(w, h, 1, 1, Some(bits.as_ptr() as *const _));
    if bmp.is_invalid() {
        bail!("CreateBitmap failed");
    }
    Ok(Bitmap(bmp))
}

/// 32bpp top-down DIB section filled with BGRA pixels.
unsafe fn color_bitmap(w: i32, h: i32, bgra: &[u8]) -> Result<Bitmap> {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    let bmp = Bitmap(CreateDIBSection(
        None,
        &bmi,
        DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    )?);
    // SAFETY: the DIB section holds w*h 32bpp pixels and bgra has exactly that many bytes.
    std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, (w * h * 4) as usize);
    Ok(bmp)
}
