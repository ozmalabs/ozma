// SPDX-License-Identifier: AGPL-3.0-only
//! Optional GBM/EGL importer for non-linear (tiled) DMA-BUFs.
//!
//! The linear importer in [`crate::dbus_dmabuf`] reads pixels directly through
//! `mmap`. A tiled or vendor-swizzled buffer cannot be read that way, so this
//! module imports the descriptor into an EGLImage
//! (`EGL_EXT_image_dma_buf_import[_modifiers]`), attaches the image to a GL
//! texture, and reads the pixels back through a framebuffer object. That is the
//! same import path QEMU itself uses when it consumes a DMA-BUF.
//!
//! No EGL/GBM/GL headers are required at build time: the libraries are opened
//! with `dlopen` on first use, and every entry point is resolved with `dlsym` or
//! `eglGetProcAddress`. A host without EGL/GBM therefore builds normally and
//! gets a typed "not available" result instead of a link failure.
//!
//! ## Why `unsafe` is isolated here
//!
//! Dynamic loading and the EGL/GL C API necessarily use raw pointers and
//! function pointers. Every `unsafe` block in this module is a wrapper around
//! one documented C call. The safe surface is:
//!
//! - [`classify_modifier`] (pure parsing, used before any FFI),
//! - [`probe`] (read-only capability discovery),
//! - [`TiledEglBuffer`] (owns the descriptor, EGL display/context, texture, and
//!   framebuffer for exactly one imported scanout).
//!
//! `TiledEglBuffer` makes the context current immediately before every GL call
//! and drops the context/display after the texture and framebuffer are deleted.
//! It is `Send` because an EGL context may be made current on another thread
//! once its previous user has released it; it is deliberately not `Sync`.
//!
//! ## What this importer does not do
//!
//! - It reads back packed 32-bit formats only. Tiled YUV needs a
//!   colour-conversion shader or an external-texture path; those buffers keep
//!   the typed unsupported error rather than being decoded incorrectly.
//! - It only claims modifiers it understands. `DRM_FORMAT_MOD_INVALID` is
//!   refused even though it is not `0`: an exporter that cannot name its layout
//!   (QEMU on an AMD render node does this) must not be treated as linear.
//! - Intel CCS is refused. EGL accepts the BMG flat-CCS modifier, but the
//!   compression metadata is not exposed as a DMA-BUF plane, so a compressed
//!   producer's bytes cannot be decoded; importing it anyway would silently
//!   corrupt a frame.
//! - AMD DCC is supported only when the descriptor carries *both* the main
//!   surface and the DCC metadata plane. Mesa's radeonsi importer requires the
//!   metadata plane and fails with `EGL_BAD_MATCH` without it. `ScanoutDMABUF2`
//!   can carry both; the single-fd `ScanoutDMABUF` cannot.
//!
//! [`probe_modifier_import`] and [`probe_compressed_imports`] allocate private
//! GBM buffers and perform the real EGL import/readback so the exact per-node
//! result (including the EGL error code) can be reported without faking a
//! frame. `dbus_display::compressed_modifier_probe()` exposes that report.
//!
//! The GL readback returns rows in DMA-BUF row order (row 0 first). The caller
//! keeps the existing `y0_top` and damage-rectangle handling in
//! [`crate::dbus_dmabuf::MappedDmaBuf`].

use std::ffi::{c_char, c_void, CString};
use std::fs::OpenOptions;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nix::libc;

/// `DRM_FORMAT_MOD_INVALID`: the exporter did not report a concrete layout.
pub(crate) const DRM_FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;

/// Intel tiled modifiers that this importer can hand to EGL.
pub(crate) const I915_FORMAT_MOD_X_TILED: u64 = 0x0100_0000_0000_0001;
pub(crate) const I915_FORMAT_MOD_Y_TILED: u64 = 0x0100_0000_0000_0002;
pub(crate) const I915_FORMAT_MOD_YF_TILED: u64 = 0x0100_0000_0000_0003;
pub(crate) const I915_FORMAT_MOD_4_TILED: u64 = 0x0100_0000_0000_0009;

/// AMD tiled modifiers without DCC compression.
pub(crate) const AMD_FMT_MOD_TILE_GFX9_64K_S: u64 = 0x0200_0000_0000_0901;
pub(crate) const AMD_FMT_MOD_TILE_GFX9_4K_D_X: u64 = 0x0200_0000_0040_1603;
pub(crate) const AMD_FMT_MOD_TILE_GFX9_64K_R_X: u64 = 0x0200_0000_0040_1b03;

const EGL_NO_DISPLAY: *mut c_void = std::ptr::null_mut();
const EGL_NO_CONTEXT: *mut c_void = std::ptr::null_mut();
const EGL_NO_SURFACE: *mut c_void = std::ptr::null_mut();
const EGL_NONE: i32 = 0x3038;
const EGL_TRUE: u32 = 1;
const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_VENDOR: i32 = 0x3053;
const EGL_VERSION: i32 = 0x3054;
const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;
const EGL_OPENGL_ES_API: u32 = 0x30A0;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_WINDOW_BIT: i32 = 0x0004;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
const EGL_RED_SIZE: i32 = 0x3024;
const EGL_GREEN_SIZE: i32 = 0x3023;
const EGL_BLUE_SIZE: i32 = 0x3022;
const EGL_ALPHA_SIZE: i32 = 0x3021;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE1_FD_EXT: i32 = 0x3275;
const EGL_DMA_BUF_PLANE1_OFFSET_EXT: i32 = 0x3276;
const EGL_DMA_BUF_PLANE1_PITCH_EXT: i32 = 0x3277;
const EGL_DMA_BUF_PLANE2_FD_EXT: i32 = 0x3278;
const EGL_DMA_BUF_PLANE2_OFFSET_EXT: i32 = 0x3279;
const EGL_DMA_BUF_PLANE2_PITCH_EXT: i32 = 0x327A;
const EGL_DMA_BUF_PLANE3_FD_EXT: i32 = 0x3440;
const EGL_DMA_BUF_PLANE3_OFFSET_EXT: i32 = 0x3441;
const EGL_DMA_BUF_PLANE3_PITCH_EXT: i32 = 0x3442;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT: i32 = 0x3445;
const EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT: i32 = 0x3446;
const EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT: i32 = 0x3447;
const EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT: i32 = 0x3448;
const EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT: i32 = 0x3449;
const EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT: i32 = 0x344A;

/// EGL error codes that this importer reports verbatim in typed errors.
const EGL_SUCCESS: i32 = 0x3000;
const EGL_BAD_ACCESS: i32 = 0x3002;
const EGL_BAD_ALLOC: i32 = 0x3003;
const EGL_BAD_ATTRIBUTE: i32 = 0x3004;
const EGL_BAD_MATCH: i32 = 0x3009;
const EGL_BAD_PARAMETER: i32 = 0x300C;
const EGL_BAD_CONTEXT: i32 = 0x3006;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_NEAREST: i32 = 0x2600;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_RENDERER: u32 = 0x1F01;
const GL_NO_ERROR: u32 = 0;
const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_SCISSOR_TEST: u32 = 0x0C11;

type EglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type EglGetDisplay = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type EglGetPlatformDisplay = unsafe extern "C" fn(u32, *mut c_void, *const i32) -> *mut c_void;
type EglInitialize = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> u32;
type EglTerminate = unsafe extern "C" fn(*mut c_void) -> u32;
type EglQueryString = unsafe extern "C" fn(*mut c_void, i32) -> *const c_char;
type EglBindApi = unsafe extern "C" fn(u32) -> u32;
type EglChooseConfig =
    unsafe extern "C" fn(*mut c_void, *const i32, *mut *mut c_void, i32, *mut i32) -> u32;
type EglCreateContext =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const i32) -> *mut c_void;
type EglDestroyContext = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type EglMakeCurrent =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> u32;
type EglGetError = unsafe extern "C" fn() -> i32;
type EglCreateImage =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type EglDestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type EglQueryDmaBufFormats = unsafe extern "C" fn(*mut c_void, i32, *mut i32, *mut i32) -> u32;
type EglQueryDmaBufModifiers =
    unsafe extern "C" fn(*mut c_void, i32, i32, *mut u64, *mut u32, *mut i32) -> u32;

type GbmCreateDevice = unsafe extern "C" fn(RawFd) -> *mut c_void;
type GbmDeviceDestroy = unsafe extern "C" fn(*mut c_void);
type GbmBoCreateWithModifiers =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, u32) -> *mut c_void;
type GbmBoDestroy = unsafe extern "C" fn(*mut c_void);
type GbmBoGetFd = unsafe extern "C" fn(*mut c_void) -> RawFd;
type GbmBoGetStride = unsafe extern "C" fn(*mut c_void) -> u32;
type GbmBoGetModifier = unsafe extern "C" fn(*mut c_void) -> u64;
type GbmBoGetPlaneCount = unsafe extern "C" fn(*mut c_void) -> i32;
type GbmBoGetFdForPlane = unsafe extern "C" fn(*mut c_void, i32) -> RawFd;
type GbmBoGetStrideForPlane = unsafe extern "C" fn(*mut c_void, i32) -> u32;
type GbmBoGetOffset = unsafe extern "C" fn(*mut c_void, i32) -> u32;

