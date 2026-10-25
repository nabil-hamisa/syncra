//! NVENC H.264 encoder tuned for minimum latency.
//!
//! - Preset P1 + ULTRA_LOW_LATENCY tuning, CBR with a single-frame VBV.
//! - Infinite GOP, no B-frames, intra refresh instead of periodic IDRs.
//! - N slices per picture with sub-frame readback: slices are handed to the
//!   caller as soon as the hardware writes them, so transmission can start
//!   before the whole frame is encoded.

use std::ffi::c_void;
use std::ptr;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use nvenc_sys::*;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

/// Most reference frames we ask for (how far back a frame may predict, which
/// bounds reference invalidation). The real value also depends on resolution.
const MAX_REF_FRAMES: u32 = 16;
/// H.264 level 5.1 decoded picture buffer, in macroblocks (Table A-1).
const LEVEL_51_MAX_DPB_MBS: u32 = 184_320;

/// NVENC reports this in `hwEncodeStatus` once the whole picture is written.
const HW_ENCODE_STATUS_COMPLETE: u32 = 2;

#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub slices: u32,
    pub subframe: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EncodeTiming {
    /// Encode submit → first bitstream bytes available.
    pub first_chunk_us: u32,
    /// Encode submit → picture complete.
    pub total_us: u32,
    pub bytes: u32,
    pub chunks: u32,
    pub idr: bool,
}

pub struct Encoder {
    api: &'static Api,
    enc: *mut c_void,
    /// Registered input textures with their size.
    registered: Vec<(NV_ENC_REGISTERED_PTR, u32, u32)>,
    bitstream: NV_ENC_OUTPUT_PTR,
    slice_offsets: Vec<u32>,
    cfg: EncoderConfig,
    subframe: bool,
    frame_idx: u32,
    /// Kept alive for `nvEncReconfigureEncoder`; `init.encodeConfig` points at `config`.
    config: Box<NV_ENC_CONFIG>,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    bitrate_bps: u32,
    ref_invalidation: bool,
    ref_frames: u32,
}

macro_rules! nv {
    ($api:expr, $f:ident ( $($arg:expr),* $(,)? )) => {{
        let f = $api.fns.$f.expect(concat!(stringify!($f), " missing from NVENC function list"));
        let s = f($($arg),*);
        if s != _NVENCSTATUS_NV_ENC_SUCCESS {
            bail!("{} failed: {}", stringify!($f), status_name(s));
        }
    }};
}

impl Encoder {
    /// Open an NVENC session on `device` and register `inputs` (BGRA textures
    /// from the capture ring) as encoder input resources.
    pub fn new(
        device: &ID3D11Device,
        inputs: &[ID3D11Texture2D],
        cfg: EncoderConfig,
    ) -> Result<Self> {
        let api = nvenc_sys::api().map_err(anyhow::Error::msg)?;
        tracing::info!(
            "NVENC driver API {}.{}, header API {}.{}",
            api.driver_max_version >> 4,
            api.driver_max_version & 0xf,
            NVENCAPI_MAJOR_VERSION,
            NVENCAPI_MINOR_VERSION
        );

        unsafe {
            let mut open: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
            open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            open.deviceType = _NV_ENC_DEVICE_TYPE_NV_ENC_DEVICE_TYPE_DIRECTX;
            open.device = device.as_raw();
            open.apiVersion = NVENCAPI_VERSION;
            let mut enc = ptr::null_mut();
            nv!(api, nvEncOpenEncodeSessionEx(&mut open, &mut enc));

            let mut this = Encoder {
                api,
                enc,
                registered: Vec::new(),
                bitstream: ptr::null_mut(),
                slice_offsets: vec![0; cfg.slices.max(1) as usize],
                cfg,
                subframe: false,
                frame_idx: 0,
                // SAFETY: all-zero is a valid (unused) value for these plain C structs.
                config: Box::new(std::mem::zeroed()),
                init: Box::new(std::mem::zeroed()),
                bitrate_bps: cfg.bitrate_bps,
                ref_invalidation: false,
                ref_frames: 1,
            };
            this.init(inputs).context("NVENC init")?;
            Ok(this)
        }
    }

