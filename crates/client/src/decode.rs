//! NVDEC H.264 decoder with zero display delay, writing straight into D3D11
//! textures through CUDA graphics interop (no CPU round trip).
//!
//! Output is two textures: luma (R8, w x h) and interleaved chroma (R8G8,
//! w/2 x h/2), which the renderer converts to RGB.

use std::ffi::{c_int, c_void};
use std::ptr;

use anyhow::{bail, Context, Result};
use nvdec_sys::*;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

const CU_GRAPHICS_MAP_RESOURCE_FLAGS_WRITE_DISCARD: u32 = 2;

macro_rules! cu {
    ($api:expr, $call:ident ( $($arg:expr),* $(,)? )) => {{
        let s = ($api.$call)($($arg),*);
        if s != CUDA_SUCCESS {
            bail!("{} failed: {}", stringify!($call), error_name(&api().unwrap().cuda, s));
        }
    }};
}

pub struct Planes {
    pub y: ID3D11Texture2D,
    pub uv: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
}

/// State reachable from the parser callbacks.
struct Inner {
    api: &'static Api,
    stream: CUstream,
    decoder: CUvideodecoder,
    device: ID3D11Device,
    width: u32,
    height: u32,
    planes: Option<Planes>,
    resources: [CUgraphicsResource; 2],
    /// What the decoder was created for: a later sequence that fits is
    /// handled with `cuvidReconfigureDecoder` (adaptive resolution) instead
    /// of a ~50 ms destroy + create.
    codec: cudaVideoCodec,
    max_coded: (u32, u32),
    surfaces: u32,
    frames_out: u64,
    /// NVDEC status and parser timestamp (= frame id) of the last output picture.
    last_ok: bool,
    last_timestamp: i64,
    error: Option<anyhow::Error>,
}

pub struct Decoder {
    inner: Box<Inner>,
    parser: CUvideoparser,
    ctx: CUcontext,
}

impl Decoder {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let api = nvdec_sys::api().map_err(anyhow::Error::msg)?;
        unsafe {
            let adapter: IDXGIAdapter = device
                .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?
                .GetAdapter()?;
            let mut dev: CUdevice = 0;
            cu!(api.cuda, cuD3D11GetDevice(&mut dev, adapter.as_raw()));
            let mut name = [0i8; 128];
            (api.cuda.cuDeviceGetName)(name.as_mut_ptr(), name.len() as c_int, dev);
            tracing::info!(
                "NVDEC on CUDA device {dev}: {}",
                std::ffi::CStr::from_ptr(name.as_ptr()).to_string_lossy()
            );

            // CU_CTX_SCHED_BLOCKING_SYNC would add wake-up latency; spin instead.
            const CU_CTX_SCHED_SPIN: u32 = 1;
            let mut ctx = ptr::null_mut();
            cu!(api.cuda, cuCtxCreate_v2(&mut ctx, CU_CTX_SCHED_SPIN, dev));

            let mut stream = ptr::null_mut();
            const CU_STREAM_NON_BLOCKING: u32 = 1;
            cu!(
                api.cuda,
                cuStreamCreate(&mut stream, CU_STREAM_NON_BLOCKING)
            );

            let mut inner = Box::new(Inner {
                api,
                stream,
                decoder: ptr::null_mut(),
                device: device.clone(),
                width: 0,
                height: 0,
                planes: None,
                resources: [ptr::null_mut(); 2],
                codec: 0,
                max_coded: (0, 0),
                surfaces: 0,
                frames_out: 0,
                last_ok: true,
                last_timestamp: -1,
                error: None,
            });

            let mut params: CUVIDPARSERPARAMS = std::mem::zeroed();
            params.CodecType = cudaVideoCodec_enum_cudaVideoCodec_H264;
            params.ulMaxNumDecodeSurfaces = 1; // raised by the sequence callback
            params.ulMaxDisplayDelay = 0; // output each picture as soon as it is decoded
            params.ulErrorThreshold = 100;
            params.pUserData = (&mut *inner) as *mut Inner as *mut c_void;
            params.pfnSequenceCallback = Some(on_sequence);
            params.pfnDecodePicture = Some(on_decode);
            params.pfnDisplayPicture = Some(on_display);
            let mut parser = ptr::null_mut();
            cu!(api.cuvid, cuvidCreateVideoParser(&mut parser, &mut params));

            Ok(Decoder { inner, parser, ctx })
        }
    }

    /// Feed one complete access unit. With zero display delay and the
    /// end-of-picture flag, the picture is decoded and copied into the output
    /// textures before this returns. Returns true if a new picture is ready.
    pub fn decode(&mut self, au: &[u8], timestamp: i64) -> Result<bool> {
        self.feed(au, true, timestamp)
    }

    /// Feed part of a picture as it arrives from the network. Bytes are
    /// buffered by the parser; the picture is decoded when `end_of_picture`
    /// is set. Returns true if a new picture is ready.
    pub fn feed(&mut self, data: &[u8], end_of_picture: bool, timestamp: i64) -> Result<bool> {
        let mut flags = CUvideopacketflags_CUVID_PKT_TIMESTAMP;
        if end_of_picture {
            flags |= CUvideopacketflags_CUVID_PKT_ENDOFPICTURE;
        }
        self.parse(data, flags, timestamp)
    }

    fn parse(&mut self, data: &[u8], flags: CUvideopacketflags, timestamp: i64) -> Result<bool> {
        let before = self.inner.frames_out;
        unsafe {
            let mut pkt: CUVIDSOURCEDATAPACKET = std::mem::zeroed();
            pkt.flags = flags as _;
            pkt.payload_size = data.len() as _;
            pkt.payload = if data.is_empty() {
                ptr::null()
            } else {
                data.as_ptr()
            };
            pkt.timestamp = timestamp;
            cu!(
                self.inner.api.cuvid,
                cuvidParseVideoData(self.parser, &mut pkt)
            );
        }
        if let Some(e) = self.inner.error.take() {
            return Err(e);
        }
        Ok(self.inner.frames_out > before)
    }

    /// Frame id (parser timestamp) of the picture now in the output textures,
    /// and whether NVDEC decoded it without error or concealment.
    pub fn last_output(&self) -> (i64, bool) {
        (self.inner.last_timestamp, self.inner.last_ok)
    }

    pub fn planes(&self) -> Option<&Planes> {
        self.inner.planes.as_ref()
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let api = self.inner.api;
            (api.cuvid.cuvidDestroyVideoParser)(self.parser);
            self.inner.release_decoder();
            (api.cuda.cuStreamDestroy_v2)(self.inner.stream);
            (api.cuda.cuCtxDestroy_v2)(self.ctx);
        }
    }
}