type GlGenTextures = unsafe extern "C" fn(i32, *mut u32);
type GlDeleteTextures = unsafe extern "C" fn(i32, *const u32);
type GlBindTexture = unsafe extern "C" fn(u32, u32);
type GlTexParameteri = unsafe extern "C" fn(u32, u32, i32);
type GlGenFramebuffers = unsafe extern "C" fn(i32, *mut u32);
type GlDeleteFramebuffers = unsafe extern "C" fn(i32, *const u32);
type GlBindFramebuffer = unsafe extern "C" fn(u32, u32);
type GlFramebufferTexture2D = unsafe extern "C" fn(u32, u32, u32, u32, i32);
type GlCheckFramebufferStatus = unsafe extern "C" fn(u32) -> u32;
type GlReadPixels = unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void);
type GlFinish = unsafe extern "C" fn();
type GlGetError = unsafe extern "C" fn() -> u32;
type GlGetString = unsafe extern "C" fn(u32) -> *const u8;
type GlViewport = unsafe extern "C" fn(i32, i32, i32, i32);
type GlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type GlClear = unsafe extern "C" fn(u32);
type GlEnable = unsafe extern "C" fn(u32);
type GlDisable = unsafe extern "C" fn(u32);
type GlScissor = unsafe extern "C" fn(i32, i32, i32, i32);
type GlEglImageTargetTexture2D = unsafe extern "C" fn(u32, *mut c_void);

#[allow(dead_code)]
struct Api {
    egl_get_display: Option<EglGetDisplay>,
    egl_get_platform_display: Option<EglGetPlatformDisplay>,
    egl_initialize: EglInitialize,
    egl_terminate: EglTerminate,
    egl_query_string: EglQueryString,
    egl_bind_api: EglBindApi,
    egl_choose_config: EglChooseConfig,
    egl_create_context: EglCreateContext,
    egl_destroy_context: EglDestroyContext,
    egl_make_current: EglMakeCurrent,
    egl_get_error: EglGetError,
    egl_create_image: EglCreateImage,
    egl_destroy_image: EglDestroyImage,
    egl_query_dma_buf_formats: EglQueryDmaBufFormats,
    egl_query_dma_buf_modifiers: EglQueryDmaBufModifiers,
    gbm_create_device: GbmCreateDevice,
    gbm_device_destroy: GbmDeviceDestroy,
    gbm_bo_create_with_modifiers: GbmBoCreateWithModifiers,
    gbm_bo_destroy: GbmBoDestroy,
    gbm_bo_get_fd: GbmBoGetFd,
    gbm_bo_get_stride: GbmBoGetStride,
    gbm_bo_get_modifier: GbmBoGetModifier,
    gbm_bo_get_plane_count: Option<GbmBoGetPlaneCount>,
    gbm_bo_get_fd_for_plane: Option<GbmBoGetFdForPlane>,
    gbm_bo_get_stride_for_plane: Option<GbmBoGetStrideForPlane>,
    gbm_bo_get_offset: Option<GbmBoGetOffset>,
    gl_gen_textures: GlGenTextures,
    gl_delete_textures: GlDeleteTextures,
    gl_bind_texture: GlBindTexture,
    gl_tex_parameteri: GlTexParameteri,
    gl_gen_framebuffers: GlGenFramebuffers,
    gl_delete_framebuffers: GlDeleteFramebuffers,
    gl_bind_framebuffer: GlBindFramebuffer,
    gl_framebuffer_texture2d: GlFramebufferTexture2D,
    gl_check_framebuffer_status: GlCheckFramebufferStatus,
    gl_read_pixels: GlReadPixels,
    gl_finish: GlFinish,
    gl_get_error: GlGetError,
    gl_get_string: GlGetString,
    gl_viewport: GlViewport,
    gl_clear_color: GlClearColor,
    gl_clear: GlClear,
    gl_enable: GlEnable,
    gl_disable: GlDisable,
    gl_scissor: GlScissor,
    gl_egl_image_target_texture: GlEglImageTargetTexture2D,
}

struct ApiLoad {
    api: Option<Api>,
    error: Option<String>,
}

fn load() -> &'static ApiLoad {
    static LOAD: OnceLock<ApiLoad> = OnceLock::new();
    LOAD.get_or_init(|| unsafe { load_inner() })
}

unsafe fn dlopen(name: &str) -> Option<*mut c_void> {
    let name = CString::new(name).ok()?;
    let handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if handle.is_null() {
        None
    } else {
        Some(handle)
    }
}

unsafe fn dlsym<T: Copy>(handle: *mut c_void, name: &str) -> Option<T> {
    let name = CString::new(name).ok()?;
    let symbol = libc::dlsym(handle, name.as_ptr());
    if symbol.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&symbol))
    }
}

unsafe fn proc_address<T: Copy>(get_proc: EglGetProcAddress, name: &str) -> Option<T> {
    let name = CString::new(name).ok()?;
    let symbol = get_proc(name.as_ptr());
    if symbol.is_null() {
        None
    } else {
        Some(std::mem::transmute_copy(&symbol))
    }
}

/// Resolve one symbol from `eglGetProcAddress` first and then `libGLESv2`.
unsafe fn gl_symbol<T: Copy>(
    get_proc: EglGetProcAddress,
    gles: *mut c_void,
    name: &str,
) -> Option<T> {
    proc_address(get_proc, name).or_else(|| dlsym(gles, name))
}

unsafe fn load_inner() -> ApiLoad {
    let Some(egl) = dlopen("libEGL.so.1").or_else(|| dlopen("libEGL.so")) else {
        return ApiLoad {
            api: None,
            error: Some("libEGL.so.1/libEGL.so could not be loaded".to_owned()),
        };
    };
    let Some(gbm) = dlopen("libgbm.so.1").or_else(|| dlopen("libgbm.so")) else {
        return ApiLoad {
            api: None,
            error: Some("libgbm.so.1/libgbm.so could not be loaded".to_owned()),
        };
    };
    let Some(gles) = dlopen("libGLESv2.so.2").or_else(|| dlopen("libGLESv2.so")) else {
        return ApiLoad {
            api: None,
            error: Some("libGLESv2.so.2/libGLESv2.so could not be loaded".to_owned()),
        };
    };

    macro_rules! missing {
        ($name:literal) => {{
            return ApiLoad {
                api: None,
                error: Some(format!("required EGL/GBM/GL symbol is missing: {}", $name)),
            };
        }};
    }
    macro_rules! require {
        ($handle:expr, $name:literal) => {
            match dlsym($handle, $name) {
                Some(symbol) => symbol,
                None => missing!($name),
            }
        };
    }

    let egl_get_proc_address: EglGetProcAddress = require!(egl, "eglGetProcAddress");
    macro_rules! require_proc {
        ($name:literal) => {
            match proc_address(egl_get_proc_address, $name).or_else(|| dlsym(egl, $name)) {
                Some(symbol) => symbol,
                None => missing!($name),
            }
        };
    }
    macro_rules! require_gl {
        ($name:literal) => {
            match gl_symbol(egl_get_proc_address, gles, $name) {
                Some(symbol) => symbol,
                None => missing!($name),
            }
        };
    }

    let api = Api {
        egl_get_display: dlsym(egl, "eglGetDisplay"),
        egl_get_platform_display: proc_address(egl_get_proc_address, "eglGetPlatformDisplayEXT")
            .or_else(|| dlsym(egl, "eglGetPlatformDisplay")),
        egl_initialize: require!(egl, "eglInitialize"),
        egl_terminate: require!(egl, "eglTerminate"),
        egl_query_string: require!(egl, "eglQueryString"),
        egl_bind_api: require!(egl, "eglBindAPI"),
        egl_choose_config: require!(egl, "eglChooseConfig"),
        egl_create_context: require!(egl, "eglCreateContext"),
        egl_destroy_context: require!(egl, "eglDestroyContext"),
        egl_make_current: require!(egl, "eglMakeCurrent"),
        egl_get_error: require!(egl, "eglGetError"),
        egl_create_image: require_proc!("eglCreateImageKHR"),
        egl_destroy_image: require_proc!("eglDestroyImageKHR"),
        egl_query_dma_buf_formats: require_proc!("eglQueryDmaBufFormatsEXT"),
        egl_query_dma_buf_modifiers: require_proc!("eglQueryDmaBufModifiersEXT"),
        gbm_create_device: require!(gbm, "gbm_create_device"),
        gbm_device_destroy: require!(gbm, "gbm_device_destroy"),
        gbm_bo_create_with_modifiers: require!(gbm, "gbm_bo_create_with_modifiers"),
        gbm_bo_destroy: require!(gbm, "gbm_bo_destroy"),
        gbm_bo_get_fd: require!(gbm, "gbm_bo_get_fd"),
        gbm_bo_get_stride: require!(gbm, "gbm_bo_get_stride"),
        gbm_bo_get_modifier: require!(gbm, "gbm_bo_get_modifier"),
        gbm_bo_get_plane_count: dlsym(gbm, "gbm_bo_get_plane_count"),
        gbm_bo_get_fd_for_plane: dlsym(gbm, "gbm_bo_get_fd_for_plane"),
        gbm_bo_get_stride_for_plane: dlsym(gbm, "gbm_bo_get_stride_for_plane"),
        gbm_bo_get_offset: dlsym(gbm, "gbm_bo_get_offset"),
        gl_gen_textures: require_gl!("glGenTextures"),
        gl_delete_textures: require_gl!("glDeleteTextures"),
        gl_bind_texture: require_gl!("glBindTexture"),
        gl_tex_parameteri: require_gl!("glTexParameteri"),
        gl_gen_framebuffers: require_gl!("glGenFramebuffers"),
        gl_delete_framebuffers: require_gl!("glDeleteFramebuffers"),
        gl_bind_framebuffer: require_gl!("glBindFramebuffer"),
        gl_framebuffer_texture2d: require_gl!("glFramebufferTexture2D"),
        gl_check_framebuffer_status: require_gl!("glCheckFramebufferStatus"),
        gl_read_pixels: require_gl!("glReadPixels"),
        gl_finish: require_gl!("glFinish"),
        gl_get_error: require_gl!("glGetError"),
        gl_get_string: require_gl!("glGetString"),
        gl_viewport: require_gl!("glViewport"),
        gl_clear_color: require_gl!("glClearColor"),
        gl_clear: require_gl!("glClear"),
        gl_enable: require_gl!("glEnable"),
        gl_disable: require_gl!("glDisable"),
        gl_scissor: require_gl!("glScissor"),
        gl_egl_image_target_texture: require_proc!("glEGLImageTargetTexture2DOES"),
    };

    ApiLoad {
        api: Some(api),
        error: None,
    }
}