    unsafe fn caps(&self, cap: NV_ENC_CAPS) -> i32 {
        let mut p: NV_ENC_CAPS_PARAM = std::mem::zeroed();
        p.version = NV_ENC_CAPS_PARAM_VER;
        p.capsToQuery = cap;
        let mut v = 0;
        let f = self.api.fns.nvEncGetEncodeCaps.unwrap();
        if f(self.enc, NV_ENC_CODEC_H264_GUID, &mut p, &mut v) != _NVENCSTATUS_NV_ENC_SUCCESS {
            return 0;
        }
        v
    }

    unsafe fn init(&mut self, inputs: &[ID3D11Texture2D]) -> Result<()> {
        let api = self.api;
        let cfg = self.cfg;

        self.subframe =
            cfg.subframe && self.caps(_NV_ENC_CAPS_NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK) != 0;
        if cfg.subframe && !self.subframe {
            tracing::warn!(
                "GPU does not support sub-frame readback; falling back to whole-frame output"
            );
        }

        let mut preset: NV_ENC_PRESET_CONFIG = std::mem::zeroed();
        preset.version = NV_ENC_PRESET_CONFIG_VER;
        preset.presetCfg.version = NV_ENC_CONFIG_VER;
        nv!(
            api,
            nvEncGetEncodePresetConfigEx(
                self.enc,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P1_GUID,
                NV_ENC_TUNING_INFO_NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                &mut preset,
            )
        );

        let mut config = preset.presetCfg;
        config.version = NV_ENC_CONFIG_VER;
        config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        config.gopLength = NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1; // IPPP, no B-frames

        let rc = &mut config.rcParams;
        rc.version = NV_ENC_RC_PARAMS_VER;
        rc.rateControlMode = _NV_ENC_PARAMS_RC_MODE_NV_ENC_PARAMS_RC_CBR;
        rc.averageBitRate = cfg.bitrate_bps;
        rc.maxBitRate = cfg.bitrate_bps;
        // Single-frame VBV: a frame never needs more than one frame interval on the wire.
        rc.vbvBufferSize = cfg.bitrate_bps / cfg.fps;
        rc.vbvInitialDelay = rc.vbvBufferSize;
        rc.multiPass = _NV_ENC_MULTI_PASS_NV_ENC_MULTI_PASS_DISABLED;
        rc.set_zeroReorderDelay(1);
        rc.set_enableLookahead(0);
        rc.set_enableAQ(1);

        let h264 = &mut config.encodeCodecConfig.h264Config;
        h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        h264.set_repeatSPSPPS(1);
        h264.set_enableIntraRefresh(1);
        h264.intraRefreshPeriod = cfg.fps * 2;
        h264.intraRefreshCnt = (cfg.fps / 10).max(4);
        h264.sliceMode = 3; // fixed number of slices per picture
        h264.sliceModeData = cfg.slices.max(1);
        // Reference invalidation can only skip a lost frame if an older clean
        // one is still in the DPB. 16 frames cover RTTs up to ~130 ms at 120 fps;
        // at 1080p that needs level 5.1 (level 4.x allows only 4).
        // The DPB limit is in macroblocks: 16 frames fit at 1080p, ~12 at 1440p,
        // 5 at 4K.
        let mbs = cfg.width.div_ceil(16) * cfg.height.div_ceil(16);
        self.ref_frames = (LEVEL_51_MAX_DPB_MBS / mbs.max(1)).clamp(1, MAX_REF_FRAMES);
        h264.level = _NV_ENC_LEVEL_NV_ENC_LEVEL_H264_51 as u32;
        h264.maxNumRefFrames = self.ref_frames;

        // NVENC's internal ARGB→YUV conversion is BT.709 limited range
        // (measured with `host --color-probe`); signal it so decoders agree.
        let vui = &mut h264.h264VUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFormat = _NV_ENC_VUI_VIDEO_FORMAT_NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 0;
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = _NV_ENC_VUI_COLOR_PRIMARIES_NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        vui.transferCharacteristics =
            _NV_ENC_VUI_TRANSFER_CHARACTERISTIC_NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        vui.colourMatrix = _NV_ENC_VUI_MATRIX_COEFFS_NV_ENC_VUI_MATRIX_COEFFS_BT709;

        let mut init: NV_ENC_INITIALIZE_PARAMS = std::mem::zeroed();
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = NV_ENC_CODEC_H264_GUID;
        init.presetGUID = NV_ENC_PRESET_P1_GUID;
        init.tuningInfo = NV_ENC_TUNING_INFO_NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.encodeWidth = cfg.width;
        init.encodeHeight = cfg.height;
        init.darWidth = cfg.width;
        init.darHeight = cfg.height;
        init.maxEncodeWidth = cfg.width;
        init.maxEncodeHeight = cfg.height;
        init.frameRateNum = cfg.fps;
        init.frameRateDen = 1;
        init.enablePTD = 1;
        init.enableEncodeAsync = 0; // required for slice offsets / sub-frame readback
        if self.subframe {
            init.set_reportSliceOffsets(1);
            init.set_enableSubFrameWrite(1);
        }
        // Box both so the pointer stays valid for later reconfigures.
        *self.config = config;
        *self.init = init;
        self.init.encodeConfig = &mut *self.config;
        nv!(api, nvEncInitializeEncoder(self.enc, &mut *self.init));
        self.ref_invalidation =
            self.caps(_NV_ENC_CAPS_NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION) != 0;

        self.register(inputs, cfg.width, cfg.height)?;

        let mut bs: NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
        bs.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        nv!(api, nvEncCreateBitstreamBuffer(self.enc, &mut bs));
        self.bitstream = bs.bitstreamBuffer;

        tracing::info!(
            "NVENC ready: {}x{}@{} H.264 CBR {:.1} Mbps, {} slices, sub-frame readback {}, ref invalidation {}",
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_bps as f64 / 1e6,
            cfg.slices,
            if self.subframe { "ON" } else { "off" },
            if self.ref_invalidation { "ON" } else { "off" }
        );
        Ok(())
    }