impl Inner {
    unsafe fn release_planes(&mut self) {
        for r in &mut self.resources {
            if !r.is_null() {
                (self.api.cuda.cuGraphicsUnregisterResource)(*r);
                *r = ptr::null_mut();
            }
        }
        self.planes = None;
    }

    unsafe fn release_decoder(&mut self) {
        self.release_planes();
        if !self.decoder.is_null() {
            (self.api.cuvid.cuvidDestroyDecoder)(self.decoder);
            self.decoder = ptr::null_mut();
        }
    }

    unsafe fn create_decoder(&mut self, fmt: &CUVIDEOFORMAT) -> Result<c_int> {
        let api = self.api;
        if fmt.chroma_format != cudaVideoChromaFormat_enum_cudaVideoChromaFormat_420
            || fmt.bit_depth_luma_minus8 != 0
        {
            bail!("only 8-bit 4:2:0 streams are supported");
        }
        let width = (fmt.display_area.right - fmt.display_area.left) as u32;
        let height = (fmt.display_area.bottom - fmt.display_area.top) as u32;
        let surfaces = fmt.min_num_decode_surfaces as u32 + 2;
        let coded = (fmt.coded_width, fmt.coded_height);

        if !self.decoder.is_null()
            && fmt.codec == self.codec
            && coded.0 <= self.max_coded.0
            && coded.1 <= self.max_coded.1
            && surfaces <= self.surfaces
        {
            self.release_planes();
            let mut r: CUVIDRECONFIGUREDECODERINFO = std::mem::zeroed();
            r.ulWidth = coded.0;
            r.ulHeight = coded.1;
            r.ulTargetWidth = width;
            r.ulTargetHeight = height;
            r.ulNumDecodeSurfaces = self.surfaces;
            r.display_area.left = fmt.display_area.left as _;
            r.display_area.top = fmt.display_area.top as _;
            r.display_area.right = fmt.display_area.right as _;
            r.display_area.bottom = fmt.display_area.bottom as _;
            let rc = (api.cuvid.cuvidReconfigureDecoder)(self.decoder, &mut r);
            if rc == CUDA_SUCCESS && self.create_planes(width, height).is_ok() {
                tracing::info!(
                    "decoder reconfigured: {width}x{height} (coded {}x{})",
                    coded.0,
                    coded.1
                );
                return Ok(self.surfaces as c_int);
            }
            // Fall through to a full recreate rather than leaving a live
            // decoder without output planes.
            tracing::warn!(
                "decoder reconfigure failed ({}); recreating",
                error_name(&api.cuda, rc)
            );
        }

        self.release_decoder();

        let mut info: CUVIDDECODECREATEINFO = std::mem::zeroed();
        info.CodecType = fmt.codec;
        info.ChromaFormat = fmt.chroma_format;
        info.OutputFormat = cudaVideoSurfaceFormat_enum_cudaVideoSurfaceFormat_NV12;
        info.bitDepthMinus8 = 0;
        info.DeinterlaceMode = cudaVideoDeinterlaceMode_enum_cudaVideoDeinterlaceMode_Weave;
        info.ulNumOutputSurfaces = 2;
        info.ulCreationFlags = cudaVideoCreateFlags_enum_cudaVideoCreate_PreferCUVID as _;
        info.ulNumDecodeSurfaces = surfaces as _;
        info.ulWidth = fmt.coded_width as _;
        info.ulHeight = fmt.coded_height as _;
        // Room to grow to 1080p in place: a stream that starts small
        // (adaptive resolution) can then step up with a reconfigure.
        let max = (coded.0.max(1920), coded.1.max(1088));
        info.ulMaxWidth = max.0 as _;
        info.ulMaxHeight = max.1 as _;
        info.display_area.left = fmt.display_area.left as _;
        info.display_area.top = fmt.display_area.top as _;
        info.display_area.right = fmt.display_area.right as _;
        info.display_area.bottom = fmt.display_area.bottom as _;
        info.ulTargetWidth = width as _;
        info.ulTargetHeight = height as _;
        cu!(api.cuvid, cuvidCreateDecoder(&mut self.decoder, &mut info));
        self.codec = fmt.codec;
        self.max_coded = max;
        self.surfaces = surfaces;
        self.create_planes(width, height)?;
        tracing::info!(
            "decoder: {}x{} (coded {}x{}), {} decode surfaces",
            width,
            height,
            fmt.coded_width,
            fmt.coded_height,
            surfaces
        );
        Ok(surfaces as c_int)
    }