/// Export every plane of a GBM BO as an owned descriptor plus offset/stride.
///
/// `gbm_bo_get_fd` only describes plane 0, so multi-plane (DCC) fixtures and
/// probes need the newer `*_for_plane` entry points. They are optional at load
/// time so older libgbm still builds and runs the single-plane path.
unsafe fn gbm_bo_planes(api: &Api, bo: *mut c_void) -> Result<Vec<(OwnedFd, u32, u32)>, String> {
    let plane_count = match api.gbm_bo_get_plane_count {
        Some(get_plane_count) => get_plane_count(bo),
        None => 1,
    };
    if !(1..=4).contains(&plane_count) {
        return Err(format!("GBM reported {plane_count} planes"));
    }
    let mut planes = Vec::with_capacity(plane_count as usize);
    for plane_index in 0..plane_count {
        let raw_fd = match api.gbm_bo_get_fd_for_plane {
            Some(get_fd_for_plane) => get_fd_for_plane(bo, plane_index),
            None if plane_index == 0 => (api.gbm_bo_get_fd)(bo),
            None => -1,
        };
        if raw_fd < 0 {
            return Err(format!("gbm_bo_get_fd_for_plane({plane_index}) failed"));
        }
        // `gbm_bo_get_fd[_for_plane]` returns a newly owned descriptor.
        let fd = OwnedFd::from_raw_fd(raw_fd);
        let stride = match api.gbm_bo_get_stride_for_plane {
            Some(get_stride_for_plane) => get_stride_for_plane(bo, plane_index),
            None if plane_index == 0 => (api.gbm_bo_get_stride)(bo),
            None => {
                return Err(format!(
                    "gbm_bo_get_stride_for_plane is unavailable for plane {plane_index}"
                ))
            }
        };
        let offset = match api.gbm_bo_get_offset {
            Some(get_offset) => get_offset(bo, plane_index),
            None if plane_index == 0 => 0,
            None => return Err("gbm_bo_get_offset is unavailable".to_owned()),
        };
        planes.push((fd, offset, stride));
    }
    Ok(planes)
}

/// One render node that was probed for EGL/GBM import support.
#[derive(Clone, Debug)]
pub struct RenderNodeProbe {
    /// `/dev/dri/renderD*` path.
    pub path: String,
    /// Whether EGL/GBM/GL initialised on this node.
    pub available: bool,
    /// GL renderer string when initialisation succeeded.
    pub renderer: Option<String>,
    /// Why initialisation failed, if it did.
    pub error: Option<String>,
    /// Modifiers reported by `eglQueryDmaBufModifiersEXT` for a packed fourcc.
    pub reported_modifiers: Vec<u64>,
}

/// A modifier reported by the host but deliberately not claimed by this importer.
#[derive(Clone, Debug)]
pub struct UnsupportedModifier {
    /// The exact modifier value.
    pub modifier: u64,
    /// Decoded name, for example `AMD_FMT_MOD(tile=27, dcc=1)`.
    pub name: String,
    /// Why the importer refuses it.
    pub reason: String,
}

/// Result of probing the host's tiled DMA-BUF import capability.
#[derive(Clone, Debug, Default)]
pub struct TiledImportProbe {
    /// `libEGL` loaded and every required EGL entry point resolved.
    pub egl_available: bool,
    /// `libgbm` and `libGLESv2` loaded and every required entry point resolved.
    pub gbm_available: bool,
    /// `EGL_EXT_image_dma_buf_import` was advertised by at least one display.
    pub import_dma_buf: bool,
    /// `EGL_EXT_image_dma_buf_import_modifiers` was advertised.
    pub import_modifiers: bool,
    /// EGL vendor string from the first usable display.
    pub egl_vendor: Option<String>,
    /// EGL version string from the first usable display.
    pub egl_version: Option<String>,
    /// GL renderer from the first usable display.
    pub gl_renderer: Option<String>,
    /// Per-render-node results.
    pub render_nodes: Vec<RenderNodeProbe>,
    /// Modifiers this importer understands and can pass to EGL.
    pub supported_modifiers: Vec<u64>,
    /// Subset of `supported_modifiers` that also needs the auxiliary DCC
    /// metadata plane (AMD DCC); a single-plane `ScanoutDMABUF` cannot use it.
    pub aux_plane_modifiers: Vec<u64>,
    /// Modifiers the host advertises but this importer refuses.
    pub unsupported_modifiers: Vec<UnsupportedModifier>,
    /// Diagnostics, including why EGL is unavailable and nodes that failed.
    pub notes: Vec<String>,
}

/// Outcome of one real GBM-allocation + EGL-import + readback probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModifierImportVerdict {
    /// GBM allocated it, EGL imported it, and a painted pattern read back
    /// exactly. For compressed layouts this is only reachable when the
    /// descriptor carries every plane the importer needs.
    Verified,
    /// EGL accepted the modifier and round-tripped, but the modifier is
    /// compressed and the descriptor has only the main plane. The compression
    /// metadata lives outside the DMA-BUF, so a compressed third-party
    /// producer cannot be decoded safely.
    CompressedMetadataMissing,
    /// GBM could not allocate a buffer with this modifier on this node.
    GbmAllocationFailed,
    /// GBM allocated it, but `eglCreateImageKHR` failed.
    EglImportFailed,
    /// EGL created the image, but the framebuffer was not complete.
    FramebufferIncomplete,
    /// The framebuffer was complete but `glReadPixels` did not return the
    /// painted pattern (or reported a GL error).
    ReadbackFailed,
    /// EGL/GBM was unavailable on this node.
    Unavailable,
}

/// One real compressed/tiled modifier probe on one render node.
#[derive(Clone, Debug)]
pub struct ModifierImportProbe {
    /// Fourcc that was passed to GBM/EGL.
    pub fourcc: u32,
    /// Exact modifier value.
    pub modifier: u64,
    /// Render node used for the probe.
    pub node: String,
    /// Number of planes GBM exported (0 when allocation failed).
    pub planes: usize,
    /// Whether GBM allocated a buffer with this modifier.
    pub gbm_allocated: bool,
    /// Modifier GBM reported for the allocation.
    pub gbm_reported_modifier: Option<u64>,
    /// Exact `eglGetError()` value when image creation failed.
    pub egl_error: Option<i32>,
    /// Exact `glGetError()` value when binding/readback failed.
    pub gl_error: Option<u32>,
    /// Overall verdict.
    pub verdict: ModifierImportVerdict,
    /// Human-readable detail, including the exact modifier/fourcc/error.
    pub detail: String,
}

/// Probe EGL/GBM availability and the tiled modifiers the host advertises.
pub(crate) fn probe() -> TiledImportProbe {
    let loaded = load();
    let mut result = TiledImportProbe {
        egl_available: loaded.api.is_some(),
        gbm_available: loaded.api.is_some(),
        ..TiledImportProbe::default()
    };
    let Some(api) = loaded.api.as_ref() else {
        result.notes.push(
            loaded
                .error
                .clone()
                .unwrap_or_else(|| "EGL/GBM loader failed".to_owned()),
        );
        return result;
    };

    let nodes = candidate_devices();
    if nodes.is_empty() {
        result
            .notes
            .push("no /dev/dri/renderD* nodes were found".to_owned());
        return result;
    }

    let mut supported = Vec::new();
    let mut aux_plane = Vec::new();
    let mut unsupported = Vec::new();
    for node in nodes {
        let path = node.display().to_string();
        match EglState::new(api, &node) {
            Ok(state) => {
                let display = state.display;
                let extensions =
                    unsafe { query_string(api, display, EGL_EXTENSIONS) }.unwrap_or_default();
                result.import_dma_buf |= extensions
                    .split_whitespace()
                    .any(|extension| extension == "EGL_EXT_image_dma_buf_import");
                result.import_modifiers |= extensions
                    .split_whitespace()
                    .any(|extension| extension == "EGL_EXT_image_dma_buf_import_modifiers");
                if result.egl_vendor.is_none() {
                    result.egl_vendor = unsafe { query_string(api, display, EGL_VENDOR) };
                    result.egl_version = unsafe { query_string(api, display, EGL_VERSION) };
                    result.gl_renderer = state.renderer.clone();
                }
                let mut modifiers = Vec::new();
                // SAFETY: `display` is a live EGL display owned by `state`.
                for format in unsafe { query_packed_formats(api, display) } {
                    modifiers.extend(unsafe { query_modifiers(api, display, format) });
                }
                modifiers.sort_unstable();
                modifiers.dedup();
                for &modifier in &modifiers {
                    if classify_modifier(modifier, packed_probe_fourcc()).is_ok() {
                        if !supported.contains(&modifier) {
                            supported.push(modifier);
                        }
                        if modifier_requires_aux_plane(modifier) && !aux_plane.contains(&modifier) {
                            aux_plane.push(modifier);
                        }
                    } else {
                        let reason =
                            classify_modifier(modifier, packed_probe_fourcc()).unwrap_err();
                        if !unsupported
                            .iter()
                            .any(|entry: &UnsupportedModifier| entry.modifier == modifier)
                        {
                            unsupported.push(UnsupportedModifier {
                                modifier,
                                name: modifier_name(modifier),
                                reason,
                            });
                        }
                    }
                }
                result.render_nodes.push(RenderNodeProbe {
                    path,
                    available: true,
                    renderer: state.renderer.clone(),
                    error: None,
                    reported_modifiers: modifiers,
                });
            }
            Err(error) => {
                result.notes.push(format!("{path}: {error}"));
                result.render_nodes.push(RenderNodeProbe {
                    path,
                    available: false,
                    renderer: None,
                    error: Some(error),
                    reported_modifiers: Vec::new(),
                });
            }
        }
    }
    supported.sort_unstable();
    supported.dedup();
    result.supported_modifiers = supported;
    aux_plane.sort_unstable();
    aux_plane.dedup();
    result.aux_plane_modifiers = aux_plane;
    result.unsupported_modifiers = unsupported;
    result
}

