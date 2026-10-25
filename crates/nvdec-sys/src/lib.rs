//! Raw FFI for the CUDA driver API and NVDEC (nvcuvid), loaded at runtime from
//! `nvcuda.dll` and `nvcuvid.dll`.
//!
//! Types come from bindgen over FFmpeg's `dynlink_*.h` headers (`bindings.rs`).
//! bindgen does not emit C function typedefs, so the entry points this project
//! uses are declared below.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code
)]
#![allow(clippy::all)]

mod bindings;
pub use bindings::*;

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

pub const CUDA_SUCCESS: CUresult = cudaError_enum_CUDA_SUCCESS;

macro_rules! dyn_api {
    ($(#[$m:meta])* pub struct $name:ident from $dll:literal {
        $( fn $f:ident ( $($arg:ident : $ty:ty),* $(,)? ) -> $ret:ty; )*
    }) => {
        $(#[$m])*
        pub struct $name {
            $( pub $f: unsafe extern "system" fn($($ty),*) -> $ret, )*
            _lib: libloading::Library,
        }

        impl $name {
            unsafe fn load() -> Result<Self, String> {
                let lib = libloading::Library::new($dll).map_err(|e| format!("load {}: {e}", $dll))?;
                Ok($name {
                    $( $f: *lib
                        .get::<unsafe extern "system" fn($($ty),*) -> $ret>(concat!(stringify!($f), "\0").as_bytes())
                        .map_err(|e| format!("{}!{}: {e}", $dll, stringify!($f)))?, )*
                    _lib: lib,
                })
            }
        }
    };
}

dyn_api! {
    /// CUDA driver API subset (symbol names include their `_v2` suffix).
    pub struct Cuda from "nvcuda.dll" {
        fn cuInit(flags: c_uint) -> CUresult;
        fn cuGetErrorName(err: CUresult, name: *mut *const c_char) -> CUresult;
        fn cuDeviceGetName(name: *mut c_char, len: c_int, dev: CUdevice) -> CUresult;
        fn cuD3D11GetDevice(dev: *mut CUdevice, adapter: *mut c_void) -> CUresult;
        fn cuCtxCreate_v2(ctx: *mut CUcontext, flags: c_uint, dev: CUdevice) -> CUresult;
        fn cuCtxDestroy_v2(ctx: CUcontext) -> CUresult;
        fn cuCtxPushCurrent_v2(ctx: CUcontext) -> CUresult;
        fn cuCtxPopCurrent_v2(ctx: *mut CUcontext) -> CUresult;
        fn cuStreamCreate(stream: *mut CUstream, flags: c_uint) -> CUresult;
        fn cuStreamDestroy_v2(stream: CUstream) -> CUresult;
        fn cuStreamSynchronize(stream: CUstream) -> CUresult;
        fn cuMemcpy2DAsync_v2(copy: *const CUDA_MEMCPY2D, stream: CUstream) -> CUresult;
        fn cuGraphicsD3D11RegisterResource(res: *mut CUgraphicsResource, d3d_res: *mut c_void, flags: c_uint) -> CUresult;
        fn cuGraphicsUnregisterResource(res: CUgraphicsResource) -> CUresult;
        fn cuGraphicsResourceSetMapFlags_v2(res: CUgraphicsResource, flags: c_uint) -> CUresult;
        fn cuGraphicsMapResources(count: c_uint, res: *mut CUgraphicsResource, stream: CUstream) -> CUresult;
        fn cuGraphicsUnmapResources(count: c_uint, res: *mut CUgraphicsResource, stream: CUstream) -> CUresult;
        fn cuGraphicsSubResourceGetMappedArray(arr: *mut CUarray, res: CUgraphicsResource, index: c_uint, mip: c_uint) -> CUresult;
    }
}

dyn_api! {
    /// NVDEC: bitstream parser + hardware decoder.
    pub struct Cuvid from "nvcuvid.dll" {
        fn cuvidGetDecoderCaps(caps: *mut CUVIDDECODECAPS) -> CUresult;
        fn cuvidCreateDecoder(dec: *mut CUvideodecoder, info: *mut CUVIDDECODECREATEINFO) -> CUresult;
        fn cuvidDestroyDecoder(dec: CUvideodecoder) -> CUresult;
        fn cuvidReconfigureDecoder(dec: CUvideodecoder, info: *mut CUVIDRECONFIGUREDECODERINFO) -> CUresult;
        fn cuvidDecodePicture(dec: CUvideodecoder, pic: *mut CUVIDPICPARAMS) -> CUresult;
        fn cuvidGetDecodeStatus(dec: CUvideodecoder, idx: c_int, status: *mut CUVIDGETDECODESTATUS) -> CUresult;
        fn cuvidMapVideoFrame64(dec: CUvideodecoder, idx: c_int, ptr: *mut u64, pitch: *mut c_uint, proc_: *mut CUVIDPROCPARAMS) -> CUresult;
        fn cuvidUnmapVideoFrame64(dec: CUvideodecoder, ptr: u64) -> CUresult;
        fn cuvidCreateVideoParser(parser: *mut CUvideoparser, params: *mut CUVIDPARSERPARAMS) -> CUresult;
        fn cuvidParseVideoData(parser: CUvideoparser, packet: *mut CUVIDSOURCEDATAPACKET) -> CUresult;
        fn cuvidDestroyVideoParser(parser: CUvideoparser) -> CUresult;
    }
}

pub struct Api {
    pub cuda: Cuda,
    pub cuvid: Cuvid,
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// Load both DLLs and `cuInit` once per process.
pub fn api() -> Result<&'static Api, String> {
    API.get_or_init(|| unsafe {
        let cuda = Cuda::load()?;
        let s = (cuda.cuInit)(0);
        if s != CUDA_SUCCESS {
            return Err(format!("cuInit failed: {}", error_name(&cuda, s)));
        }
        Ok(Api {
            cuda,
            cuvid: Cuvid::load()?,
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

pub fn error_name(cuda: &Cuda, s: CUresult) -> String {
    unsafe {
        let mut p: *const c_char = std::ptr::null();
        if (cuda.cuGetErrorName)(s, &mut p) == CUDA_SUCCESS && !p.is_null() {
            return std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
        }
    }
    format!("CUresult({s})")
}