    /// D3D11 output textures for the decoded size, registered with CUDA.
    unsafe fn create_planes(&mut self, width: u32, height: u32) -> Result<()> {
        let api = self.api;
        let planes = Planes {
            y: texture(&self.device, width, height, DXGI_FORMAT_R8_UNORM)?,
            uv: texture(&self.device, width / 2, height / 2, DXGI_FORMAT_R8G8_UNORM)?,
            width,
            height,
        };
        for (i, tex) in [&planes.y, &planes.uv].into_iter().enumerate() {
            let mut rc =
                (api.cuda.cuGraphicsD3D11RegisterResource)(&mut self.resources[i], tex.as_raw(), 0);
            if rc == CUDA_SUCCESS {
                rc = (api.cuda.cuGraphicsResourceSetMapFlags_v2)(
                    self.resources[i],
                    CU_GRAPHICS_MAP_RESOURCE_FLAGS_WRITE_DISCARD,
                );
            }
            if rc != CUDA_SUCCESS {
                // Unregister whatever succeeded before the textures go away.
                self.release_planes();
                bail!(
                    "CUDA texture registration failed: {}",
                    error_name(&api.cuda, rc)
                );
            }
        }
        self.planes = Some(planes);
        self.width = width;
        self.height = height;
        Ok(())
    }