/// Probe every host-advertised compressed modifier and tiled YUV layout with a
/// real GBM allocation, EGL import, painted pattern, and readback.
///
/// This allocates and frees private GBM buffers; it never touches a QEMU
/// scanout or emits a frame. It is intentionally separate from [`probe`]
/// because it performs actual imports and is slower.
pub(crate) fn probe_compressed_imports() -> Vec<ModifierImportProbe> {
    let loaded = load();
    let Some(api) = loaded.api.as_ref() else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for node in candidate_devices() {
        let mut state = match EglState::new(api, &node) {
            Ok(state) => state,
            Err(error) => {
                results.push(ModifierImportProbe {
                    fourcc: 0,
                    modifier: 0,
                    node: node.display().to_string(),
                    planes: 0,
                    gbm_allocated: false,
                    gbm_reported_modifier: None,
                    egl_error: None,
                    gl_error: None,
                    verdict: ModifierImportVerdict::Unavailable,
                    detail: error,
                });
                continue;
            }
        };
        let display = state.display;
        // SAFETY: `display` is a live EGL display owned by `state`.
        let formats = unsafe { query_formats(api, display) };
        for format in formats {
            let fourcc = format as u32;
            let interesting_format = crate::dbus_dmabuf::is_packed_fourcc(fourcc)
                || matches!(
                    fourcc,
                    crate::dbus_dmabuf::DRM_FORMAT_NV12
                        | crate::dbus_dmabuf::DRM_FORMAT_NV21
                        | crate::dbus_dmabuf::DRM_FORMAT_YUV420
                        | crate::dbus_dmabuf::DRM_FORMAT_YVU420
                );
            if !interesting_format {
                continue;
            }
            // SAFETY: `display` is live and `format` came from it.
            let mut modifiers = unsafe { query_modifiers(api, display, format) };
            modifiers.sort_unstable();
            modifiers.dedup();
            for modifier in modifiers {
                if modifier == 0 || modifier == DRM_FORMAT_MOD_INVALID {
                    continue;
                }
                let is_yuv = !crate::dbus_dmabuf::is_packed_fourcc(fourcc);
                if !modifier_is_compressed(modifier) && !is_yuv {
                    continue;
                }
                results.push(probe_modifier_on_state(&mut state, fourcc, modifier));
            }
        }
    }
    results
}

/// Probe one fourcc/modifier pair on every candidate render node.
pub(crate) fn probe_modifier_import(fourcc: u32, modifier: u64) -> Vec<ModifierImportProbe> {
    let loaded = load();
    let Some(api) = loaded.api.as_ref() else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for node in candidate_devices() {
        match EglState::new(api, &node) {
            Ok(mut state) => {
                results.push(probe_modifier_on_state(&mut state, fourcc, modifier));
            }
            Err(error) => results.push(ModifierImportProbe {
                fourcc,
                modifier,
                node: node.display().to_string(),
                planes: 0,
                gbm_allocated: false,
                gbm_reported_modifier: None,
                egl_error: None,
                gl_error: None,
                verdict: ModifierImportVerdict::Unavailable,
                detail: error,
            }),
        }
    }
    results
}

fn probe_modifier_on_state(
    state: &mut EglState,
    fourcc: u32,
    modifier: u64,
) -> ModifierImportProbe {
    let api = state.api;
    let node = state.path.clone();
    let width = 64u32;
    let height = 64u32;
    let mut result = ModifierImportProbe {
        fourcc,
        modifier,
        node: node.clone(),
        planes: 0,
        gbm_allocated: false,
        gbm_reported_modifier: None,
        egl_error: None,
        gl_error: None,
        verdict: ModifierImportVerdict::GbmAllocationFailed,
        detail: String::new(),
    };

    // SAFETY: `state.gbm_device` is live for `state`'s lifetime and `modifier`
    // points at a valid local value.
    let bo = unsafe {
        (api.gbm_bo_create_with_modifiers)(state.gbm_device, width, height, fourcc, &modifier, 1)
    };
    if bo.is_null() {
        let error = std::io::Error::last_os_error();
        result.detail = format!(
            "gbm_bo_create_with_modifiers({}, {}) failed: {error}",
            crate::dbus_dmabuf::fourcc_name(fourcc),
            modifier_name(modifier)
        );
        return result;
    }
    result.gbm_allocated = true;
    result.gbm_reported_modifier = Some(unsafe { (api.gbm_bo_get_modifier)(bo) });
    let planes = unsafe { gbm_bo_planes(api, bo) };
    unsafe { (api.gbm_bo_destroy)(bo) };
    let planes = match planes {
        Ok(planes) => planes,
        Err(error) => {
            result.detail = format!(
                "allocated {} with {} but could not export its planes: {error}",
                crate::dbus_dmabuf::fourcc_name(fourcc),
                modifier_name(modifier)
            );
            return result;
        }
    };
    result.planes = planes.len();
    if result.gbm_reported_modifier != Some(modifier) {
        result.detail = format!(
            "GBM returned modifier {:#018x} for requested {:#018x}",
            result.gbm_reported_modifier.unwrap_or(0),
            modifier
        );
        return result;
    }

    let attrs: Vec<PlaneAttrs> = planes
        .iter()
        .map(|(fd, offset, stride)| PlaneAttrs {
            fd: fd.as_raw_fd(),
            offset: *offset,
            stride: *stride,
        })
        .collect();
    if let Err(error) = state.bind_dmabuf(&attrs, width, height, fourcc, modifier) {
        result.egl_error = error.egl_error;
        result.gl_error = error.gl_error;
        result.verdict = match error.stage {
            EglImportStage::CreateImage => ModifierImportVerdict::EglImportFailed,
            EglImportStage::Framebuffer => ModifierImportVerdict::FramebufferIncomplete,
            _ => ModifierImportVerdict::ReadbackFailed,
        };
        result.detail = format!(
            "{} with {} on {}: {error}",
            crate::dbus_dmabuf::fourcc_name(fourcc),
            modifier_name(modifier),
            node
        );
        return result;
    }

    // Paint the same four quadrants the fixture tests use, then read them back.
    // A compressed layout must round-trip through the GPU for this to match.
    let colors = [
        [1.0, 0.0, 0.0, 1.0],
        [0.0, 1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0, 1.0],
        [1.0, 1.0, 1.0, 1.0],
    ];
    if let Err(error) = state.paint_quadrants(colors) {
        result.verdict = ModifierImportVerdict::ReadbackFailed;
        result.detail = format!(
            "imported {} with {} but painting failed: {error}",
            crate::dbus_dmabuf::fourcc_name(fourcc),
            modifier_name(modifier)
        );
        return result;
    }
    let pixels = match state.read_bgra() {
        Ok(pixels) => pixels,
        Err(error) => {
            result.verdict = ModifierImportVerdict::ReadbackFailed;
            result.detail = format!(
                "imported {} with {} but readback failed: {error}",
                crate::dbus_dmabuf::fourcc_name(fourcc),
                modifier_name(modifier)
            );
            return result;
        }
    };
    let row = width as usize * 4;
    let top = (height as usize - 1) * row;
    let half = (width as usize / 2) * 4;
    let expected = [
        (0, [0u8, 0, 255, 255]),            // bottom-left red
        (half, [0, 255, 0, 255]),           // bottom-right green
        (top, [255, 0, 0, 255]),            // top-left blue
        (top + half, [255, 255, 255, 255]), // top-right white
    ];
    let correct = expected.iter().all(|(offset, color)| {
        pixels
            .get(*offset..*offset + 4)
            .is_some_and(|pixel| pixel == color)
    });
    if !correct {
        result.verdict = ModifierImportVerdict::ReadbackFailed;
        result.detail = format!(
            "imported {} with {} but the painted quadrants did not read back exactly",
            crate::dbus_dmabuf::fourcc_name(fourcc),
            modifier_name(modifier)
        );
        return result;
    }

    if modifier_is_compressed(modifier) && planes.len() < 2 {
        result.verdict = ModifierImportVerdict::CompressedMetadataMissing;
        result.detail = format!(
            "EGL accepted and round-tripped {} with {}, but the descriptor has {} plane and the \
             compression metadata is not addressable; a third-party compressed producer cannot be \
             decoded safely",
            crate::dbus_dmabuf::fourcc_name(fourcc),
            modifier_name(modifier),
            planes.len()
        );
    } else {
        result.verdict = ModifierImportVerdict::Verified;
        result.detail = format!(
            "{} with {} imported and round-tripped on {} ({} plane(s))",
            crate::dbus_dmabuf::fourcc_name(fourcc),
            modifier_name(modifier),
            node,
            planes.len()
        );
    }
    result
}