    /// Register more input textures (`width` x `height`, at most the size
    /// the encoder was created with). Returns the index of the first one for
    /// [`Encoder::encode`].
    pub fn register(
        &mut self,
        inputs: &[ID3D11Texture2D],
        width: u32,
        height: u32,
    ) -> Result<usize> {
        let api = self.api;
        let first = self.registered.len();
        for tex in inputs {
            // SAFETY: valid session; the texture outlives its registration
            // (unregistered in Drop before the textures are released).
            unsafe {
                let mut reg: NV_ENC_REGISTER_RESOURCE = std::mem::zeroed();
                reg.version = NV_ENC_REGISTER_RESOURCE_VER;
                reg.resourceType = _NV_ENC_INPUT_RESOURCE_TYPE_NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
                reg.width = width;
                reg.height = height;
                reg.resourceToRegister = tex.as_raw();
                reg.bufferFormat = _NV_ENC_BUFFER_FORMAT_NV_ENC_BUFFER_FORMAT_ARGB;
                reg.bufferUsage = _NV_ENC_BUFFER_USAGE_NV_ENC_INPUT_IMAGE;
                nv!(api, nvEncRegisterResource(self.enc, &mut reg));
                self.registered
                    .push((reg.registeredResource, width, height));
            }
        }
        Ok(first)
    }

    /// Switch the encoded size (up to the creation size) with
    /// `nvEncReconfigureEncoder`: resets the encoder, and the next picture is
    /// an IDR with new SPS/PPS, so the decoder follows on its own.
    pub fn set_resolution(&mut self, width: u32, height: u32) -> Result<()> {
        if (width, height) == (self.init.encodeWidth, self.init.encodeHeight) {
            return Ok(());
        }
        let api = self.api;
        let mut init = *self.init;
        init.encodeWidth = width;
        init.encodeHeight = height;
        init.darWidth = width;
        init.darHeight = height;
        // SAFETY: `init` points at the boxed config, valid for the call.
        unsafe {
            let mut p: NV_ENC_RECONFIGURE_PARAMS = std::mem::zeroed();
            p.version = NV_ENC_RECONFIGURE_PARAMS_VER;
            p.reInitEncodeParams = init;
            p.reInitEncodeParams.encodeConfig = &mut *self.config;
            p.set_resetEncoder(1);
            p.set_forceIDR(1);
            nv!(api, nvEncReconfigureEncoder(self.enc, &mut p));
        }
        *self.init = init;
        self.init.encodeConfig = &mut *self.config;
        Ok(())
    }