    unsafe fn display(&mut self, disp: &CUVIDPARSERDISPINFO) -> Result<()> {
        let api = self.api;
        let mut proc_: CUVIDPROCPARAMS = std::mem::zeroed();
        proc_.progressive_frame = disp.progressive_frame;
        proc_.top_field_first = disp.top_field_first;
        proc_.output_stream = self.stream;

        let mut src: u64 = 0;
        let mut pitch: u32 = 0;
        cu!(
            api.cuvid,
            cuvidMapVideoFrame64(
                self.decoder,
                disp.picture_index,
                &mut src,
                &mut pitch,
                &mut proc_
            )
        );
        // Mapping waited for the decode, so the status is final now.
        let mut status: CUVIDGETDECODESTATUS = std::mem::zeroed();
        let r = (api.cuvid.cuvidGetDecodeStatus)(self.decoder, disp.picture_index, &mut status);
        // Only an explicit error or concealment counts as corrupt; a status
        // query that fails or is not supported says nothing about the picture.
        self.last_ok = r != CUDA_SUCCESS
            || (status.decodeStatus != cuvidDecodeStatus_enum_cuvidDecodeStatus_Error
                && status.decodeStatus != cuvidDecodeStatus_enum_cuvidDecodeStatus_Error_Concealed);
        if !self.last_ok || r != CUDA_SUCCESS {
            tracing::debug!(
                "decode status of frame {}: query {} status {}",
                disp.timestamp,
                error_name(&api.cuda, r),
                status.decodeStatus
            );
        }
        self.last_timestamp = disp.timestamp;

        let mut copy = || -> Result<()> {
            cu!(
                api.cuda,
                cuGraphicsMapResources(2, self.resources.as_mut_ptr(), self.stream)
            );
            let mut arrays = [ptr::null_mut(); 2];
            for (array, resource) in arrays.iter_mut().zip(self.resources) {
                cu!(
                    api.cuda,
                    cuGraphicsSubResourceGetMappedArray(array, resource, 0, 0)
                );
            }
            let planes = [
                (src, self.width as usize, self.height as usize),
                // NV12 chroma follows luma at pitch * surface height.
                (
                    src + pitch as u64 * self.height as u64,
                    self.width as usize,
                    self.height as usize / 2,
                ),
            ];
            for (i, &(dev, width_bytes, rows)) in planes.iter().enumerate() {
                let mut c: CUDA_MEMCPY2D = std::mem::zeroed();
                c.srcMemoryType = CUmemorytype_enum_CU_MEMORYTYPE_DEVICE;
                c.srcDevice = dev;
                c.srcPitch = pitch as usize;
                c.dstMemoryType = CUmemorytype_enum_CU_MEMORYTYPE_ARRAY;
                c.dstArray = arrays[i];
                c.WidthInBytes = width_bytes;
                c.Height = rows;
                cu!(api.cuda, cuMemcpy2DAsync_v2(&c, self.stream));
            }
            // Unmap is ordered after the copies on the stream; D3D11 then sees the data.
            cu!(
                api.cuda,
                cuGraphicsUnmapResources(2, self.resources.as_mut_ptr(), self.stream)
            );
            cu!(api.cuda, cuStreamSynchronize(self.stream));
            Ok(())
        };
        let result = copy();
        (api.cuvid.cuvidUnmapVideoFrame64)(self.decoder, src);
        result?;
        self.frames_out += 1;
        Ok(())
    }
}

fn texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    unsafe {
        let mut t = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D")?;
        Ok(t.unwrap())
    }
}

unsafe fn inner<'a>(user: *mut c_void) -> &'a mut Inner {
    &mut *(user as *mut Inner)
}

/// Returns the number of decode surfaces the parser should use (0 = error).
unsafe extern "C" fn on_sequence(user: *mut c_void, fmt: *mut CUVIDEOFORMAT) -> c_int {
    let s = inner(user);
    match s.create_decoder(&*fmt) {
        Ok(n) => n,
        Err(e) => {
            s.error = Some(e);
            0
        }
    }
}

unsafe extern "C" fn on_decode(user: *mut c_void, pic: *mut CUVIDPICPARAMS) -> c_int {
    let s = inner(user);
    if s.decoder.is_null() {
        return 0;
    }
    let r = (s.api.cuvid.cuvidDecodePicture)(s.decoder, pic);
    if r != CUDA_SUCCESS {
        s.error = Some(anyhow::anyhow!(
            "cuvidDecodePicture failed: {}",
            error_name(&s.api.cuda, r)
        ));
        return 0;
    }
    1
}

unsafe extern "C" fn on_display(user: *mut c_void, disp: *mut CUVIDPARSERDISPINFO) -> c_int {
    let s = inner(user);
    if disp.is_null() {
        return 1; // end of stream
    }
    if let Err(e) = s.display(&*disp) {
        s.error = Some(e);
        return 0;
    }
    1
}