/// Does this modifier describe a layout that the EGL path is allowed to read?
///
/// `Ok(())` means the modifier is in the known safe set; EGL may still refuse a
/// particular buffer, which becomes a typed import error. `Err(reason)` is the
/// exact reason reported to the caller.
///
/// Non-linear YUV remains refused: the GBM/EGL path only reads back packed
/// 32-bit formats. AMD DCC is accepted because Mesa imports it from the main
/// plane *plus* the DCC metadata plane; callers must use
/// [`modifier_requires_aux_plane`] and pass both descriptors to
/// [`TiledEglBuffer::new_with_planes`]. Intel CCS is refused because the
/// flat-CCS metadata is not exposed as a plane in the DMA-BUF, so EGL accepts
/// the modifier but cannot decode a compressed third-party producer.
pub(crate) fn classify_modifier(modifier: u64, fourcc: u32) -> Result<(), String> {
    if modifier == 0 {
        return Ok(());
    }
    if !crate::dbus_dmabuf::is_packed_fourcc(fourcc) {
        return Err(format!(
            "tiled YUV/non-32-bit fourcc {} with modifier {} has no EGL RGBA readback path \
             (colour conversion would need a shader)",
            crate::dbus_dmabuf::fourcc_name(fourcc),
            modifier_name(modifier)
        ));
    }
    if modifier == DRM_FORMAT_MOD_INVALID {
        return Err(
            "DRM_FORMAT_MOD_INVALID: the exporter did not report a concrete layout, so the buffer \
             cannot be safely interpreted as linear or tiled"
                .to_owned(),
        );
    }

    let vendor = modifier >> 56;
    match vendor {
        0x01 => match modifier & 0xff {
            1 | 2 | 3 | 9 => Ok(()),
            0x10 | 0x11 => Err(format!(
                "Intel modifier {} is flat-CCS compressed; its compression metadata is not exposed \
                 as a DMA-BUF plane, so EGL cannot decode a compressed producer (importing it \
                 would silently misinterpret the bytes)",
                modifier_name(modifier)
            )),
            4..=8 | 10..=15 => Err(format!(
                "Intel modifier {} is CCS-compressed with an auxiliary CCS/clear-colour plane; this \
                 importer has no verified Intel multi-plane CCS readback path",
                modifier_name(modifier)
            )),
            _ => Err(format!(
                "unknown Intel modifier {}",
                modifier_name(modifier)
            )),
        },
        0x02 => {
            let tile_version = modifier & 0xff;
            let tile = (modifier >> 8) & 0x1f;
            let dcc = (modifier >> 13) & 1;
            let dcc_retile = (modifier >> 14) & 1;
            let dcc_pipe_align = (modifier >> 15) & 1;
            if dcc == 0 && (dcc_retile != 0 || dcc_pipe_align != 0) {
                return Err(format!(
                    "AMD modifier {} sets DCC retile/pipe-align bits without DCC; the modifier is \
                     malformed",
                    modifier_name(modifier)
                ));
            }
            match (tile_version, tile) {
                (1..=5, 9 | 22 | 27) => Ok(()),
                _ => Err(format!(
                    "unknown AMD tiled modifier {}",
                    modifier_name(modifier)
                )),
            }
        }
        _ => Err(format!(
            "vendor {} modifier {} is not in the supported tiled set",
            vendor,
            modifier_name(modifier)
        )),
    }
}

/// True for an AMD modifier whose DCC metadata is a second DMA-BUF plane.
///
/// Mesa's radeonsi importer requires that plane: importing only the main plane
/// fails with `EGL_BAD_MATCH`. Callers that have a second descriptor must pass
/// it to [`TiledEglBuffer::new_with_planes`].
pub(crate) fn modifier_requires_aux_plane(modifier: u64) -> bool {
    modifier >> 56 == 0x02 && ((modifier >> 13) & 1) != 0
}

/// True for modifiers that describe a compressed layout (Intel CCS or AMD DCC).
pub(crate) fn modifier_is_compressed(modifier: u64) -> bool {
    match modifier >> 56 {
        0x01 => matches!(modifier & 0xff, 4..=8 | 10..=17),
        0x02 => ((modifier >> 13) & 1) != 0,
        _ => false,
    }
}

/// Human-readable EGL error name for typed errors and probe reports.
pub(crate) fn egl_error_name(code: i32) -> &'static str {
    match code {
        EGL_SUCCESS => "EGL_SUCCESS",
        EGL_BAD_ACCESS => "EGL_BAD_ACCESS",
        EGL_BAD_ALLOC => "EGL_BAD_ALLOC",
        EGL_BAD_ATTRIBUTE => "EGL_BAD_ATTRIBUTE",
        EGL_BAD_CONTEXT => "EGL_BAD_CONTEXT",
        EGL_BAD_MATCH => "EGL_BAD_MATCH",
        EGL_BAD_PARAMETER => "EGL_BAD_PARAMETER",
        _ => "EGL error",
    }
}

/// Human-readable modifier name used in errors and the probe report.
pub(crate) fn modifier_name(modifier: u64) -> String {
    match modifier {
        0 => "DRM_FORMAT_MOD_LINEAR".to_owned(),
        DRM_FORMAT_MOD_INVALID => format!("DRM_FORMAT_MOD_INVALID ({modifier:#018x})"),
        I915_FORMAT_MOD_X_TILED => format!("I915_FORMAT_MOD_X_TILED ({modifier:#018x})"),
        I915_FORMAT_MOD_Y_TILED => format!("I915_FORMAT_MOD_Y_TILED ({modifier:#018x})"),
        I915_FORMAT_MOD_YF_TILED => format!("I915_FORMAT_MOD_Yf_TILED ({modifier:#018x})"),
        I915_FORMAT_MOD_4_TILED => format!("I915_FORMAT_MOD_4_TILED ({modifier:#018x})"),
        0x0100_0000_0000_0004 => format!("I915_FORMAT_MOD_Y_TILED_CCS ({modifier:#018x})"),
        0x0100_0000_0000_0005 => format!("I915_FORMAT_MOD_Yf_TILED_CCS ({modifier:#018x})"),
        0x0100_0000_0000_0006 => format!("I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS ({modifier:#018x})"),
        0x0100_0000_0000_0007 => format!("I915_FORMAT_MOD_Y_TILED_GEN12_MC_CCS ({modifier:#018x})"),
        0x0100_0000_0000_0008 => {
            format!("I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC ({modifier:#018x})")
        }
        0x0100_0000_0000_000a => format!("I915_FORMAT_MOD_4_TILED_DG2_RC_CCS ({modifier:#018x})"),
        0x0100_0000_0000_000b => format!("I915_FORMAT_MOD_4_TILED_DG2_MC_CCS ({modifier:#018x})"),
        0x0100_0000_0000_000c => {
            format!("I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC ({modifier:#018x})")
        }
        0x0100_0000_0000_000d => format!("I915_FORMAT_MOD_4_TILED_MTL_RC_CCS ({modifier:#018x})"),
        0x0100_0000_0000_000e => format!("I915_FORMAT_MOD_4_TILED_MTL_MC_CCS ({modifier:#018x})"),
        0x0100_0000_0000_000f => {
            format!("I915_FORMAT_MOD_4_TILED_MTL_RC_CCS_CC ({modifier:#018x})")
        }
        0x0100_0000_0000_0010 => format!("I915_FORMAT_MOD_4_TILED_LNL_CCS ({modifier:#018x})"),
        0x0100_0000_0000_0011 => format!("I915_FORMAT_MOD_4_TILED_BMG_CCS ({modifier:#018x})"),
        AMD_FMT_MOD_TILE_GFX9_64K_S => {
            format!("AMD_FMT_MOD_TILE_GFX9_64K_S ({modifier:#018x})")
        }
        AMD_FMT_MOD_TILE_GFX9_4K_D_X => {
            format!("AMD_FMT_MOD_TILE_GFX9_4K_D_X ({modifier:#018x})")
        }
        AMD_FMT_MOD_TILE_GFX9_64K_R_X => {
            format!("AMD_FMT_MOD_TILE_GFX9_64K_R_X ({modifier:#018x})")
        }
        _ if modifier >> 56 == 0x02 => {
            let tile_version = modifier & 0xff;
            let tile = (modifier >> 8) & 0x1f;
            let dcc = (modifier >> 13) & 1;
            let dcc_retile = (modifier >> 14) & 1;
            format!(
                "AMD_FMT_MOD(tile_version={tile_version}, tile={tile}, dcc={dcc}, retile={dcc_retile}) ({modifier:#018x})"
            )
        }
        _ => format!("{modifier:#018x}"),
    }
}