    /// Change the CBR target without a keyframe (`nvEncReconfigureEncoder`).
    /// The single-frame VBV follows, so frame size stays bounded by one
    /// frame interval at the new rate.
    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == self.bitrate_bps {
            return Ok(());
        }
        let api = self.api;
        // Work on a copy and commit only if the encoder accepts it, so a
        // rejected reconfigure leaves the stored config matching the encoder.
        let mut config = *self.config;
        let rc = &mut config.rcParams;
        rc.averageBitRate = bps;
        rc.maxBitRate = bps;
        rc.vbvBufferSize = bps / self.cfg.fps;
        rc.vbvInitialDelay = rc.vbvBufferSize;
        // SAFETY: `config` and the copied init params are fully initialized
        // locals that outlive the synchronous call.
        unsafe {
            let mut p: NV_ENC_RECONFIGURE_PARAMS = std::mem::zeroed();
            p.version = NV_ENC_RECONFIGURE_PARAMS_VER;
            p.reInitEncodeParams = *self.init;
            p.reInitEncodeParams.encodeConfig = &mut config;
            p.set_resetEncoder(0);
            p.set_forceIDR(0);
            nv!(api, nvEncReconfigureEncoder(self.enc, &mut p));
        }
        *self.config = config;
        self.bitrate_bps = bps;
        Ok(())
    }

    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    /// How many past frames the encoder keeps as references.
    pub fn ref_frames(&self) -> u32 {
        self.ref_frames
    }

    pub fn supports_ref_invalidation(&self) -> bool {
        self.ref_invalidation
    }

    /// Index the next encoded picture will get (also its NVENC timestamp).
    pub fn next_frame_index(&self) -> u32 {
        self.frame_idx
    }

    /// Stop referencing the picture encoded with index `frame`: later frames
    /// predict only from pictures the client has. Returns false if
    /// unsupported (the caller should force an IDR instead).
    pub fn invalidate(&mut self, frame: u32) -> Result<bool> {
        if !self.ref_invalidation {
            return Ok(false);
        }
        // SAFETY: valid encoder session; the timestamp is a plain value.
        let s = unsafe { self.api.fns.nvEncInvalidateRefFrames.unwrap()(self.enc, frame as u64) };
        if s != _NVENCSTATUS_NV_ENC_SUCCESS {
            // E.g. the frame already left the DPB: not fatal, the caller
            // falls back to an IDR.
            tracing::warn!(
                "nvEncInvalidateRefFrames({frame}) failed: {}",
                status_name(s)
            );
            return Ok(false);
        }
        Ok(true)
    }

    /// Encode the ring texture at `input`. `on_chunk(bytes, last)` is called
    /// with each new run of bitstream bytes as soon as it is available (one or
    /// more whole slices in sub-frame mode, the whole picture otherwise);
    /// `last` is true exactly once, on the final call for the picture.
    pub fn encode(
        &mut self,
        input: usize,
        force_idr: bool,
        mut on_chunk: impl FnMut(&[u8], bool),
    ) -> Result<EncodeTiming> {
        let api = self.api;
        let mut timing = EncodeTiming::default();
        unsafe {
            let mut map: NV_ENC_MAP_INPUT_RESOURCE = std::mem::zeroed();
            map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
            let (reg, width, height) = self.registered[input];
            map.registeredResource = reg;
            nv!(api, nvEncMapInputResource(self.enc, &mut map));

            let mut pic: NV_ENC_PIC_PARAMS = std::mem::zeroed();
            pic.version = NV_ENC_PIC_PARAMS_VER;
            pic.inputWidth = width;
            pic.inputHeight = height;
            pic.inputBuffer = map.mappedResource;
            pic.bufferFmt = map.mappedBufferFmt;
            pic.outputBitstream = self.bitstream;
            pic.pictureStruct = _NV_ENC_PIC_STRUCT_NV_ENC_PIC_STRUCT_FRAME;
            pic.frameIdx = self.frame_idx;
            pic.inputTimeStamp = self.frame_idx as u64;
            if force_idr {
                pic.encodePicFlags = (_NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_FORCEIDR
                    | _NV_ENC_PIC_FLAGS_NV_ENC_PIC_FLAG_OUTPUT_SPSPPS)
                    as u32;
            }

            let start = Instant::now();
            let submit = api.fns.nvEncEncodePicture.unwrap()(self.enc, &mut pic);
            if submit != _NVENCSTATUS_NV_ENC_SUCCESS {
                let _ = api.fns.nvEncUnmapInputResource.unwrap()(self.enc, map.mappedResource);
                bail!("nvEncEncodePicture failed: {}", status_name(submit));
            }
            // NVENC has consumed this timestamp: advance now so later frames
            // never reuse it, even if readback fails below.
            self.frame_idx = self.frame_idx.wrapping_add(1);

            let result = self.read_bitstream(start, &mut timing, &mut on_chunk);
            nv!(api, nvEncUnmapInputResource(self.enc, map.mappedResource));
            result?;
        }
        Ok(timing)
    }

    unsafe fn read_bitstream(
        &mut self,
        start: Instant,
        timing: &mut EncodeTiming,
        on_chunk: &mut impl FnMut(&[u8], bool),
    ) -> Result<()> {
        let api = self.api;
        let lock_fn = api.fns.nvEncLockBitstream.unwrap();
        let unlock_fn = api.fns.nvEncUnlockBitstream.unwrap();
        let mut emitted = 0u32;

        // Sub-frame phase: poll without waiting and hand out each new run of
        // slices as soon as it lands. Never treat a poll as final: the driver
        // can report the picture complete before `bitstreamSizeInBytes`
        // includes the last slice (seen as pictures missing their bottom
        // slice), so completion only ends polling.
        while self.subframe {
            let mut lock: NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
            lock.version = NV_ENC_LOCK_BITSTREAM_VER;
            lock.outputBitstream = self.bitstream;
            lock.sliceOffsets = self.slice_offsets.as_mut_ptr();
            lock.set_doNotWait(1);

            let s = lock_fn(self.enc, &mut lock);
            if s == _NVENCSTATUS_NV_ENC_ERR_LOCK_BUSY {
                std::hint::spin_loop();
                continue;
            }
            if s != _NVENCSTATUS_NV_ENC_SUCCESS {
                bail!("nvEncLockBitstream failed: {}", status_name(s));
            }
            let size = lock.bitstreamSizeInBytes;
            let complete = lock.hwEncodeStatus == HW_ENCODE_STATUS_COMPLETE;
            // Once complete, leave the remainder to the final lock so the last
            // slice and the end-of-picture mark travel as ONE chunk: a separate
            // near-empty terminator chunk costs a send per frame and, being
            // tiny and last, is what a loss burst most easily kills.
            if size > emitted && !complete {
                let data = std::slice::from_raw_parts(
                    (lock.bitstreamBufferPtr as *const u8).add(emitted as usize),
                    (size - emitted) as usize,
                );
                if timing.chunks == 0 {
                    timing.first_chunk_us = start.elapsed().as_micros() as u32;
                }
                on_chunk(data, false);
                timing.chunks += 1;
                emitted = size;
            }
            let s = unlock_fn(self.enc, self.bitstream);
            if s != _NVENCSTATUS_NV_ENC_SUCCESS {
                bail!("nvEncUnlockBitstream failed: {}", status_name(s));
            }
            if complete {
                break;
            }
            std::hint::spin_loop();
        }

        // Final, blocking lock: authoritative size of the whole picture. Emit
        // whatever the polls missed (often nothing) as the LAST chunk.
        let mut lock: NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream;
        lock.sliceOffsets = self.slice_offsets.as_mut_ptr();
        nv!(api, nvEncLockBitstream(self.enc, &mut lock));
        let size = lock.bitstreamSizeInBytes;
        let tail_start = emitted.min(size);
        let data = std::slice::from_raw_parts(
            (lock.bitstreamBufferPtr as *const u8).add(tail_start as usize),
            (size - tail_start) as usize,
        );
        if timing.chunks == 0 {
            timing.first_chunk_us = start.elapsed().as_micros() as u32;
        }
        on_chunk(data, true);
        timing.chunks += 1;
        if self.subframe && size > emitted && emitted > 0 {
            tracing::trace!("final lock found {} more bytes", size - emitted);
        }
        emitted = size;
        timing.idr = lock.pictureType == _NV_ENC_PIC_TYPE_NV_ENC_PIC_TYPE_IDR;
        nv!(api, nvEncUnlockBitstream(self.enc, self.bitstream));

        timing.total_us = start.elapsed().as_micros() as u32;
        timing.bytes = emitted;
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let f = &self.api.fns;
            for &(r, _, _) in &self.registered {
                let _ = f.nvEncUnregisterResource.unwrap()(self.enc, r);
            }
            if !self.bitstream.is_null() {
                let _ = f.nvEncDestroyBitstreamBuffer.unwrap()(self.enc, self.bitstream);
            }
            let _ = f.nvEncDestroyEncoder.unwrap()(self.enc);
        }
    }
}