fn candidate_devices() -> Vec<PathBuf> {
    if let Ok(device) = std::env::var("OZMA_DMABUF_EGL_DEVICE") {
        let device = device.trim();
        if !device.is_empty() {
            return vec![PathBuf::from(device)];
        }
    }
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    paths.sort();
    paths
}

fn packed_probe_fourcc() -> u32 {
    crate::dbus_dmabuf::DRM_FORMAT_XRGB8888
}

unsafe fn query_string(api: &Api, display: *mut c_void, name: i32) -> Option<String> {
    let value = (api.egl_query_string)(display, name);
    if value.is_null() {
        return None;
    }
    std::ffi::CStr::from_ptr(value)
        .to_str()
        .ok()
        .map(ToOwned::to_owned)
}

unsafe fn query_formats(api: &Api, display: *mut c_void) -> Vec<i32> {
    let mut count = 0;
    if (api.egl_query_dma_buf_formats)(display, 0, std::ptr::null_mut(), &mut count) != EGL_TRUE
        || count <= 0
    {
        return Vec::new();
    }
    let mut formats = vec![0i32; count as usize];
    if (api.egl_query_dma_buf_formats)(display, count, formats.as_mut_ptr(), &mut count) != EGL_TRUE
    {
        return Vec::new();
    }
    formats.truncate(count.max(0) as usize);
    formats
}

unsafe fn query_packed_formats(api: &Api, display: *mut c_void) -> Vec<i32> {
    query_formats(api, display)
        .into_iter()
        .filter(|format| crate::dbus_dmabuf::is_packed_fourcc(*format as u32))
        .collect()
}

unsafe fn query_modifiers(api: &Api, display: *mut c_void, format: i32) -> Vec<u64> {
    let mut count = 0;
    if (api.egl_query_dma_buf_modifiers)(
        display,
        format,
        0,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        &mut count,
    ) != EGL_TRUE
        || count <= 0
    {
        return Vec::new();
    }
    let mut modifiers = vec![0u64; count as usize];
    if (api.egl_query_dma_buf_modifiers)(
        display,
        format,
        count,
        modifiers.as_mut_ptr(),
        std::ptr::null_mut(),
        &mut count,
    ) != EGL_TRUE
    {
        return Vec::new();
    }
    modifiers.truncate(count.max(0) as usize);
    modifiers
}

/// One DMA-BUF plane handed to EGL.
#[derive(Clone, Copy, Debug)]
struct PlaneAttrs {
    fd: RawFd,
    offset: u32,
    stride: u32,
}

/// Which EGL/GL step failed during an import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EglImportStage {
    Context,
    Texture,
    CreateImage,
    TargetTexture,
    Framebuffer,
}

/// A typed EGL import failure with the exact EGL/GL error codes.
#[derive(Debug)]
struct EglImportError {
    stage: EglImportStage,
    message: String,
    egl_error: Option<i32>,
    gl_error: Option<u32>,
}

impl std::fmt::Display for EglImportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

struct EglState {
    api: &'static Api,
    display: *mut c_void,
    context: *mut c_void,
    gbm_device: *mut c_void,
    device_file: std::fs::File,
    texture: u32,
    framebuffer: u32,
    width: u32,
    height: u32,
    renderer: Option<String>,
    path: String,
}

// SAFETY: the state is owned by one `TiledEglBuffer`/`MappedDmaBuf` at a time.
// EGL contexts are not thread-safe, but they are explicitly made current before
// each GL call, and `TiledEglBuffer` is not `Sync`, so two threads cannot use
// the same context concurrently.
unsafe impl Send for EglState {}

impl EglState {
    fn new(api: &'static Api, path: &Path) -> Result<Self, String> {
        let device_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("open {} failed: {error}", path.display()))?;

        // SAFETY: the descriptor stays open in `device_file` for the lifetime of
        // the GBM device (dropped after `gbm_device_destroy` below).
        let gbm_device = unsafe { (api.gbm_create_device)(device_file.as_raw_fd()) };
        if gbm_device.is_null() {
            return Err("gbm_create_device failed".to_owned());
        }

        let display = unsafe { get_platform_display(api, gbm_device) };
        if display.is_null() {
            unsafe { (api.gbm_device_destroy)(gbm_device) };
            return Err("eglGetPlatformDisplay(GBM) failed".to_owned());
        }

        let mut major = 0;
        let mut minor = 0;
        if unsafe { (api.egl_initialize)(display, &mut major, &mut minor) } != EGL_TRUE {
            let error = unsafe { (api.egl_get_error)() };
            unsafe {
                (api.egl_terminate)(display);
                (api.gbm_device_destroy)(gbm_device);
            }
            return Err(format!("eglInitialize failed (0x{error:x})"));
        }

        let extensions = unsafe { query_string(api, display, EGL_EXTENSIONS) }.unwrap_or_default();
        let has = |name: &str| extensions.split_whitespace().any(|item| item == name);
        if !has("EGL_EXT_image_dma_buf_import") {
            unsafe {
                (api.egl_terminate)(display);
                (api.gbm_device_destroy)(gbm_device);
            }
            return Err("EGL_EXT_image_dma_buf_import is not advertised".to_owned());
        }
        if !has("EGL_EXT_image_dma_buf_import_modifiers") {
            unsafe {
                (api.egl_terminate)(display);
                (api.gbm_device_destroy)(gbm_device);
            }
            return Err("EGL_EXT_image_dma_buf_import_modifiers is not advertised".to_owned());
        }
        if !has("EGL_KHR_surfaceless_context") {
            unsafe {
                (api.egl_terminate)(display);
                (api.gbm_device_destroy)(gbm_device);
            }
            return Err("EGL_KHR_surfaceless_context is not advertised".to_owned());
        }

        if unsafe { (api.egl_bind_api)(EGL_OPENGL_ES_API) } != EGL_TRUE {
            let error = unsafe { (api.egl_get_error)() };
            unsafe {
                (api.egl_terminate)(display);
                (api.gbm_device_destroy)(gbm_device);
            }
            return Err(format!(
                "eglBindAPI(EGL_OPENGL_ES_API) failed (0x{error:x})"
            ));
        }

        let config = match unsafe { choose_config(api, display) } {
            Ok(config) => config,
            Err(error) => {
                unsafe {
                    (api.egl_terminate)(display);
                    (api.gbm_device_destroy)(gbm_device);
                }
                return Err(error);
            }
        };

        let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        let context = unsafe {
            (api.egl_create_context)(display, config, EGL_NO_CONTEXT, context_attributes.as_ptr())
        };
        if context.is_null() {
            let error = unsafe { (api.egl_get_error)() };
            unsafe {
                (api.egl_terminate)(display);
                (api.gbm_device_destroy)(gbm_device);
            }
            return Err(format!("eglCreateContext failed (0x{error:x})"));
        }

        if unsafe { (api.egl_make_current)(display, EGL_NO_SURFACE, EGL_NO_SURFACE, context) }
            != EGL_TRUE
        {
            let error = unsafe { (api.egl_get_error)() };
            unsafe {
                (api.egl_destroy_context)(display, context);
                (api.egl_terminate)(display);
                (api.gbm_device_destroy)(gbm_device);
            }
            return Err(format!("eglMakeCurrent failed (0x{error:x})"));
        }

        let renderer = unsafe {
            let value = (api.gl_get_string)(GL_RENDERER);
            if value.is_null() {
                None
            } else {
                std::ffi::CStr::from_ptr(value as *const c_char)
                    .to_str()
                    .ok()
                    .map(ToOwned::to_owned)
            }
        };

        Ok(Self {
            api,
            display,
            context,
            gbm_device,
            device_file,
            texture: 0,
            framebuffer: 0,
            width: 0,
            height: 0,
            renderer,
            path: path.display().to_string(),
        })
    }

    fn make_current(&self) -> Result<(), String> {
        if unsafe {
            (self.api.egl_make_current)(self.display, EGL_NO_SURFACE, EGL_NO_SURFACE, self.context)
        } != EGL_TRUE
        {
            let error = unsafe { (self.api.egl_get_error)() };
            return Err(format!("eglMakeCurrent failed (0x{error:x})"));
        }
        Ok(())
    }

    /// Import `planes` (one packed surface, or a DCC main + metadata pair).
    ///
    /// Every plane gets the same modifier; that is what Mesa's radeonsi DCC
    /// importer expects, and `eglCreateImageKHR` rejects a DCC modifier with
    /// only the main plane (`EGL_BAD_MATCH`).
    fn bind_dmabuf(
        &mut self,
        planes: &[PlaneAttrs],
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
    ) -> Result<(), EglImportError> {
        self.make_current().map_err(|message| EglImportError {
            stage: EglImportStage::Context,
            message,
            egl_error: None,
            gl_error: None,
        })?;
        if planes.is_empty() || planes.len() > 4 {
            return Err(EglImportError {
                stage: EglImportStage::CreateImage,
                message: format!("EGL DMA-BUF import needs 1-4 planes, got {}", planes.len()),
                egl_error: None,
                gl_error: None,
            });
        }
        let mut attributes = [EGL_NONE; 64];
        let mut index = 0;
        let mut push = |key: i32, value: i32| {
            attributes[index] = key;
            attributes[index + 1] = value;
            index += 2;
        };
        push(EGL_WIDTH, width as i32);
        push(EGL_HEIGHT, height as i32);
        push(EGL_LINUX_DRM_FOURCC_EXT, fourcc as i32);
        for (plane_index, plane) in planes.iter().enumerate() {
            let fd_key = [
                EGL_DMA_BUF_PLANE0_FD_EXT,
                EGL_DMA_BUF_PLANE1_FD_EXT,
                EGL_DMA_BUF_PLANE2_FD_EXT,
                EGL_DMA_BUF_PLANE3_FD_EXT,
            ][plane_index];
            let offset_key = [
                EGL_DMA_BUF_PLANE0_OFFSET_EXT,
                EGL_DMA_BUF_PLANE1_OFFSET_EXT,
                EGL_DMA_BUF_PLANE2_OFFSET_EXT,
                EGL_DMA_BUF_PLANE3_OFFSET_EXT,
            ][plane_index];
            let pitch_key = [
                EGL_DMA_BUF_PLANE0_PITCH_EXT,
                EGL_DMA_BUF_PLANE1_PITCH_EXT,
                EGL_DMA_BUF_PLANE2_PITCH_EXT,
                EGL_DMA_BUF_PLANE3_PITCH_EXT,
            ][plane_index];
            let modifier_lo_key = [
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT,
                EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT,
                EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT,
            ][plane_index];
            let modifier_hi_key = [
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT,
                EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT,
                EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT,
            ][plane_index];
            push(fd_key, plane.fd);
            push(offset_key, plane.offset as i32);
            push(pitch_key, plane.stride as i32);
            push(modifier_lo_key, (modifier & 0xffff_ffff) as i32);
            push(modifier_hi_key, (modifier >> 32) as i32);
        }
        attributes[index] = EGL_NONE;
        let image = unsafe {
            (self.api.egl_create_image)(
                self.display,
                EGL_NO_CONTEXT,
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attributes.as_ptr(),
            )
        };
        if image.is_null() {
            let error = unsafe { (self.api.egl_get_error)() };
            return Err(EglImportError {
                stage: EglImportStage::CreateImage,
                message: format!(
                    "eglCreateImageKHR failed (0x{error:x} {}) for {} plane(s) of {} with {}",
                    egl_error_name(error),
                    planes.len(),
                    crate::dbus_dmabuf::fourcc_name(fourcc),
                    modifier_name(modifier)
                ),
                egl_error: Some(error),
                gl_error: None,
            });
        }

        let mut texture = 0;
        unsafe {
            (self.api.gl_gen_textures)(1, &mut texture);
            (self.api.gl_bind_texture)(GL_TEXTURE_2D, texture);
            (self.api.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (self.api.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (self.api.gl_egl_image_target_texture)(GL_TEXTURE_2D, image);
            (self.api.egl_destroy_image)(self.display, image);
        }
        if texture == 0 {
            return Err(EglImportError {
                stage: EglImportStage::Texture,
                message: "glGenTextures returned 0".to_owned(),
                egl_error: None,
                gl_error: None,
            });
        }
        let gl_error = unsafe { (self.api.gl_get_error)() };
        if gl_error != GL_NO_ERROR {
            unsafe {
                (self.api.gl_delete_textures)(1, &texture);
            }
            return Err(EglImportError {
                stage: EglImportStage::TargetTexture,
                message: format!("glEGLImageTargetTexture2DOES failed (GL error 0x{gl_error:x})"),
                egl_error: None,
                gl_error: Some(gl_error),
            });
        }

        let mut framebuffer = 0;
        unsafe {
            (self.api.gl_gen_framebuffers)(1, &mut framebuffer);
            (self.api.gl_bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
            (self.api.gl_framebuffer_texture2d)(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                texture,
                0,
            );
        }
        let status = unsafe { (self.api.gl_check_framebuffer_status)(GL_FRAMEBUFFER) };
        if status != GL_FRAMEBUFFER_COMPLETE {
            unsafe {
                (self.api.gl_delete_framebuffers)(1, &framebuffer);
                (self.api.gl_delete_textures)(1, &texture);
            }
            return Err(EglImportError {
                stage: EglImportStage::Framebuffer,
                message: format!("framebuffer for imported DMA-BUF is incomplete (0x{status:x})"),
                egl_error: None,
                gl_error: None,
            });
        }

        // Replace an older binding in the same state, if any.
        if self.texture != 0 {
            unsafe {
                (self.api.gl_delete_textures)(1, &self.texture);
            }
        }
        if self.framebuffer != 0 {
            unsafe {
                (self.api.gl_delete_framebuffers)(1, &self.framebuffer);
            }
        }
        self.texture = texture;
        self.framebuffer = framebuffer;
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn read_bgra(&self) -> Result<Vec<u8>, String> {
        if self.texture == 0 || self.framebuffer == 0 || self.width == 0 || self.height == 0 {
            return Err("DMA-BUF has not been bound to an EGL image".to_owned());
        }
        self.make_current()?;
        let width = self.width;
        let height = self.height;
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        unsafe {
            (self.api.gl_bind_framebuffer)(GL_FRAMEBUFFER, self.framebuffer);
            (self.api.gl_read_pixels)(
                0,
                0,
                width as i32,
                height as i32,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                pixels.as_mut_ptr() as *mut c_void,
            );
            (self.api.gl_finish)();
        }
        let error = unsafe { (self.api.gl_get_error)() };
        if error != GL_NO_ERROR {
            return Err(format!("glReadPixels failed (GL error 0x{error:x})"));
        }
        // GL returns R,G,B,A; the rest of the capture pipeline is BGRA.
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Ok(pixels)
    }

    fn paint_quadrants(&self, colors: [[f32; 4]; 4]) -> Result<(), String> {
        self.make_current()?;
        let half_width = self.width / 2;
        let half_height = self.height / 2;
        unsafe {
            (self.api.gl_viewport)(0, 0, self.width as i32, self.height as i32);
            (self.api.gl_enable)(GL_SCISSOR_TEST);
            for (index, color) in colors.iter().enumerate() {
                let x = if index % 2 == 0 { 0 } else { half_width as i32 };
                let y = if index < 2 { 0 } else { half_height as i32 };
                (self.api.gl_scissor)(x, y, half_width as i32, half_height as i32);
                (self.api.gl_clear_color)(color[0], color[1], color[2], color[3]);
                (self.api.gl_clear)(GL_COLOR_BUFFER_BIT);
            }
            (self.api.gl_disable)(GL_SCISSOR_TEST);
            (self.api.gl_finish)();
        }
        let error = unsafe { (self.api.gl_get_error)() };
        if error != GL_NO_ERROR {
            return Err(format!("GL clear failed (GL error 0x{error:x})"));
        }
        Ok(())
    }
}

impl Drop for EglState {
    fn drop(&mut self) {
        unsafe {
            // GL object deletion needs the owning context current. The state is
            // not `Sync`, so no other thread can hold it; make it current on
            // whichever thread is dropping the imported scanout.
            (self.api.egl_make_current)(self.display, EGL_NO_SURFACE, EGL_NO_SURFACE, self.context);
            if self.framebuffer != 0 {
                (self.api.gl_delete_framebuffers)(1, &self.framebuffer);
            }
            if self.texture != 0 {
                (self.api.gl_delete_textures)(1, &self.texture);
            }
            (self.api.egl_make_current)(
                self.display,
                EGL_NO_SURFACE,
                EGL_NO_SURFACE,
                EGL_NO_CONTEXT,
            );
            if !self.context.is_null() {
                (self.api.egl_destroy_context)(self.display, self.context);
            }
            (self.api.egl_terminate)(self.display);
            (self.api.gbm_device_destroy)(self.gbm_device);
        }
        let _ = &self.device_file;
    }
}

unsafe fn get_platform_display(api: &Api, gbm_device: *mut c_void) -> *mut c_void {
    if let Some(get_platform_display) = api.egl_get_platform_display {
        let display = get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, std::ptr::null());
        if !display.is_null() {
            return display;
        }
    }
    if let Some(get_display) = api.egl_get_display {
        return get_display(gbm_device);
    }
    EGL_NO_DISPLAY
}

unsafe fn choose_config(api: &Api, display: *mut c_void) -> Result<*mut c_void, String> {
    let candidates: [&[i32]; 2] = [
        &[
            EGL_SURFACE_TYPE,
            EGL_WINDOW_BIT,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_ALPHA_SIZE,
            8,
            EGL_NONE,
        ],
        &[
            EGL_SURFACE_TYPE,
            EGL_WINDOW_BIT,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            EGL_RED_SIZE,
            5,
            EGL_GREEN_SIZE,
            5,
            EGL_BLUE_SIZE,
            5,
            EGL_ALPHA_SIZE,
            0,
            EGL_NONE,
        ],
    ];
    for attributes in candidates {
        let mut config = std::ptr::null_mut();
        let mut count = 0;
        if (api.egl_choose_config)(display, attributes.as_ptr(), &mut config, 1, &mut count)
            == EGL_TRUE
            && count >= 1
            && !config.is_null()
        {
            return Ok(config);
        }
    }
    let error = (api.egl_get_error)();
    Err(format!("eglChooseConfig failed (0x{error:x})"))
}

/// One owned DMA-BUF plane plus the byte offset/stride EGL needs.
struct OwnedPlane {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
}

/// An imported tiled/composited DMA-BUF and the EGL/GL state that reads it back.
pub(crate) struct TiledEglBuffer {
    /// Declared before `planes` so GL teardown runs while the descriptors are open.
    state: EglState,
    planes: Vec<OwnedPlane>,
    modifier: u64,
}

impl TiledEglBuffer {
    /// Import one plane of a tiled DMA-BUF for CPU readback.
    ///
    /// `width`/`height` are the backing-buffer dimensions, not the display
    /// crop; the caller applies `x`/`y`/`y0_top` through `MappedDmaBuf`.
    pub(crate) fn new(
        fd: OwnedFd,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        offset: u32,
        stride: u32,
    ) -> Result<Self, String> {
        Self::new_with_planes(vec![(fd, offset, stride)], width, height, fourcc, modifier)
    }

    /// Import a packed DMA-BUF with one or more planes.
    ///
    /// AMD DCC exports the main surface and its DCC metadata as two planes;
    /// Mesa requires both for the `EGLImage` and fails with `EGL_BAD_MATCH`
    /// when only the main plane is supplied. Non-DCC packed formats must use
    /// exactly one plane. All planes carry the same modifier.
    pub(crate) fn new_with_planes(
        planes: Vec<(OwnedFd, u32, u32)>,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
    ) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err("tiled DMA-BUF has a zero dimension".to_owned());
        }
        if !crate::dbus_dmabuf::is_packed_fourcc(fourcc) {
            return Err(format!(
                "{} is not a packed 32-bit fourcc; the EGL path cannot convert it to BGRA",
                crate::dbus_dmabuf::fourcc_name(fourcc)
            ));
        }
        classify_modifier(modifier, fourcc)?;

        let aux_expected = modifier_requires_aux_plane(modifier);
        let expected_planes = if aux_expected { 2 } else { 1 };
        if planes.len() != expected_planes {
            return Err(format!(
                "modifier {} needs {expected_planes} EGL plane(s), but {} descriptor(s) were \
                 supplied{}",
                modifier_name(modifier),
                planes.len(),
                if aux_expected {
                    " (the DCC metadata plane is missing)"
                } else {
                    ""
                }
            ));
        }

        let row_bytes = width
            .checked_mul(4)
            .ok_or_else(|| "tiled DMA-BUF row length overflows".to_owned())?;
        let mut owned = Vec::with_capacity(planes.len());
        for (index, (fd, offset, stride)) in planes.into_iter().enumerate() {
            let stat = nix::sys::stat::fstat(&fd).ok();
            if index == 0 {
                if stride < row_bytes {
                    return Err(format!(
                        "tiled DMA-BUF stride {stride} is smaller than a {width}-pixel BGRA row \
                         ({row_bytes})"
                    ));
                }
                let needed = (offset as u64)
                    .checked_add((stride as u64).saturating_mul(height as u64))
                    .ok_or_else(|| "tiled DMA-BUF plane length overflows".to_owned())?;
                if let Some(stat) = stat {
                    if stat.st_size > 0 && (stat.st_size as u64) < needed {
                        return Err(format!(
                            "tiled DMA-BUF reports {} bytes; {needed} are required",
                            stat.st_size
                        ));
                    }
                }
            } else if let Some(stat) = stat {
                // The DCC metadata plane is not laid out as `stride * height`
                // pixel rows, so only check that its offset is inside the
                // descriptor.
                if stat.st_size > 0 && (stat.st_size as u64) <= offset as u64 {
                    return Err(format!(
                        "DCC metadata plane offset {offset} is outside its {}-byte descriptor",
                        stat.st_size
                    ));
                }
            }
            owned.push(OwnedPlane { fd, offset, stride });
        }

        let loaded = load();
        let Some(api) = loaded.api.as_ref() else {
            return Err(loaded
                .error
                .clone()
                .unwrap_or_else(|| "EGL/GBM loader failed".to_owned()));
        };

        let plane_attrs: Vec<PlaneAttrs> = owned
            .iter()
            .map(|plane| PlaneAttrs {
                fd: plane.fd.as_raw_fd(),
                offset: plane.offset,
                stride: plane.stride,
            })
            .collect();

        let mut failures = Vec::new();
        for node in candidate_devices() {
            let path = node.display().to_string();
            match EglState::new(api, &node) {
                Ok(mut state) => {
                    match state.bind_dmabuf(&plane_attrs, width, height, fourcc, modifier) {
                        Ok(()) => {
                            return Ok(Self {
                                state,
                                planes: owned,
                                modifier,
                            })
                        }
                        Err(error) => failures.push(format!("{path}: {error}")),
                    }
                }
                Err(error) => failures.push(format!("{path}: {error}")),
            }
        }
        if failures.is_empty() {
            Err("no /dev/dri/renderD* node was available for EGL import".to_owned())
        } else {
            Err(format!(
                "no EGL render node accepted {} with {} ({} plane(s)): {}",
                crate::dbus_dmabuf::fourcc_name(fourcc),
                modifier_name(modifier),
                plane_attrs.len(),
                failures.join("; ")
            ))
        }
    }

    /// The modifier this buffer was imported with.
    pub(crate) fn modifier(&self) -> u64 {
        self.modifier
    }

    fn sync(&self, start: bool) -> Result<(), String> {
        for plane in &self.planes {
            crate::dbus_dmabuf::sync_fd(plane.fd.as_raw_fd(), start)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Read the current backing buffer into tightly packed BGRA in DMA-BUF row
    /// order (row 0 first).
    pub(crate) fn read_bgra(&self) -> Result<Vec<u8>, String> {
        self.sync(true)?;
        let result = self.state.read_bgra();
        let ended = self.sync(false);
        match (result, ended) {
            (Err(error), _) => Err(error),
            (Ok(_pixels), Err(error)) => Err(error),
            (Ok(pixels), Ok(())) => Ok(pixels),
        }
    }
}

#[cfg(test)]
impl TiledEglBuffer {
    /// Create a tiled GBM buffer, paint four quadrants through GL, and return
    /// its single descriptor plus the EGL-reported plane 0 stride.
    ///
    /// This is a real driver buffer, not a synthetic byte layout: GBM performs
    /// the tiling/swizzling and GL writes through the same EGLImage path this
    /// module uses for QEMU scanouts.
    pub(crate) fn synthetic_fixture(
        modifier: u64,
        width: u32,
        height: u32,
        colors: [[f32; 4]; 4],
    ) -> Result<(OwnedFd, u32), String> {
        let (mut planes, stride) = Self::synthetic_fixture_planes(modifier, width, height, colors)?;
        if planes.len() != 1 {
            return Err(format!(
                "modifier {} produced {} planes; use synthetic_fixture_planes",
                modifier_name(modifier),
                planes.len()
            ));
        }
        let (fd, _, _) = planes.pop().expect("checked one plane");
        Ok((fd, stride))
    }

    /// Like [`Self::synthetic_fixture`], but returns every plane GBM exported.
    ///
    /// AMD DCC buffers have a second (metadata) plane; the returned vector is
    /// `(fd, offset, stride)` per plane and can be passed to
    /// [`Self::new_with_planes`].
    pub(crate) fn synthetic_fixture_planes(
        modifier: u64,
        width: u32,
        height: u32,
        colors: [[f32; 4]; 4],
    ) -> Result<(Vec<(OwnedFd, u32, u32)>, u32), String> {
        if width < 2 || height < 2 {
            return Err("fixture dimensions must be at least 2x2".to_owned());
        }
        classify_modifier(modifier, crate::dbus_dmabuf::DRM_FORMAT_XRGB8888)?;
        let loaded = load();
        let Some(api) = loaded.api.as_ref() else {
            return Err(loaded
                .error
                .clone()
                .unwrap_or_else(|| "EGL/GBM loader failed".to_owned()));
        };
        let mut failures = Vec::new();
        for node in candidate_devices() {
            let path = node.display().to_string();
            let mut state = match EglState::new(api, &node) {
                Ok(state) => state,
                Err(error) => {
                    failures.push(format!("{path}: {error}"));
                    continue;
                }
            };
            let bo = unsafe {
                (api.gbm_bo_create_with_modifiers)(
                    state.gbm_device,
                    width,
                    height,
                    crate::dbus_dmabuf::DRM_FORMAT_XRGB8888,
                    &modifier,
                    1,
                )
            };
            if bo.is_null() {
                failures.push(format!("{path}: gbm_bo_create_with_modifiers failed"));
                continue;
            }
            let reported = unsafe { (api.gbm_bo_get_modifier)(bo) };
            let planes = unsafe { gbm_bo_planes(api, bo) };
            unsafe { (api.gbm_bo_destroy)(bo) };
            let planes = match planes {
                Ok(planes) => planes,
                Err(error) => {
                    failures.push(format!("{path}: {error}"));
                    continue;
                }
            };
            if reported != modifier {
                failures.push(format!(
                    "{path}: GBM returned modifier {reported:#018x}, expected {modifier:#018x}"
                ));
                continue;
            }
            let stride0 = planes.first().map(|plane| plane.2).unwrap_or(0);
            let attrs: Vec<PlaneAttrs> = planes
                .iter()
                .map(|(fd, offset, stride)| PlaneAttrs {
                    fd: fd.as_raw_fd(),
                    offset: *offset,
                    stride: *stride,
                })
                .collect();
            if state
                .bind_dmabuf(
                    &attrs,
                    width,
                    height,
                    crate::dbus_dmabuf::DRM_FORMAT_XRGB8888,
                    modifier,
                )
                .is_err()
            {
                failures.push(format!("{path}: could not bind fixture"));
                continue;
            }
            state.paint_quadrants(colors)?;
            return Ok((planes, stride0));
        }
        Err(format!(
            "no render node could create the tiled fixture: {}",
            failures.join("; ")
        ))
    }
}
