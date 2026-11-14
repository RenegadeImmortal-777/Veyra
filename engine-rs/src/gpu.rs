// Veyra GPU / Compute Acceleration Engine:
// - On Windows: dynamically loads DXGI, Direct3D 11 (`d3d11.dll`), and HLSL
//   shader compiler (`d3dcompiler_47.dll`), enumerates real hardware adapters,
//   compiles `cs_5_0` compute shaders, and executes matrix multiplication,
//   elementwise vector/activation kernels, softmax, 2D convolution, and custom
//   user HLSL compute shaders directly on the GPU (with WARP fallback).
// - On non-Windows or headless systems without a GPU adapter: transparently
//   executes multi-threaded CPU compute while reporting `backend = "cpu-fallback"`.

#![allow(clippy::too_many_arguments)]

use std::{
    collections::HashMap,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};

use crate::tensor::Conv2dDims;
use crate::value::Val;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum BackendPref {
    Auto = 0,
    Vulkan = 1,
    D3D11 = 2,
    Cpu = 3,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum PrecisionPref {
    F32 = 0,
    F64 = 1,
}

static BACKEND_PREF: AtomicU8 = AtomicU8::new(BackendPref::Auto as u8);
static PRECISION_PREF: AtomicU8 = AtomicU8::new(PrecisionPref::F32 as u8);

fn current_backend_pref() -> BackendPref {
    match BACKEND_PREF.load(Ordering::Relaxed) {
        1 => BackendPref::Vulkan,
        2 => BackendPref::D3D11,
        3 => BackendPref::Cpu,
        _ => BackendPref::Auto,
    }
}

fn current_precision_pref() -> PrecisionPref {
    if PRECISION_PREF.load(Ordering::Relaxed) == 1 {
        PrecisionPref::F64
    } else {
        PrecisionPref::F32
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AdapterSummary {
    pub(crate) index: usize,
    pub(crate) name: String,
    pub(crate) vendor_id: u32,
    pub(crate) device_id: u32,
    pub(crate) dedicated_vram_mb: f64,
    pub(crate) shared_mem_mb: f64,
    pub(crate) is_software: bool,
}

static GPU_DISPATCHES: AtomicU64 = AtomicU64::new(0);
static GPU_SHADERS_COMPILED: AtomicUsize = AtomicUsize::new(0);
static GPU_FUSED_KERNELS: AtomicUsize = AtomicUsize::new(0);

#[cfg(windows)]
mod d3d11_backend {
    use super::*;
    use core::ffi::{c_char, c_void};
    use libloading::{Library, Symbol};

    const S_OK: i32 = 0;
    const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;
    const D3D_DRIVER_TYPE_UNKNOWN: u32 = 0;
    const D3D_DRIVER_TYPE_HARDWARE: u32 = 1;
    const D3D_DRIVER_TYPE_WARP: u32 = 5;
    const D3D11_SDK_VERSION: u32 = 7;
    const D3D11_FEATURE_DOUBLES: u32 = 1;

    #[repr(C)]
    struct D3D11FeatureDataDoubles {
        double_precision_float_shader_ops: i32,
    }

    const D3D11_USAGE_DEFAULT: u32 = 0;
    const D3D11_USAGE_STAGING: u32 = 3;
    const D3D11_BIND_CONSTANT_BUFFER: u32 = 0x4;
    const D3D11_BIND_SHADER_RESOURCE: u32 = 0x8;
    const D3D11_BIND_UNORDERED_ACCESS: u32 = 0x80;
    const D3D11_CPU_ACCESS_READ: u32 = 0x20000;
    const D3D11_RESOURCE_MISC_BUFFER_STRUCTURED: u32 = 0x40;
    const D3D11_SRV_DIMENSION_BUFFER: u32 = 1;
    const D3D11_UAV_DIMENSION_BUFFER: u32 = 1;
    const D3D11_MAP_READ: u32 = 1;
    const DXGI_FORMAT_UNKNOWN: u32 = 0;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }

    const IID_IDXGIFACTORY1: Guid = Guid {
        data1: 0x770a_ae78,
        data2: 0xf26f,
        data3: 0x4dba,
        data4: [0xa8, 0x29, 0x25, 0x3c, 0x83, 0xd1, 0xb3, 0x87],
    };

    #[repr(C)]
    struct DxgiAdapterDesc1 {
        description: [u16; 128],
        vendor_id: u32,
        device_id: u32,
        sub_sys_id: u32,
        revision: u32,
        dedicated_video_memory: usize,
        dedicated_system_memory: usize,
        shared_system_memory: usize,
        adapter_luid: [u32; 2],
        flags: u32,
    }

    #[repr(C)]
    struct D3D11BufferDesc {
        byte_width: u32,
        usage: u32,
        bind_flags: u32,
        cpu_access_flags: u32,
        misc_flags: u32,
        structure_byte_stride: u32,
    }

    #[repr(C)]
    struct D3D11SubresourceData {
        sys_mem: *const c_void,
        sys_mem_pitch: u32,
        sys_mem_slice_pitch: u32,
    }

    #[repr(C)]
    struct D3D11MappedSubresource {
        data: *mut c_void,
        row_pitch: u32,
        depth_pitch: u32,
    }

    #[repr(C)]
    struct D3D11ShaderResourceViewDesc {
        format: u32,
        view_dimension: u32,
        first_element: u32,
        num_elements: u32,
        _pad: [u32; 2],
    }

    #[repr(C)]
    struct D3D11UnorderedAccessViewDesc {
        format: u32,
        view_dimension: u32,
        first_element: u32,
        num_elements: u32,
        flags: u32,
        _pad: u32,
    }

    #[derive(Clone, Copy)]
    struct ComPtr(*mut c_void);

    impl ComPtr {
        unsafe fn vtbl(&self) -> *const *const c_void {
            unsafe { *(self.0 as *const *const *const c_void) }
        }
        unsafe fn method<T>(&self, slot: usize) -> T {
            unsafe {
                let p = *self.vtbl().add(slot);
                core::mem::transmute_copy(&p)
            }
        }
        unsafe fn release(&self) {
            if !self.0.is_null() {
                let f: unsafe extern "system" fn(*mut c_void) -> u32 = unsafe { self.method(2) };
                unsafe {
                    let _ = f(self.0);
                }
            }
        }
    }

    type FnCreateDxgiFactory1 = unsafe extern "system" fn(*const Guid, *mut *mut c_void) -> i32;
    type FnD3D11CreateDevice = unsafe extern "system" fn(
        *mut c_void,
        u32,
        *mut c_void,
        u32,
        *const u32,
        u32,
        u32,
        *mut *mut c_void,
        *mut u32,
        *mut *mut c_void,
    ) -> i32;
    type FnD3DCompile = unsafe extern "system" fn(
        *const c_void,
        usize,
        *const c_char,
        *const c_void,
        *const c_void,
        *const c_char,
        *const c_char,
        u32,
        u32,
        *mut *mut c_void,
        *mut *mut c_void,
    ) -> i32;

    pub(super) struct D3D11State {
        _dxgi: Library,
        _d3d11: Library,
        _d3dcompiler: Library,
        d3d_compile: FnD3DCompile,
        device: ComPtr,
        context: ComPtr,
        pub(super) backend: String,
        pub(super) active_adapter: AdapterSummary,
        pub(super) adapters: Vec<AdapterSummary>,
        pub(super) feature_level: u32,
        pub(super) is_hardware: bool,
        pub(super) supports_f64: bool,
        shaders: HashMap<String, ComPtr>,
    }

    // Safety: COM pointers inside D3D11State are protected by a global Mutex and only used under lock.
    unsafe impl Send for D3D11State {}

    impl Drop for D3D11State {
        fn drop(&mut self) {
            for (_, cs) in self.shaders.drain() {
                unsafe { cs.release() };
            }
            unsafe {
                self.context.release();
                self.device.release();
            }
        }
    }

    impl D3D11State {
        pub(super) fn init() -> Result<Self, String> {
            let dxgi = unsafe { Library::new("dxgi.dll") }.map_err(|e| e.to_string())?;
            let d3d11 = unsafe { Library::new("d3d11.dll") }.map_err(|e| e.to_string())?;
            let d3dcompiler =
                unsafe { Library::new("d3dcompiler_47.dll") }.map_err(|e| e.to_string())?;

            let create_factory: Symbol<FnCreateDxgiFactory1> =
                unsafe { dxgi.get(b"CreateDXGIFactory1\0") }.map_err(|e| e.to_string())?;
            let create_device: Symbol<FnD3D11CreateDevice> =
                unsafe { d3d11.get(b"D3D11CreateDevice\0") }.map_err(|e| e.to_string())?;
            let d3d_compile_sym: Symbol<FnD3DCompile> =
                unsafe { d3dcompiler.get(b"D3DCompile\0") }.map_err(|e| e.to_string())?;
            let d3d_compile = *d3d_compile_sym;

            let mut factory_raw: *mut c_void = core::ptr::null_mut();
            let hr = unsafe { create_factory(&IID_IDXGIFACTORY1, &mut factory_raw) };
            let mut adapters = Vec::new();
            let mut best_hw_adapter: Option<(usize, *mut c_void, AdapterSummary)> = None;

            if hr >= S_OK && !factory_raw.is_null() {
                let factory = ComPtr(factory_raw);
                let enum_adapters1: unsafe extern "system" fn(
                    *mut c_void,
                    u32,
                    *mut *mut c_void,
                ) -> i32 = unsafe { factory.method(12) };
                let mut idx = 0u32;
                loop {
                    let mut ad_raw: *mut c_void = core::ptr::null_mut();
                    let a_hr = unsafe { enum_adapters1(factory.0, idx, &mut ad_raw) };
                    if a_hr < S_OK || ad_raw.is_null() {
                        break;
                    }
                    let ad = ComPtr(ad_raw);
                    let get_desc1: unsafe extern "system" fn(
                        *mut c_void,
                        *mut DxgiAdapterDesc1,
                    ) -> i32 = unsafe { ad.method(10) };
                    let mut desc: DxgiAdapterDesc1 = unsafe { core::mem::zeroed() };
                    let d_hr = unsafe { get_desc1(ad.0, &mut desc) };
                    if d_hr >= S_OK {
                        let end = desc
                            .description
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(desc.description.len());
                        let name = String::from_utf16_lossy(&desc.description[..end]);
                        let is_sw = (desc.flags & DXGI_ADAPTER_FLAG_SOFTWARE) != 0;
                        let summary = AdapterSummary {
                            index: idx as usize,
                            name,
                            vendor_id: desc.vendor_id,
                            device_id: desc.device_id,
                            dedicated_vram_mb: (desc.dedicated_video_memory as f64)
                                / (1024.0 * 1024.0),
                            shared_mem_mb: (desc.shared_system_memory as f64) / (1024.0 * 1024.0),
                            is_software: is_sw,
                        };
                        adapters.push(summary.clone());
                        if !is_sw && best_hw_adapter.is_none() {
                            best_hw_adapter = Some((idx as usize, ad_raw, summary));
                        } else {
                            unsafe { ad.release() };
                        }
                    } else {
                        unsafe { ad.release() };
                    }
                    idx += 1;
                }
                unsafe { factory.release() };
            }

            let feature_levels = [0xB100u32, 0xB000u32, 0xA100u32, 0xA000u32];
            let mut dev_raw: *mut c_void = core::ptr::null_mut();
            let mut ctx_raw: *mut c_void = core::ptr::null_mut();
            let mut feat: u32 = 0;
            let mut is_hardware = false;
            let mut active_adapter = AdapterSummary {
                index: 0,
                name: "Software WARP".into(),
                vendor_id: 0,
                device_id: 0,
                dedicated_vram_mb: 0.0,
                shared_mem_mb: 0.0,
                is_software: true,
            };

            let mut dev_hr = -1i32;
            if let Some((_, ad_raw, summary)) = best_hw_adapter {
                dev_hr = unsafe {
                    create_device(
                        ad_raw,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        core::ptr::null_mut(),
                        0,
                        feature_levels.as_ptr(),
                        feature_levels.len() as u32,
                        D3D11_SDK_VERSION,
                        &mut dev_raw,
                        &mut feat,
                        &mut ctx_raw,
                    )
                };
                unsafe { ComPtr(ad_raw).release() };
                if dev_hr >= S_OK && !dev_raw.is_null() && feat >= 0xB000 {
                    is_hardware = true;
                    active_adapter = summary;
                } else {
                    if !ctx_raw.is_null() {
                        unsafe { ComPtr(ctx_raw).release() };
                        ctx_raw = core::ptr::null_mut();
                    }
                    if !dev_raw.is_null() {
                        unsafe { ComPtr(dev_raw).release() };
                        dev_raw = core::ptr::null_mut();
                    }
                }
            }

            if dev_hr < S_OK || dev_raw.is_null() {
                dev_hr = unsafe {
                    create_device(
                        core::ptr::null_mut(),
                        D3D_DRIVER_TYPE_HARDWARE,
                        core::ptr::null_mut(),
                        0,
                        feature_levels.as_ptr(),
                        feature_levels.len() as u32,
                        D3D11_SDK_VERSION,
                        &mut dev_raw,
                        &mut feat,
                        &mut ctx_raw,
                    )
                };
                if dev_hr >= S_OK && !dev_raw.is_null() && feat >= 0xB000 {
                    is_hardware = true;
                    if let Some(hw) = adapters.iter().find(|a| !a.is_software) {
                        active_adapter = hw.clone();
                    } else {
                        active_adapter.name = "Hardware D3D11 GPU".into();
                        active_adapter.is_software = false;
                    }
                }
            }

            if dev_hr < S_OK || dev_raw.is_null() || feat < 0xB000 {
                if !ctx_raw.is_null() {
                    unsafe { ComPtr(ctx_raw).release() };
                }
                if !dev_raw.is_null() {
                    unsafe { ComPtr(dev_raw).release() };
                }
                dev_raw = core::ptr::null_mut();
                ctx_raw = core::ptr::null_mut();
                let warp_hr = unsafe {
                    create_device(
                        core::ptr::null_mut(),
                        D3D_DRIVER_TYPE_WARP,
                        core::ptr::null_mut(),
                        0,
                        feature_levels.as_ptr(),
                        feature_levels.len() as u32,
                        D3D11_SDK_VERSION,
                        &mut dev_raw,
                        &mut feat,
                        &mut ctx_raw,
                    )
                };
                if warp_hr < S_OK || dev_raw.is_null() || ctx_raw.is_null() || feat < 0xB000 {
                    if !ctx_raw.is_null() {
                        unsafe { ComPtr(ctx_raw).release() };
                    }
                    if !dev_raw.is_null() {
                        unsafe { ComPtr(dev_raw).release() };
                    }
                    return Err("no DirectCompute cs_5_0 adapter available".into());
                }
                is_hardware = false;
            }

            let backend = if is_hardware {
                "d3d11-compute".to_string()
            } else {
                "d3d11-warp".to_string()
            };

            let dev_ptr = ComPtr(dev_raw);
            let check_feature: unsafe extern "system" fn(
                *mut c_void,
                u32,
                *mut c_void,
                u32,
            ) -> i32 = unsafe { dev_ptr.method(33) };
            let mut doubles = D3D11FeatureDataDoubles {
                double_precision_float_shader_ops: 0,
            };
            let fhr = unsafe {
                check_feature(
                    dev_ptr.0,
                    D3D11_FEATURE_DOUBLES,
                    (&mut doubles as *mut D3D11FeatureDataDoubles) as *mut c_void,
                    core::mem::size_of::<D3D11FeatureDataDoubles>() as u32,
                )
            };
            let supports_f64 = fhr >= S_OK && doubles.double_precision_float_shader_ops != 0;

            Ok(Self {
                _dxgi: dxgi,
                _d3d11: d3d11,
                _d3dcompiler: d3dcompiler,
                d3d_compile,
                device: dev_ptr,
                context: ComPtr(ctx_raw),
                backend,
                active_adapter,
                adapters,
                feature_level: feat,
                is_hardware,
                supports_f64,
                shaders: HashMap::new(),
            })
        }

        fn compile_or_get_shader(
            &mut self,
            cache_key: &str,
            source: &str,
            entry: &str,
        ) -> Result<ComPtr, String> {
            if let Some(&shader) = self.shaders.get(cache_key) {
                return Ok(shader);
            }
            let entry_c = std::ffi::CString::new(entry).map_err(|e| e.to_string())?;
            let target_c = b"cs_5_0\0";
            let name_c = b"veyra_compute.hlsl\0";
            let mut code_raw: *mut c_void = core::ptr::null_mut();
            let mut err_raw: *mut c_void = core::ptr::null_mut();
            let hr = unsafe {
                (self.d3d_compile)(
                    source.as_ptr() as *const c_void,
                    source.len(),
                    name_c.as_ptr() as *const c_char,
                    core::ptr::null(),
                    core::ptr::null(),
                    entry_c.as_ptr(),
                    target_c.as_ptr() as *const c_char,
                    0,
                    0,
                    &mut code_raw,
                    &mut err_raw,
                )
            };
            if hr < S_OK || code_raw.is_null() {
                let err_msg = if !err_raw.is_null() {
                    let blob = ComPtr(err_raw);
                    let get_ptr: unsafe extern "system" fn(*mut c_void) -> *const c_void =
                        unsafe { blob.method(3) };
                    let get_sz: unsafe extern "system" fn(*mut c_void) -> usize =
                        unsafe { blob.method(4) };
                    let p = unsafe { get_ptr(blob.0) as *const u8 };
                    let sz = unsafe { get_sz(blob.0) };
                    let msg = if !p.is_null() && sz > 0 {
                        String::from_utf8_lossy(unsafe { core::slice::from_raw_parts(p, sz) })
                            .trim()
                            .to_string()
                    } else {
                        format!("HLSL compile error 0x{:08X}", hr as u32)
                    };
                    unsafe { blob.release() };
                    msg
                } else {
                    format!("D3DCompile failed with hr=0x{:08X}", hr as u32)
                };
                return Err(err_msg);
            }
            if !err_raw.is_null() {
                unsafe { ComPtr(err_raw).release() };
            }
            let blob = ComPtr(code_raw);
            let get_ptr: unsafe extern "system" fn(*mut c_void) -> *const c_void =
                unsafe { blob.method(3) };
            let get_sz: unsafe extern "system" fn(*mut c_void) -> usize = unsafe { blob.method(4) };
            let bytecode = unsafe { get_ptr(blob.0) };
            let len = unsafe { get_sz(blob.0) };

            let create_cs: unsafe extern "system" fn(
                *mut c_void,
                *const c_void,
                usize,
                *mut c_void,
                *mut *mut c_void,
            ) -> i32 = unsafe { self.device.method(18) };
            let mut cs_raw: *mut c_void = core::ptr::null_mut();
            let cs_hr = unsafe {
                create_cs(
                    self.device.0,
                    bytecode,
                    len,
                    core::ptr::null_mut(),
                    &mut cs_raw,
                )
            };
            unsafe { blob.release() };
            if cs_hr < S_OK || cs_raw.is_null() {
                return Err(format!(
                    "ID3D11Device::CreateComputeShader failed 0x{:08X}",
                    cs_hr as u32
                ));
            }
            let shader = ComPtr(cs_raw);
            self.shaders.insert(cache_key.to_string(), shader);
            GPU_SHADERS_COMPILED.fetch_add(1, Ordering::Relaxed);
            Ok(shader)
        }

        fn create_structured_srv_raw(
            &self,
            bytes: &[u8],
            stride: usize,
            fallback_len: usize,
        ) -> Result<(ComPtr, ComPtr), String> {
            let safe_zero = [0u8; 8];
            let (ptr, byte_width, num_elements) = if bytes.is_empty() {
                (
                    safe_zero.as_ptr() as *const c_void,
                    fallback_len as u32,
                    1u32,
                )
            } else {
                (
                    bytes.as_ptr() as *const c_void,
                    bytes.len() as u32,
                    (bytes.len() / stride) as u32,
                )
            };
            let desc = D3D11BufferDesc {
                byte_width,
                usage: D3D11_USAGE_DEFAULT,
                bind_flags: D3D11_BIND_SHADER_RESOURCE,
                cpu_access_flags: 0,
                misc_flags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED,
                structure_byte_stride: stride as u32,
            };
            let init = D3D11SubresourceData {
                sys_mem: ptr,
                sys_mem_pitch: 0,
                sys_mem_slice_pitch: 0,
            };
            let create_buffer: unsafe extern "system" fn(
                *mut c_void,
                *const D3D11BufferDesc,
                *const D3D11SubresourceData,
                *mut *mut c_void,
            ) -> i32 = unsafe { self.device.method(3) };
            let mut buf_raw: *mut c_void = core::ptr::null_mut();
            let hr = unsafe { create_buffer(self.device.0, &desc, &init, &mut buf_raw) };
            if hr < S_OK || buf_raw.is_null() {
                return Err(format!("CreateBuffer(SRV) failed 0x{:08X}", hr as u32));
            }
            let buf = ComPtr(buf_raw);
            let srv_desc = D3D11ShaderResourceViewDesc {
                format: DXGI_FORMAT_UNKNOWN,
                view_dimension: D3D11_SRV_DIMENSION_BUFFER,
                first_element: 0,
                num_elements,
                _pad: [0; 2],
            };
            let create_srv: unsafe extern "system" fn(
                *mut c_void,
                *mut c_void,
                *const D3D11ShaderResourceViewDesc,
                *mut *mut c_void,
            ) -> i32 = unsafe { self.device.method(7) };
            let mut srv_raw: *mut c_void = core::ptr::null_mut();
            let shr = unsafe { create_srv(self.device.0, buf.0, &srv_desc, &mut srv_raw) };
            if shr < S_OK || srv_raw.is_null() {
                unsafe { buf.release() };
                return Err(format!(
                    "CreateShaderResourceView failed 0x{:08X}",
                    shr as u32
                ));
            }
            Ok((buf, ComPtr(srv_raw)))
        }

        fn create_structured_uav_raw(
            &self,
            count: usize,
            stride: usize,
        ) -> Result<(ComPtr, ComPtr), String> {
            let safe_count = count.max(1);
            let desc = D3D11BufferDesc {
                byte_width: (safe_count * stride) as u32,
                usage: D3D11_USAGE_DEFAULT,
                bind_flags: D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE,
                cpu_access_flags: 0,
                misc_flags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED,
                structure_byte_stride: stride as u32,
            };
            let create_buffer: unsafe extern "system" fn(
                *mut c_void,
                *const D3D11BufferDesc,
                *const D3D11SubresourceData,
                *mut *mut c_void,
            ) -> i32 = unsafe { self.device.method(3) };
            let mut buf_raw: *mut c_void = core::ptr::null_mut();
            let hr =
                unsafe { create_buffer(self.device.0, &desc, core::ptr::null(), &mut buf_raw) };
            if hr < S_OK || buf_raw.is_null() {
                return Err(format!("CreateBuffer(UAV) failed 0x{:08X}", hr as u32));
            }
            let buf = ComPtr(buf_raw);
            let uav_desc = D3D11UnorderedAccessViewDesc {
                format: DXGI_FORMAT_UNKNOWN,
                view_dimension: D3D11_UAV_DIMENSION_BUFFER,
                first_element: 0,
                num_elements: safe_count as u32,
                flags: 0,
                _pad: 0,
            };
            let create_uav: unsafe extern "system" fn(
                *mut c_void,
                *mut c_void,
                *const D3D11UnorderedAccessViewDesc,
                *mut *mut c_void,
            ) -> i32 = unsafe { self.device.method(8) };
            let mut uav_raw: *mut c_void = core::ptr::null_mut();
            let uhr = unsafe { create_uav(self.device.0, buf.0, &uav_desc, &mut uav_raw) };
            if uhr < S_OK || uav_raw.is_null() {
                unsafe { buf.release() };
                return Err(format!(
                    "CreateUnorderedAccessView failed 0x{:08X}",
                    uhr as u32
                ));
            }
            Ok((buf, ComPtr(uav_raw)))
        }

        fn create_cbuffer(&self, words: &[u32]) -> Result<ComPtr, String> {
            let mut padded = words.to_vec();
            while padded.len() % 4 != 0 || padded.is_empty() {
                padded.push(0);
            }
            let desc = D3D11BufferDesc {
                byte_width: (padded.len() * 4) as u32,
                usage: D3D11_USAGE_DEFAULT,
                bind_flags: D3D11_BIND_CONSTANT_BUFFER,
                cpu_access_flags: 0,
                misc_flags: 0,
                structure_byte_stride: 0,
            };
            let init = D3D11SubresourceData {
                sys_mem: padded.as_ptr() as *const c_void,
                sys_mem_pitch: 0,
                sys_mem_slice_pitch: 0,
            };
            let create_buffer: unsafe extern "system" fn(
                *mut c_void,
                *const D3D11BufferDesc,
                *const D3D11SubresourceData,
                *mut *mut c_void,
            ) -> i32 = unsafe { self.device.method(3) };
            let mut cb_raw: *mut c_void = core::ptr::null_mut();
            let hr = unsafe { create_buffer(self.device.0, &desc, &init, &mut cb_raw) };
            if hr < S_OK || cb_raw.is_null() {
                return Err(format!("CreateBuffer(CB) failed 0x{:08X}", hr as u32));
            }
            Ok(ComPtr(cb_raw))
        }

        fn read_back_uav_raw(
            &self,
            out_buf: ComPtr,
            count: usize,
            stride: usize,
        ) -> Result<Vec<u8>, String> {
            let safe_count = count.max(1);
            let total_bytes = count * stride;
            let desc = D3D11BufferDesc {
                byte_width: (safe_count * stride) as u32,
                usage: D3D11_USAGE_STAGING,
                bind_flags: 0,
                cpu_access_flags: D3D11_CPU_ACCESS_READ,
                misc_flags: 0,
                structure_byte_stride: 0,
            };
            let create_buffer: unsafe extern "system" fn(
                *mut c_void,
                *const D3D11BufferDesc,
                *const D3D11SubresourceData,
                *mut *mut c_void,
            ) -> i32 = unsafe { self.device.method(3) };
            let mut staging_raw: *mut c_void = core::ptr::null_mut();
            let hr =
                unsafe { create_buffer(self.device.0, &desc, core::ptr::null(), &mut staging_raw) };
            if hr < S_OK || staging_raw.is_null() {
                return Err(format!("CreateBuffer(Staging) failed 0x{:08X}", hr as u32));
            }
            let staging = ComPtr(staging_raw);
            let copy_resource: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) =
                unsafe { self.context.method(47) };
            unsafe { copy_resource(self.context.0, staging.0, out_buf.0) };

            let map_fn: unsafe extern "system" fn(
                *mut c_void,
                *mut c_void,
                u32,
                u32,
                u32,
                *mut D3D11MappedSubresource,
            ) -> i32 = unsafe { self.context.method(14) };
            let unmap_fn: unsafe extern "system" fn(*mut c_void, *mut c_void, u32) =
                unsafe { self.context.method(15) };
            let mut mapped = D3D11MappedSubresource {
                data: core::ptr::null_mut(),
                row_pitch: 0,
                depth_pitch: 0,
            };
            let mhr =
                unsafe { map_fn(self.context.0, staging.0, 0, D3D11_MAP_READ, 0, &mut mapped) };
            if mhr < S_OK || mapped.data.is_null() {
                unsafe { staging.release() };
                return Err(format!(
                    "ID3D11DeviceContext::Map failed 0x{:08X}",
                    mhr as u32
                ));
            }
            let out = unsafe {
                core::slice::from_raw_parts(mapped.data as *const u8, total_bytes).to_vec()
            };
            unsafe {
                unmap_fn(self.context.0, staging.0, 0);
                staging.release();
            }
            Ok(out)
        }

        fn dispatch_raw(
            &mut self,
            cache_key: &str,
            hlsl: &str,
            entry: &str,
            a_bytes: &[u8],
            b_bytes: &[u8],
            out_len: usize,
            stride: usize,
            cb_words: &[u32],
            groups: (u32, u32, u32),
        ) -> Result<Vec<u8>, String> {
            let shader = self.compile_or_get_shader(cache_key, hlsl, entry)?;
            let (buf_a, srv_a) = self.create_structured_srv_raw(a_bytes, stride, stride)?;
            let (buf_b, srv_b) = match self.create_structured_srv_raw(b_bytes, stride, stride) {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        srv_a.release();
                        buf_a.release();
                    }
                    return Err(e);
                }
            };
            let (buf_out, uav_out) = match self.create_structured_uav_raw(out_len, stride) {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        srv_b.release();
                        buf_b.release();
                        srv_a.release();
                        buf_a.release();
                    }
                    return Err(e);
                }
            };
            let cb = match self.create_cbuffer(cb_words) {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        uav_out.release();
                        buf_out.release();
                        srv_b.release();
                        buf_b.release();
                        srv_a.release();
                        buf_a.release();
                    }
                    return Err(e);
                }
            };

            let cs_set_srvs: unsafe extern "system" fn(*mut c_void, u32, u32, *const *mut c_void) =
                unsafe { self.context.method(67) };
            let cs_set_uavs: unsafe extern "system" fn(
                *mut c_void,
                u32,
                u32,
                *const *mut c_void,
                *const u32,
            ) = unsafe { self.context.method(68) };
            let cs_set_shader: unsafe extern "system" fn(
                *mut c_void,
                *mut c_void,
                *const *mut c_void,
                u32,
            ) = unsafe { self.context.method(69) };
            let cs_set_cbs: unsafe extern "system" fn(*mut c_void, u32, u32, *const *mut c_void) =
                unsafe { self.context.method(71) };
            let dispatch_fn: unsafe extern "system" fn(*mut c_void, u32, u32, u32) =
                unsafe { self.context.method(41) };

            let srvs = [srv_a.0, srv_b.0];
            let uavs = [uav_out.0];
            let cbs = [cb.0];
            let init_cnt = [0u32];

            unsafe {
                cs_set_shader(self.context.0, shader.0, core::ptr::null(), 0);
                cs_set_srvs(self.context.0, 0, 2, srvs.as_ptr());
                cs_set_uavs(self.context.0, 0, 1, uavs.as_ptr(), init_cnt.as_ptr());
                cs_set_cbs(self.context.0, 0, 1, cbs.as_ptr());
                dispatch_fn(
                    self.context.0,
                    groups.0.max(1),
                    groups.1.max(1),
                    groups.2.max(1),
                );

                let null_srvs = [core::ptr::null_mut(), core::ptr::null_mut()];
                let null_uav = [core::ptr::null_mut()];
                let null_cb = [core::ptr::null_mut()];
                cs_set_srvs(self.context.0, 0, 2, null_srvs.as_ptr());
                cs_set_uavs(self.context.0, 0, 1, null_uav.as_ptr(), init_cnt.as_ptr());
                cs_set_cbs(self.context.0, 0, 1, null_cb.as_ptr());
                cs_set_shader(self.context.0, core::ptr::null_mut(), core::ptr::null(), 0);
            }

            let read_res = if out_len == 0 {
                Ok(Vec::new())
            } else {
                self.read_back_uav_raw(buf_out, out_len, stride)
            };

            unsafe {
                cb.release();
                uav_out.release();
                buf_out.release();
                srv_b.release();
                buf_b.release();
                srv_a.release();
                buf_a.release();
            }

            if read_res.is_ok() {
                GPU_DISPATCHES.fetch_add(1, Ordering::Relaxed);
            }
            read_res
        }

        pub(super) fn dispatch(
            &mut self,
            cache_key: &str,
            hlsl: &str,
            entry: &str,
            a: &[f32],
            b: &[f32],
            out_len: usize,
            cb_words: &[u32],
            groups: (u32, u32, u32),
        ) -> Result<Vec<f32>, String> {
            let ab = unsafe {
                core::slice::from_raw_parts(a.as_ptr() as *const u8, core::mem::size_of_val(a))
            };
            let bb = unsafe {
                core::slice::from_raw_parts(b.as_ptr() as *const u8, core::mem::size_of_val(b))
            };
            let bytes =
                self.dispatch_raw(cache_key, hlsl, entry, ab, bb, out_len, 4, cb_words, groups)?;
            Ok(bytes
                .chunks_exact(4)
                .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect())
        }

        pub(super) fn dispatch_f64(
            &mut self,
            cache_key: &str,
            hlsl: &str,
            entry: &str,
            a: &[f64],
            b: &[f64],
            out_len: usize,
            cb_words: &[u32],
            groups: (u32, u32, u32),
        ) -> Result<Vec<f64>, String> {
            if !self.supports_f64 {
                return Err("D3D11 adapter does not support double-precision shader ops".into());
            }
            let ab = unsafe {
                core::slice::from_raw_parts(a.as_ptr() as *const u8, core::mem::size_of_val(a))
            };
            let bb = unsafe {
                core::slice::from_raw_parts(b.as_ptr() as *const u8, core::mem::size_of_val(b))
            };
            let bytes =
                self.dispatch_raw(cache_key, hlsl, entry, ab, bb, out_len, 8, cb_words, groups)?;
            Ok(bytes
                .chunks_exact(8)
                .map(|c| f64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                .collect())
        }
    }

    static GPU_CTX: OnceLock<Mutex<Result<D3D11State, String>>> = OnceLock::new();

    pub(super) fn with_d3d11<T, F: FnOnce(&mut D3D11State) -> Result<T, String>>(
        f: F,
    ) -> Result<T, String> {
        let cell = GPU_CTX.get_or_init(|| Mutex::new(D3D11State::init()));
        let mut guard = cell
            .lock()
            .map_err(|_| "gpu context poisoned".to_string())?;
        match &mut *guard {
            Ok(state) => f(state),
            Err(e) => Err(e.clone()),
        }
    }
}

mod vulkan_backend {
    use super::*;
    use core::ffi::{CStr, c_char, c_void};
    use libloading::{Library, Symbol};

    const VK_SUCCESS: i32 = 0;
    const VK_STRUCTURE_TYPE_APPLICATION_INFO: u32 = 0;
    const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: u32 = 1;
    const VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: u32 = 2;
    const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: u32 = 3;
    const VK_STRUCTURE_TYPE_SUBMIT_INFO: u32 = 4;
    const VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO: u32 = 5;
    const VK_STRUCTURE_TYPE_FENCE_CREATE_INFO: u32 = 8;
    const VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO: u32 = 12;
    const VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO: u32 = 16;
    const VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO: u32 = 18;
    const VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO: u32 = 29;
    const VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO: u32 = 30;
    const VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO: u32 = 32;
    const VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO: u32 = 33;
    const VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO: u32 = 34;
    const VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET: u32 = 35;
    const VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO: u32 = 39;
    const VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO: u32 = 40;
    const VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO: u32 = 42;

    const VK_API_VERSION_1_0: u32 = 1 << 22;
    const VK_QUEUE_COMPUTE_BIT: u32 = 0x2;
    const VK_SHADER_STAGE_COMPUTE_BIT: u32 = 0x20;
    const VK_DESCRIPTOR_TYPE_STORAGE_BUFFER: u32 = 7;
    const VK_BUFFER_USAGE_STORAGE_BUFFER_BIT: u32 = 0x20;
    const VK_SHARING_MODE_EXCLUSIVE: u32 = 0;
    const VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT: u32 = 0x2;
    const VK_MEMORY_PROPERTY_HOST_COHERENT_BIT: u32 = 0x4;
    const VK_MEMORY_HEAP_DEVICE_LOCAL_BIT: u32 = 0x1;
    const VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT: u32 = 0x2;
    const VK_COMMAND_BUFFER_LEVEL_PRIMARY: u32 = 0;
    const VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT: u32 = 0x1;
    const VK_PIPELINE_BIND_POINT_COMPUTE: u32 = 1;
    const VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU: u32 = 2;
    const VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU: u32 = 1;
    const VK_PHYSICAL_DEVICE_TYPE_CPU: u32 = 4;
    const VK_WHOLE_SIZE: u64 = u64::MAX;

    #[repr(C)]
    struct VkApplicationInfo {
        s_type: u32,
        p_next: *const c_void,
        p_application_name: *const c_char,
        application_version: u32,
        p_engine_name: *const c_char,
        engine_version: u32,
        api_version: u32,
    }

    #[repr(C)]
    struct VkInstanceCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        p_application_info: *const VkApplicationInfo,
        enabled_layer_count: u32,
        pp_enabled_layer_names: *const *const c_char,
        enabled_extension_count: u32,
        pp_enabled_extension_names: *const *const c_char,
    }

    #[repr(C)]
    struct VkDeviceQueueCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        queue_family_index: u32,
        queue_count: u32,
        p_queue_priorities: *const f32,
    }

    #[repr(C)]
    struct VkDeviceCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        queue_create_info_count: u32,
        p_queue_create_infos: *const VkDeviceQueueCreateInfo,
        enabled_layer_count: u32,
        pp_enabled_layer_names: *const *const c_char,
        enabled_extension_count: u32,
        pp_enabled_extension_names: *const *const c_char,
        p_enabled_features: *const c_void,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VkQueueFamilyProperties {
        queue_flags: u32,
        queue_count: u32,
        timestamp_valid_bits: u32,
        min_image_transfer_granularity: [u32; 3],
    }

    #[repr(C)]
    struct VkPhysicalDevicePropertiesBuf {
        api_version: u32,
        driver_version: u32,
        vendor_id: u32,
        device_id: u32,
        device_type: u32,
        device_name: [c_char; 256],
        pipeline_cache_uuid: [u8; 16],
        _rest: [u64; 128],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VkMemoryType {
        property_flags: u32,
        heap_index: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VkMemoryHeap {
        size: u64,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct VkPhysicalDeviceMemoryProperties {
        memory_type_count: u32,
        memory_types: [VkMemoryType; 32],
        memory_heap_count: u32,
        memory_heaps: [VkMemoryHeap; 16],
    }

    #[repr(C)]
    struct VkShaderModuleCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        code_size: usize,
        p_code: *const u32,
    }

    #[repr(C)]
    struct VkDescriptorSetLayoutBinding {
        binding: u32,
        descriptor_type: u32,
        descriptor_count: u32,
        stage_flags: u32,
        p_immutable_samplers: *const c_void,
    }

    #[repr(C)]
    struct VkDescriptorSetLayoutCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        binding_count: u32,
        p_bindings: *const VkDescriptorSetLayoutBinding,
    }

    #[repr(C)]
    struct VkPushConstantRange {
        stage_flags: u32,
        offset: u32,
        size: u32,
    }

    #[repr(C)]
    struct VkPipelineLayoutCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        set_layout_count: u32,
        p_set_layouts: *const u64,
        push_constant_range_count: u32,
        p_push_constant_ranges: *const VkPushConstantRange,
    }

    #[repr(C)]
    struct VkPipelineShaderStageCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        stage: u32,
        module: u64,
        p_name: *const c_char,
        p_specialization_info: *const c_void,
    }

    #[repr(C)]
    struct VkComputePipelineCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        stage: VkPipelineShaderStageCreateInfo,
        layout: u64,
        base_pipeline_handle: u64,
        base_pipeline_index: i32,
    }

    #[repr(C)]
    struct VkBufferCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        size: u64,
        usage: u32,
        sharing_mode: u32,
        queue_family_index_count: u32,
        p_queue_family_indices: *const u32,
    }

    #[repr(C)]
    struct VkMemoryRequirements {
        size: u64,
        alignment: u64,
        memory_type_bits: u32,
    }

    #[repr(C)]
    struct VkMemoryAllocateInfo {
        s_type: u32,
        p_next: *const c_void,
        allocation_size: u64,
        memory_type_index: u32,
    }

    #[repr(C)]
    struct VkDescriptorPoolSize {
        ty: u32,
        descriptor_count: u32,
    }

    #[repr(C)]
    struct VkDescriptorPoolCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        max_sets: u32,
        pool_size_count: u32,
        p_pool_sizes: *const VkDescriptorPoolSize,
    }

    #[repr(C)]
    struct VkDescriptorSetAllocateInfo {
        s_type: u32,
        p_next: *const c_void,
        descriptor_pool: u64,
        descriptor_set_count: u32,
        p_set_layouts: *const u64,
    }

    #[repr(C)]
    struct VkDescriptorBufferInfo {
        buffer: u64,
        offset: u64,
        range: u64,
    }

    #[repr(C)]
    struct VkWriteDescriptorSet {
        s_type: u32,
        p_next: *const c_void,
        dst_set: u64,
        dst_binding: u32,
        dst_array_element: u32,
        descriptor_count: u32,
        descriptor_type: u32,
        p_image_info: *const c_void,
        p_buffer_info: *const VkDescriptorBufferInfo,
        p_texel_buffer_view: *const c_void,
    }

    #[repr(C)]
    struct VkCommandPoolCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        queue_family_index: u32,
    }

    #[repr(C)]
    struct VkCommandBufferAllocateInfo {
        s_type: u32,
        p_next: *const c_void,
        command_pool: u64,
        level: u32,
        command_buffer_count: u32,
    }

    #[repr(C)]
    struct VkCommandBufferBeginInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
        p_inheritance_info: *const c_void,
    }

    #[repr(C)]
    struct VkSubmitInfo {
        s_type: u32,
        p_next: *const c_void,
        wait_semaphore_count: u32,
        p_wait_semaphores: *const u64,
        p_wait_dst_stage_mask: *const u32,
        command_buffer_count: u32,
        p_command_buffers: *const *mut c_void,
        signal_semaphore_count: u32,
        p_signal_semaphores: *const u64,
    }

    #[repr(C)]
    struct VkFenceCreateInfo {
        s_type: u32,
        p_next: *const c_void,
        flags: u32,
    }

    type PfnGetInstanceProcAddr =
        unsafe extern "system" fn(*mut c_void, *const c_char) -> *mut c_void;
    type PfnGetDeviceProcAddr =
        unsafe extern "system" fn(*mut c_void, *const c_char) -> *mut c_void;

    unsafe fn vk_inst_proc<T: Copy>(
        gipa: PfnGetInstanceProcAddr,
        instance: *mut c_void,
        name: &CStr,
    ) -> Result<T, String> {
        let p = unsafe { gipa(instance, name.as_ptr()) };
        if p.is_null() {
            return Err(format!(
                "missing Vulkan instance proc {}",
                name.to_string_lossy()
            ));
        }
        Ok(unsafe { core::mem::transmute_copy(&p) })
    }

    unsafe fn vk_dev_proc<T: Copy>(
        gdpa: PfnGetDeviceProcAddr,
        device: *mut c_void,
        name: &CStr,
    ) -> Result<T, String> {
        let p = unsafe { gdpa(device, name.as_ptr()) };
        if p.is_null() {
            return Err(format!(
                "missing Vulkan device proc {}",
                name.to_string_lossy()
            ));
        }
        Ok(unsafe { core::mem::transmute_copy(&p) })
    }

    pub(super) struct VulkanState {
        _lib: Library,
        instance: *mut c_void,
        device: *mut c_void,
        queue: *mut c_void,
        cmd_pool: u64,
        dsl: u64,
        pipeline_layout: u64,
        mem_props: VkPhysicalDeviceMemoryProperties,
        pub(super) active_adapter: AdapterSummary,
        pub(super) adapters: Vec<AdapterSummary>,
        pub(super) api_version: u32,
        pub(super) is_hardware: bool,
        pipelines: HashMap<String, (u64, u64)>,

        destroy_instance: unsafe extern "system" fn(*mut c_void, *const c_void),
        destroy_device: unsafe extern "system" fn(*mut c_void, *const c_void),
        create_shader_module: unsafe extern "system" fn(
            *mut c_void,
            *const VkShaderModuleCreateInfo,
            *const c_void,
            *mut u64,
        ) -> i32,
        destroy_shader_module: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        destroy_dsl: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        destroy_pipeline_layout: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        create_compute_pipelines: unsafe extern "system" fn(
            *mut c_void,
            u64,
            u32,
            *const VkComputePipelineCreateInfo,
            *const c_void,
            *mut u64,
        ) -> i32,
        destroy_pipeline: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        create_buffer: unsafe extern "system" fn(
            *mut c_void,
            *const VkBufferCreateInfo,
            *const c_void,
            *mut u64,
        ) -> i32,
        destroy_buffer: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        get_buf_mem_req: unsafe extern "system" fn(*mut c_void, u64, *mut VkMemoryRequirements),
        alloc_memory: unsafe extern "system" fn(
            *mut c_void,
            *const VkMemoryAllocateInfo,
            *const c_void,
            *mut u64,
        ) -> i32,
        free_memory: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        bind_buffer_memory: unsafe extern "system" fn(*mut c_void, u64, u64, u64) -> i32,
        map_memory:
            unsafe extern "system" fn(*mut c_void, u64, u64, u64, u32, *mut *mut c_void) -> i32,
        unmap_memory: unsafe extern "system" fn(*mut c_void, u64),
        create_desc_pool: unsafe extern "system" fn(
            *mut c_void,
            *const VkDescriptorPoolCreateInfo,
            *const c_void,
            *mut u64,
        ) -> i32,
        destroy_desc_pool: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        alloc_desc_sets: unsafe extern "system" fn(
            *mut c_void,
            *const VkDescriptorSetAllocateInfo,
            *mut u64,
        ) -> i32,
        update_desc_sets: unsafe extern "system" fn(
            *mut c_void,
            u32,
            *const VkWriteDescriptorSet,
            u32,
            *const c_void,
        ),
        destroy_cmd_pool: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        alloc_cmd_buffers: unsafe extern "system" fn(
            *mut c_void,
            *const VkCommandBufferAllocateInfo,
            *mut *mut c_void,
        ) -> i32,
        free_cmd_buffers: unsafe extern "system" fn(*mut c_void, u64, u32, *const *mut c_void),
        begin_cmd_buffer:
            unsafe extern "system" fn(*mut c_void, *const VkCommandBufferBeginInfo) -> i32,
        end_cmd_buffer: unsafe extern "system" fn(*mut c_void) -> i32,
        cmd_bind_pipeline: unsafe extern "system" fn(*mut c_void, u32, u64),
        cmd_bind_desc_sets:
            unsafe extern "system" fn(*mut c_void, u32, u64, u32, u32, *const u64, u32, *const u32),
        cmd_push_constants:
            unsafe extern "system" fn(*mut c_void, u64, u32, u32, u32, *const c_void),
        cmd_dispatch: unsafe extern "system" fn(*mut c_void, u32, u32, u32),
        create_fence: unsafe extern "system" fn(
            *mut c_void,
            *const VkFenceCreateInfo,
            *const c_void,
            *mut u64,
        ) -> i32,
        destroy_fence: unsafe extern "system" fn(*mut c_void, u64, *const c_void),
        wait_for_fences: unsafe extern "system" fn(*mut c_void, u32, *const u64, u32, u64) -> i32,
        queue_submit: unsafe extern "system" fn(*mut c_void, u32, *const VkSubmitInfo, u64) -> i32,
    }

    unsafe impl Send for VulkanState {}

    impl Drop for VulkanState {
        fn drop(&mut self) {
            unsafe {
                for (_, (shader, pipeline)) in self.pipelines.drain() {
                    if pipeline != 0 {
                        (self.destroy_pipeline)(self.device, pipeline, core::ptr::null());
                    }
                    if shader != 0 {
                        (self.destroy_shader_module)(self.device, shader, core::ptr::null());
                    }
                }
                if self.cmd_pool != 0 {
                    (self.destroy_cmd_pool)(self.device, self.cmd_pool, core::ptr::null());
                }
                if self.pipeline_layout != 0 {
                    (self.destroy_pipeline_layout)(
                        self.device,
                        self.pipeline_layout,
                        core::ptr::null(),
                    );
                }
                if self.dsl != 0 {
                    (self.destroy_dsl)(self.device, self.dsl, core::ptr::null());
                }
                if !self.device.is_null() {
                    (self.destroy_device)(self.device, core::ptr::null());
                }
                if !self.instance.is_null() {
                    (self.destroy_instance)(self.instance, core::ptr::null());
                }
            }
        }
    }

    fn load_vulkan_library() -> Result<Library, String> {
        let names: &[&str] = if cfg!(windows) {
            &["vulkan-1.dll"]
        } else if cfg!(target_os = "macos") {
            &["libMoltenVK.dylib", "libvulkan.1.dylib", "libvulkan.dylib"]
        } else {
            &["libvulkan.so.1", "libvulkan.so"]
        };
        for name in names {
            if let Ok(lib) = unsafe { Library::new(name) } {
                return Ok(lib);
            }
        }
        Err("Vulkan loader library not found on system".into())
    }

    impl VulkanState {
        pub(super) fn init() -> Result<Self, String> {
            let lib = load_vulkan_library()?;
            let gipa_sym: Symbol<PfnGetInstanceProcAddr> =
                unsafe { lib.get(b"vkGetInstanceProcAddr\0") }.map_err(|e| e.to_string())?;
            let gipa = *gipa_sym;

            let create_instance: unsafe extern "system" fn(
                *const VkInstanceCreateInfo,
                *const c_void,
                *mut *mut c_void,
            ) -> i32 = unsafe { vk_inst_proc(gipa, core::ptr::null_mut(), c"vkCreateInstance")? };

            let app_info = VkApplicationInfo {
                s_type: VK_STRUCTURE_TYPE_APPLICATION_INFO,
                p_next: core::ptr::null(),
                p_application_name: c"Veyra".as_ptr(),
                application_version: 1,
                p_engine_name: c"VeyraEngine".as_ptr(),
                engine_version: 1,
                api_version: VK_API_VERSION_1_0,
            };
            let port_ext = [c"VK_KHR_portability_enumeration".as_ptr()];
            let mut instance: *mut c_void = core::ptr::null_mut();
            let mut inst_rc = -1;
            if cfg!(target_os = "macos") {
                let ci_port = VkInstanceCreateInfo {
                    s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                    p_next: core::ptr::null(),
                    flags: 1,
                    p_application_info: &app_info,
                    enabled_layer_count: 0,
                    pp_enabled_layer_names: core::ptr::null(),
                    enabled_extension_count: 1,
                    pp_enabled_extension_names: port_ext.as_ptr(),
                };
                inst_rc = unsafe { create_instance(&ci_port, core::ptr::null(), &mut instance) };
            }
            if inst_rc != VK_SUCCESS || instance.is_null() {
                let ci = VkInstanceCreateInfo {
                    s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                    p_next: core::ptr::null(),
                    flags: 0,
                    p_application_info: &app_info,
                    enabled_layer_count: 0,
                    pp_enabled_layer_names: core::ptr::null(),
                    enabled_extension_count: 0,
                    pp_enabled_extension_names: core::ptr::null(),
                };
                inst_rc = unsafe { create_instance(&ci, core::ptr::null(), &mut instance) };
            }
            if inst_rc != VK_SUCCESS || instance.is_null() {
                return Err(format!("vkCreateInstance failed rc={inst_rc}"));
            }

            let destroy_instance: unsafe extern "system" fn(*mut c_void, *const c_void) =
                unsafe { vk_inst_proc(gipa, instance, c"vkDestroyInstance")? };
            let enum_phys: unsafe extern "system" fn(
                *mut c_void,
                *mut u32,
                *mut *mut c_void,
            ) -> i32 = unsafe { vk_inst_proc(gipa, instance, c"vkEnumeratePhysicalDevices")? };
            let get_phys_props: unsafe extern "system" fn(
                *mut c_void,
                *mut VkPhysicalDevicePropertiesBuf,
            ) = unsafe { vk_inst_proc(gipa, instance, c"vkGetPhysicalDeviceProperties")? };
            let get_mem_props: unsafe extern "system" fn(
                *mut c_void,
                *mut VkPhysicalDeviceMemoryProperties,
            ) = unsafe { vk_inst_proc(gipa, instance, c"vkGetPhysicalDeviceMemoryProperties")? };
            let get_q_props: unsafe extern "system" fn(
                *mut c_void,
                *mut u32,
                *mut VkQueueFamilyProperties,
            ) = unsafe {
                vk_inst_proc(gipa, instance, c"vkGetPhysicalDeviceQueueFamilyProperties")?
            };
            let create_device: unsafe extern "system" fn(
                *mut c_void,
                *const VkDeviceCreateInfo,
                *const c_void,
                *mut *mut c_void,
            ) -> i32 = unsafe { vk_inst_proc(gipa, instance, c"vkCreateDevice")? };
            let gdpa: PfnGetDeviceProcAddr =
                unsafe { vk_inst_proc(gipa, instance, c"vkGetDeviceProcAddr")? };

            let mut dev_count = 0u32;
            unsafe { enum_phys(instance, &mut dev_count, core::ptr::null_mut()) };
            if dev_count == 0 {
                unsafe { destroy_instance(instance, core::ptr::null()) };
                return Err("no Vulkan physical devices found".into());
            }
            let mut phys_devs = vec![core::ptr::null_mut(); dev_count as usize];
            unsafe { enum_phys(instance, &mut dev_count, phys_devs.as_mut_ptr()) };

            let mut adapters = Vec::new();
            let mut chosen: Option<(
                i32,
                *mut c_void,
                u32,
                AdapterSummary,
                VkPhysicalDeviceMemoryProperties,
                u32,
            )> = None;
            for (idx, &pd) in phys_devs.iter().enumerate() {
                if pd.is_null() {
                    continue;
                }
                let mut props: VkPhysicalDevicePropertiesBuf = unsafe { core::mem::zeroed() };
                unsafe { get_phys_props(pd, &mut props) };
                let mut mp: VkPhysicalDeviceMemoryProperties = unsafe { core::mem::zeroed() };
                unsafe { get_mem_props(pd, &mut mp) };
                let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
                    .to_string_lossy()
                    .trim()
                    .to_string();
                let mut vram_bytes = 0u64;
                let mut sys_bytes = 0u64;
                for h_idx in 0..(mp.memory_heap_count as usize).min(16) {
                    let heap = mp.memory_heaps[h_idx];
                    if (heap.flags & VK_MEMORY_HEAP_DEVICE_LOCAL_BIT) != 0 {
                        vram_bytes += heap.size;
                    } else {
                        sys_bytes += heap.size;
                    }
                }
                let is_software = props.device_type == VK_PHYSICAL_DEVICE_TYPE_CPU;
                let summary = AdapterSummary {
                    index: idx,
                    name,
                    vendor_id: props.vendor_id,
                    device_id: props.device_id,
                    dedicated_vram_mb: ((vram_bytes as f64) / (1024.0 * 1024.0) * 100.0).round()
                        / 100.0,
                    shared_mem_mb: ((sys_bytes as f64) / (1024.0 * 1024.0) * 100.0).round() / 100.0,
                    is_software,
                };

                let mut qcount = 0u32;
                unsafe { get_q_props(pd, &mut qcount, core::ptr::null_mut()) };
                let mut qprops = vec![
                    VkQueueFamilyProperties {
                        queue_flags: 0,
                        queue_count: 0,
                        timestamp_valid_bits: 0,
                        min_image_transfer_granularity: [0; 3],
                    };
                    qcount as usize
                ];
                if qcount > 0 {
                    unsafe { get_q_props(pd, &mut qcount, qprops.as_mut_ptr()) };
                }
                let compute_qf = qprops
                    .iter()
                    .position(|q| (q.queue_flags & VK_QUEUE_COMPUTE_BIT) != 0 && q.queue_count > 0);

                adapters.push(summary.clone());
                if let Some(qf) = compute_qf {
                    let rank = match props.device_type {
                        VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU => 3,
                        VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU => 2,
                        VK_PHYSICAL_DEVICE_TYPE_CPU => 0,
                        _ => 1,
                    };
                    if chosen.as_ref().is_none_or(|(best_r, ..)| rank > *best_r) {
                        chosen = Some((rank, pd, qf as u32, summary, mp, props.api_version));
                    }
                }
            }

            let Some((_, pd, q_family, active_adapter, mem_props, api_version)) = chosen else {
                unsafe { destroy_instance(instance, core::ptr::null()) };
                return Err("no Vulkan compute queue family available".into());
            };

            let prio = 1.0f32;
            let qci = VkDeviceQueueCreateInfo {
                s_type: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: 0,
                queue_family_index: q_family,
                queue_count: 1,
                p_queue_priorities: &prio,
            };
            let dci = VkDeviceCreateInfo {
                s_type: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: 0,
                queue_create_info_count: 1,
                p_queue_create_infos: &qci,
                enabled_layer_count: 0,
                pp_enabled_layer_names: core::ptr::null(),
                enabled_extension_count: 0,
                pp_enabled_extension_names: core::ptr::null(),
                p_enabled_features: core::ptr::null(),
            };
            let mut device: *mut c_void = core::ptr::null_mut();
            let d_rc = unsafe { create_device(pd, &dci, core::ptr::null(), &mut device) };
            if d_rc != VK_SUCCESS || device.is_null() {
                unsafe { destroy_instance(instance, core::ptr::null()) };
                return Err(format!("vkCreateDevice failed rc={d_rc}"));
            }

            let destroy_device: unsafe extern "system" fn(*mut c_void, *const c_void) =
                unsafe { vk_dev_proc(gdpa, device, c"vkDestroyDevice")? };
            let get_queue: unsafe extern "system" fn(*mut c_void, u32, u32, *mut *mut c_void) =
                unsafe { vk_dev_proc(gdpa, device, c"vkGetDeviceQueue")? };
            let mut queue: *mut c_void = core::ptr::null_mut();
            unsafe { get_queue(device, q_family, 0, &mut queue) };

            let create_dsl: unsafe extern "system" fn(
                *mut c_void,
                *const VkDescriptorSetLayoutCreateInfo,
                *const c_void,
                *mut u64,
            ) -> i32 = unsafe { vk_dev_proc(gdpa, device, c"vkCreateDescriptorSetLayout")? };
            let destroy_dsl: unsafe extern "system" fn(*mut c_void, u64, *const c_void) =
                unsafe { vk_dev_proc(gdpa, device, c"vkDestroyDescriptorSetLayout")? };
            let create_pipeline_layout: unsafe extern "system" fn(
                *mut c_void,
                *const VkPipelineLayoutCreateInfo,
                *const c_void,
                *mut u64,
            ) -> i32 = unsafe { vk_dev_proc(gdpa, device, c"vkCreatePipelineLayout")? };
            let destroy_pipeline_layout: unsafe extern "system" fn(
                *mut c_void,
                u64,
                *const c_void,
            ) = unsafe { vk_dev_proc(gdpa, device, c"vkDestroyPipelineLayout")? };
            let create_cmd_pool: unsafe extern "system" fn(
                *mut c_void,
                *const VkCommandPoolCreateInfo,
                *const c_void,
                *mut u64,
            ) -> i32 = unsafe { vk_dev_proc(gdpa, device, c"vkCreateCommandPool")? };
            let destroy_cmd_pool: unsafe extern "system" fn(*mut c_void, u64, *const c_void) =
                unsafe { vk_dev_proc(gdpa, device, c"vkDestroyCommandPool")? };

            let bindings = [0u32, 1, 2].map(|b| VkDescriptorSetLayoutBinding {
                binding: b,
                descriptor_type: VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                descriptor_count: 1,
                stage_flags: VK_SHADER_STAGE_COMPUTE_BIT,
                p_immutable_samplers: core::ptr::null(),
            });
            let dsl_ci = VkDescriptorSetLayoutCreateInfo {
                s_type: VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: 0,
                binding_count: 3,
                p_bindings: bindings.as_ptr(),
            };
            let mut dsl = 0u64;
            if unsafe { create_dsl(device, &dsl_ci, core::ptr::null(), &mut dsl) } != VK_SUCCESS {
                unsafe {
                    destroy_device(device, core::ptr::null());
                    destroy_instance(instance, core::ptr::null());
                }
                return Err("vkCreateDescriptorSetLayout failed".into());
            }

            let pc_range = VkPushConstantRange {
                stage_flags: VK_SHADER_STAGE_COMPUTE_BIT,
                offset: 0,
                size: 64,
            };
            let pl_ci = VkPipelineLayoutCreateInfo {
                s_type: VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: 0,
                set_layout_count: 1,
                p_set_layouts: &dsl,
                push_constant_range_count: 1,
                p_push_constant_ranges: &pc_range,
            };
            let mut pipeline_layout = 0u64;
            if unsafe {
                create_pipeline_layout(device, &pl_ci, core::ptr::null(), &mut pipeline_layout)
            } != VK_SUCCESS
            {
                unsafe {
                    destroy_dsl(device, dsl, core::ptr::null());
                    destroy_device(device, core::ptr::null());
                    destroy_instance(instance, core::ptr::null());
                }
                return Err("vkCreatePipelineLayout failed".into());
            }

            let cp_ci = VkCommandPoolCreateInfo {
                s_type: VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
                queue_family_index: q_family,
            };
            let mut cmd_pool = 0u64;
            if unsafe { create_cmd_pool(device, &cp_ci, core::ptr::null(), &mut cmd_pool) }
                != VK_SUCCESS
            {
                unsafe {
                    destroy_pipeline_layout(device, pipeline_layout, core::ptr::null());
                    destroy_dsl(device, dsl, core::ptr::null());
                    destroy_device(device, core::ptr::null());
                    destroy_instance(instance, core::ptr::null());
                }
                return Err("vkCreateCommandPool failed".into());
            }

            let is_hardware = !active_adapter.is_software;
            Ok(Self {
                _lib: lib,
                instance,
                device,
                queue,
                cmd_pool,
                dsl,
                pipeline_layout,
                mem_props,
                active_adapter,
                adapters,
                api_version,
                is_hardware,
                pipelines: HashMap::new(),
                destroy_instance,
                destroy_device,
                create_shader_module: unsafe {
                    vk_dev_proc(gdpa, device, c"vkCreateShaderModule")?
                },
                destroy_shader_module: unsafe {
                    vk_dev_proc(gdpa, device, c"vkDestroyShaderModule")?
                },
                destroy_dsl,
                destroy_pipeline_layout,
                create_compute_pipelines: unsafe {
                    vk_dev_proc(gdpa, device, c"vkCreateComputePipelines")?
                },
                destroy_pipeline: unsafe { vk_dev_proc(gdpa, device, c"vkDestroyPipeline")? },
                create_buffer: unsafe { vk_dev_proc(gdpa, device, c"vkCreateBuffer")? },
                destroy_buffer: unsafe { vk_dev_proc(gdpa, device, c"vkDestroyBuffer")? },
                get_buf_mem_req: unsafe {
                    vk_dev_proc(gdpa, device, c"vkGetBufferMemoryRequirements")?
                },
                alloc_memory: unsafe { vk_dev_proc(gdpa, device, c"vkAllocateMemory")? },
                free_memory: unsafe { vk_dev_proc(gdpa, device, c"vkFreeMemory")? },
                bind_buffer_memory: unsafe { vk_dev_proc(gdpa, device, c"vkBindBufferMemory")? },
                map_memory: unsafe { vk_dev_proc(gdpa, device, c"vkMapMemory")? },
                unmap_memory: unsafe { vk_dev_proc(gdpa, device, c"vkUnmapMemory")? },
                create_desc_pool: unsafe { vk_dev_proc(gdpa, device, c"vkCreateDescriptorPool")? },
                destroy_desc_pool: unsafe {
                    vk_dev_proc(gdpa, device, c"vkDestroyDescriptorPool")?
                },
                alloc_desc_sets: unsafe { vk_dev_proc(gdpa, device, c"vkAllocateDescriptorSets")? },
                update_desc_sets: unsafe { vk_dev_proc(gdpa, device, c"vkUpdateDescriptorSets")? },
                destroy_cmd_pool,
                alloc_cmd_buffers: unsafe {
                    vk_dev_proc(gdpa, device, c"vkAllocateCommandBuffers")?
                },
                free_cmd_buffers: unsafe { vk_dev_proc(gdpa, device, c"vkFreeCommandBuffers")? },
                begin_cmd_buffer: unsafe { vk_dev_proc(gdpa, device, c"vkBeginCommandBuffer")? },
                end_cmd_buffer: unsafe { vk_dev_proc(gdpa, device, c"vkEndCommandBuffer")? },
                cmd_bind_pipeline: unsafe { vk_dev_proc(gdpa, device, c"vkCmdBindPipeline")? },
                cmd_bind_desc_sets: unsafe {
                    vk_dev_proc(gdpa, device, c"vkCmdBindDescriptorSets")?
                },
                cmd_push_constants: unsafe { vk_dev_proc(gdpa, device, c"vkCmdPushConstants")? },
                cmd_dispatch: unsafe { vk_dev_proc(gdpa, device, c"vkCmdDispatch")? },
                create_fence: unsafe { vk_dev_proc(gdpa, device, c"vkCreateFence")? },
                destroy_fence: unsafe { vk_dev_proc(gdpa, device, c"vkDestroyFence")? },
                wait_for_fences: unsafe { vk_dev_proc(gdpa, device, c"vkWaitForFences")? },
                queue_submit: unsafe { vk_dev_proc(gdpa, device, c"vkQueueSubmit")? },
            })
        }

        fn find_mem_type(&self, type_bits: u32, req_flags: u32) -> Option<u32> {
            for i in 0..(self.mem_props.memory_type_count as usize).min(32) {
                if (type_bits & (1u32 << i)) != 0
                    && (self.mem_props.memory_types[i].property_flags & req_flags) == req_flags
                {
                    return Some(i as u32);
                }
            }
            None
        }

        fn create_host_buffer(
            &self,
            bytes: &[u8],
            min_bytes: u64,
        ) -> Result<(u64, u64, u64), String> {
            let size = (bytes.len() as u64).max(min_bytes).max(4);
            let bci = VkBufferCreateInfo {
                s_type: VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: 0,
                size,
                usage: VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,
                sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
                queue_family_index_count: 0,
                p_queue_family_indices: core::ptr::null(),
            };
            let mut buf = 0u64;
            let rc =
                unsafe { (self.create_buffer)(self.device, &bci, core::ptr::null(), &mut buf) };
            if rc != VK_SUCCESS || buf == 0 {
                return Err(format!("vkCreateBuffer failed rc={rc}"));
            }
            let mut req = VkMemoryRequirements {
                size: 0,
                alignment: 0,
                memory_type_bits: 0,
            };
            unsafe { (self.get_buf_mem_req)(self.device, buf, &mut req) };
            let mem_idx = self
                .find_mem_type(
                    req.memory_type_bits,
                    VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
                )
                .ok_or("no host-visible coherent Vulkan memory type")?;
            let mai = VkMemoryAllocateInfo {
                s_type: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
                p_next: core::ptr::null(),
                allocation_size: req.size,
                memory_type_index: mem_idx,
            };
            let mut mem = 0u64;
            if unsafe { (self.alloc_memory)(self.device, &mai, core::ptr::null(), &mut mem) }
                != VK_SUCCESS
                || mem == 0
            {
                unsafe { (self.destroy_buffer)(self.device, buf, core::ptr::null()) };
                return Err("vkAllocateMemory failed".into());
            }
            if unsafe { (self.bind_buffer_memory)(self.device, buf, mem, 0) } != VK_SUCCESS {
                unsafe {
                    (self.free_memory)(self.device, mem, core::ptr::null());
                    (self.destroy_buffer)(self.device, buf, core::ptr::null());
                }
                return Err("vkBindBufferMemory failed".into());
            }
            if !bytes.is_empty() {
                let mut mapped: *mut c_void = core::ptr::null_mut();
                if unsafe { (self.map_memory)(self.device, mem, 0, VK_WHOLE_SIZE, 0, &mut mapped) }
                    != VK_SUCCESS
                    || mapped.is_null()
                {
                    unsafe {
                        (self.free_memory)(self.device, mem, core::ptr::null());
                        (self.destroy_buffer)(self.device, buf, core::ptr::null());
                    }
                    return Err("vkMapMemory upload failed".into());
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped as *mut u8, bytes.len());
                    (self.unmap_memory)(self.device, mem);
                }
            }
            Ok((buf, mem, size))
        }

        fn compile_or_get_pipeline(&mut self, key: &str, spirv: &[u32]) -> Result<u64, String> {
            if let Some(&(_, pipe)) = self.pipelines.get(key) {
                return Ok(pipe);
            }
            if spirv.is_empty() {
                return Err("SPIR-V binary cannot be empty".into());
            }
            let smci = VkShaderModuleCreateInfo {
                s_type: VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: 0,
                code_size: core::mem::size_of_val(spirv),
                p_code: spirv.as_ptr(),
            };
            let mut shader = 0u64;
            let s_rc = unsafe {
                (self.create_shader_module)(self.device, &smci, core::ptr::null(), &mut shader)
            };
            if s_rc != VK_SUCCESS || shader == 0 {
                return Err(format!("vkCreateShaderModule failed rc={s_rc}"));
            }
            let cpci = VkComputePipelineCreateInfo {
                s_type: VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: 0,
                stage: VkPipelineShaderStageCreateInfo {
                    s_type: VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                    p_next: core::ptr::null(),
                    flags: 0,
                    stage: VK_SHADER_STAGE_COMPUTE_BIT,
                    module: shader,
                    p_name: c"main".as_ptr(),
                    p_specialization_info: core::ptr::null(),
                },
                layout: self.pipeline_layout,
                base_pipeline_handle: 0,
                base_pipeline_index: -1,
            };
            let mut pipeline = 0u64;
            let p_rc = unsafe {
                (self.create_compute_pipelines)(
                    self.device,
                    0,
                    1,
                    &cpci,
                    core::ptr::null(),
                    &mut pipeline,
                )
            };
            if p_rc != VK_SUCCESS || pipeline == 0 {
                unsafe { (self.destroy_shader_module)(self.device, shader, core::ptr::null()) };
                return Err(format!("vkCreateComputePipelines failed rc={p_rc}"));
            }
            self.pipelines.insert(key.to_string(), (shader, pipeline));
            GPU_SHADERS_COMPILED.fetch_add(1, Ordering::Relaxed);
            Ok(pipeline)
        }

        pub(super) fn dispatch(
            &mut self,
            key: &str,
            spirv: &[u32],
            a: &[f32],
            b: &[f32],
            out_len: usize,
            push_words: &[u32],
            groups: (u32, u32, u32),
        ) -> Result<Vec<f32>, String> {
            let pipeline = self.compile_or_get_pipeline(key, spirv)?;
            let ab = unsafe {
                core::slice::from_raw_parts(a.as_ptr() as *const u8, core::mem::size_of_val(a))
            };
            let bb = unsafe {
                core::slice::from_raw_parts(b.as_ptr() as *const u8, core::mem::size_of_val(b))
            };
            let (buf_a, mem_a, sz_a) = self.create_host_buffer(ab, 4)?;
            let (buf_b, mem_b, sz_b) = match self.create_host_buffer(bb, 4) {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        (self.free_memory)(self.device, mem_a, core::ptr::null());
                        (self.destroy_buffer)(self.device, buf_a, core::ptr::null());
                    }
                    return Err(e);
                }
            };
            let out_bytes = (out_len * 4) as u64;
            let (buf_out, mem_out, sz_out) = match self.create_host_buffer(&[], out_bytes.max(4)) {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        (self.free_memory)(self.device, mem_b, core::ptr::null());
                        (self.destroy_buffer)(self.device, buf_b, core::ptr::null());
                        (self.free_memory)(self.device, mem_a, core::ptr::null());
                        (self.destroy_buffer)(self.device, buf_a, core::ptr::null());
                    }
                    return Err(e);
                }
            };

            let pool_sz = VkDescriptorPoolSize {
                ty: VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                descriptor_count: 3,
            };
            let dp_ci = VkDescriptorPoolCreateInfo {
                s_type: VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
                p_next: core::ptr::null(),
                flags: 0,
                max_sets: 1,
                pool_size_count: 1,
                p_pool_sizes: &pool_sz,
            };
            let mut desc_pool = 0u64;
            let mut res: Result<Vec<f32>, String> = Err("uninitialized".into());
            if unsafe {
                (self.create_desc_pool)(self.device, &dp_ci, core::ptr::null(), &mut desc_pool)
            } == VK_SUCCESS
                && desc_pool != 0
            {
                let ds_ai = VkDescriptorSetAllocateInfo {
                    s_type: VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
                    p_next: core::ptr::null(),
                    descriptor_pool: desc_pool,
                    descriptor_set_count: 1,
                    p_set_layouts: &self.dsl,
                };
                let mut desc_set = 0u64;
                if unsafe { (self.alloc_desc_sets)(self.device, &ds_ai, &mut desc_set) }
                    == VK_SUCCESS
                    && desc_set != 0
                {
                    let buf_infos = [
                        VkDescriptorBufferInfo {
                            buffer: buf_a,
                            offset: 0,
                            range: sz_a,
                        },
                        VkDescriptorBufferInfo {
                            buffer: buf_b,
                            offset: 0,
                            range: sz_b,
                        },
                        VkDescriptorBufferInfo {
                            buffer: buf_out,
                            offset: 0,
                            range: sz_out,
                        },
                    ];
                    let writes = [0u32, 1, 2].map(|idx| VkWriteDescriptorSet {
                        s_type: VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                        p_next: core::ptr::null(),
                        dst_set: desc_set,
                        dst_binding: idx,
                        dst_array_element: 0,
                        descriptor_count: 1,
                        descriptor_type: VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                        p_image_info: core::ptr::null(),
                        p_buffer_info: &buf_infos[idx as usize],
                        p_texel_buffer_view: core::ptr::null(),
                    });
                    unsafe {
                        (self.update_desc_sets)(
                            self.device,
                            3,
                            writes.as_ptr(),
                            0,
                            core::ptr::null(),
                        );
                    }

                    let cb_ai = VkCommandBufferAllocateInfo {
                        s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
                        p_next: core::ptr::null(),
                        command_pool: self.cmd_pool,
                        level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
                        command_buffer_count: 1,
                    };
                    let mut cmd: *mut c_void = core::ptr::null_mut();
                    if unsafe { (self.alloc_cmd_buffers)(self.device, &cb_ai, &mut cmd) }
                        == VK_SUCCESS
                        && !cmd.is_null()
                    {
                        let bi = VkCommandBufferBeginInfo {
                            s_type: VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                            p_next: core::ptr::null(),
                            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
                            p_inheritance_info: core::ptr::null(),
                        };
                        if unsafe { (self.begin_cmd_buffer)(cmd, &bi) } == VK_SUCCESS {
                            unsafe {
                                (self.cmd_bind_pipeline)(
                                    cmd,
                                    VK_PIPELINE_BIND_POINT_COMPUTE,
                                    pipeline,
                                );
                                (self.cmd_bind_desc_sets)(
                                    cmd,
                                    VK_PIPELINE_BIND_POINT_COMPUTE,
                                    self.pipeline_layout,
                                    0,
                                    1,
                                    &desc_set,
                                    0,
                                    core::ptr::null(),
                                );
                                if !push_words.is_empty() {
                                    let count = push_words.len().min(16);
                                    (self.cmd_push_constants)(
                                        cmd,
                                        self.pipeline_layout,
                                        VK_SHADER_STAGE_COMPUTE_BIT,
                                        0,
                                        (count * 4) as u32,
                                        push_words.as_ptr() as *const c_void,
                                    );
                                }
                                (self.cmd_dispatch)(
                                    cmd,
                                    groups.0.max(1),
                                    groups.1.max(1),
                                    groups.2.max(1),
                                );
                            }
                            if unsafe { (self.end_cmd_buffer)(cmd) } == VK_SUCCESS {
                                let fci = VkFenceCreateInfo {
                                    s_type: VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
                                    p_next: core::ptr::null(),
                                    flags: 0,
                                };
                                let mut fence = 0u64;
                                if unsafe {
                                    (self.create_fence)(
                                        self.device,
                                        &fci,
                                        core::ptr::null(),
                                        &mut fence,
                                    )
                                } == VK_SUCCESS
                                {
                                    let si = VkSubmitInfo {
                                        s_type: VK_STRUCTURE_TYPE_SUBMIT_INFO,
                                        p_next: core::ptr::null(),
                                        wait_semaphore_count: 0,
                                        p_wait_semaphores: core::ptr::null(),
                                        p_wait_dst_stage_mask: core::ptr::null(),
                                        command_buffer_count: 1,
                                        p_command_buffers: &cmd,
                                        signal_semaphore_count: 0,
                                        p_signal_semaphores: core::ptr::null(),
                                    };
                                    if unsafe { (self.queue_submit)(self.queue, 1, &si, fence) }
                                        == VK_SUCCESS
                                        && unsafe {
                                            (self.wait_for_fences)(
                                                self.device,
                                                1,
                                                &fence,
                                                1,
                                                u64::MAX,
                                            )
                                        } == VK_SUCCESS
                                    {
                                        if out_len == 0 {
                                            res = Ok(Vec::new());
                                        } else {
                                            let mut mapped: *mut c_void = core::ptr::null_mut();
                                            if unsafe {
                                                (self.map_memory)(
                                                    self.device,
                                                    mem_out,
                                                    0,
                                                    VK_WHOLE_SIZE,
                                                    0,
                                                    &mut mapped,
                                                )
                                            } == VK_SUCCESS
                                                && !mapped.is_null()
                                            {
                                                let slice = unsafe {
                                                    core::slice::from_raw_parts(
                                                        mapped as *const f32,
                                                        out_len,
                                                    )
                                                };
                                                res = Ok(slice.to_vec());
                                                unsafe {
                                                    (self.unmap_memory)(self.device, mem_out)
                                                };
                                            } else {
                                                res = Err("vkMapMemory readback failed".into());
                                            }
                                        }
                                    } else {
                                        res = Err("vkQueueSubmit/vkWaitForFences failed".into());
                                    }
                                    unsafe {
                                        (self.destroy_fence)(self.device, fence, core::ptr::null());
                                    }
                                }
                            }
                        }
                        unsafe {
                            (self.free_cmd_buffers)(self.device, self.cmd_pool, 1, &cmd);
                        }
                    }
                }
                unsafe {
                    (self.destroy_desc_pool)(self.device, desc_pool, core::ptr::null());
                }
            }

            unsafe {
                (self.free_memory)(self.device, mem_out, core::ptr::null());
                (self.destroy_buffer)(self.device, buf_out, core::ptr::null());
                (self.free_memory)(self.device, mem_b, core::ptr::null());
                (self.destroy_buffer)(self.device, buf_b, core::ptr::null());
                (self.free_memory)(self.device, mem_a, core::ptr::null());
                (self.destroy_buffer)(self.device, buf_a, core::ptr::null());
            }

            if res.is_ok() {
                GPU_DISPATCHES.fetch_add(1, Ordering::Relaxed);
            }
            res
        }
    }

    static VK_CTX: OnceLock<Mutex<Result<VulkanState, String>>> = OnceLock::new();

    pub(super) fn with_vulkan<T, F: FnOnce(&mut VulkanState) -> Result<T, String>>(
        f: F,
    ) -> Result<T, String> {
        let cell = VK_CTX.get_or_init(|| Mutex::new(VulkanState::init()));
        let mut guard = cell
            .lock()
            .map_err(|_| "vulkan context poisoned".to_string())?;
        match &mut *guard {
            Ok(state) => f(state),
            Err(e) => Err(e.clone()),
        }
    }
}

const HLSL_MATMUL: &str = r#"
cbuffer Dims : register(b0) { uint M; uint K; uint N; uint Pad; };
StructuredBuffer<float> A : register(t0);
StructuredBuffer<float> B : register(t1);
RWStructuredBuffer<float> C : register(u0);
[numthreads(16, 16, 1)]
void CSMain(uint3 gid : SV_DispatchThreadID) {
    uint r = gid.y;
    uint c = gid.x;
    if (r >= M || c >= N) return;
    float acc = 0.0f;
    for (uint k = 0; k < K; k++) {
        acc += A[r * K + k] * B[k * N + c];
    }
    C[r * N + c] = acc;
}
"#;

const HLSL_MATMUL_F64: &str = r#"
cbuffer Dims : register(b0) { uint M; uint K; uint N; uint Pad; };
StructuredBuffer<double> A : register(t0);
StructuredBuffer<double> B : register(t1);
RWStructuredBuffer<double> C : register(u0);
[numthreads(16, 16, 1)]
void CSMain(uint3 gid : SV_DispatchThreadID) {
    uint r = gid.y;
    uint c = gid.x;
    if (r >= M || c >= N) return;
    double acc = 0.0;
    for (uint k = 0; k < K; k++) {
        acc += A[r * K + k] * B[k * N + c];
    }
    C[r * N + c] = acc;
}
"#;

const HLSL_EWISE: &str = r#"
cbuffer Params : register(b0) { uint Len; uint OpCode; float Scalar; uint Pad; };
StructuredBuffer<float> A : register(t0);
StructuredBuffer<float> B : register(t1);
RWStructuredBuffer<float> Out : register(u0);
[numthreads(256, 1, 1)]
void CSMain(uint3 gid : SV_DispatchThreadID) {
    uint i = gid.x;
    if (i >= Len) return;
    float a = A[i];
    if (OpCode == 0) { Out[i] = a + B[i]; }
    else if (OpCode == 1) { Out[i] = a - B[i]; }
    else if (OpCode == 2) { Out[i] = a * B[i]; }
    else if (OpCode == 3) { Out[i] = a > 0.0f ? a : 0.0f; }
    else if (OpCode == 4) { Out[i] = 1.0f / (1.0f + exp(-a)); }
    else if (OpCode == 5) { Out[i] = a * Scalar; }
    else if (OpCode == 6) {
        float y = 0.79788456f * (a + 0.044715f * a * a * a);
        float y2 = 2.0f * y;
        if (y2 > 18.0f) { Out[i] = a; }
        else { Out[i] = 0.5f * a * (2.0f - 2.0f / (exp(max(y2, -18.0f)) + 1.0f)); }
    }
    else { Out[i] = a; }
}
"#;

const HLSL_EWISE_F64: &str = r#"
cbuffer Params : register(b0) { uint Len; uint OpCode; uint ScalarLo; uint ScalarHi; };
StructuredBuffer<double> A : register(t0);
StructuredBuffer<double> B : register(t1);
RWStructuredBuffer<double> Out : register(u0);
[numthreads(256, 1, 1)]
void CSMain(uint3 gid : SV_DispatchThreadID) {
    uint i = gid.x;
    if (i >= Len) return;
    double a = A[i];
    if (OpCode == 0) { Out[i] = a + B[i]; }
    else if (OpCode == 1) { Out[i] = a - B[i]; }
    else if (OpCode == 2) { Out[i] = a * B[i]; }
    else if (OpCode == 3) { Out[i] = a > 0.0 ? a : 0.0; }
    else if (OpCode == 5) { Out[i] = a * asdouble(ScalarLo, ScalarHi); }
    else { Out[i] = a; }
}
"#;

const HLSL_SOFTMAX: &str = r#"
cbuffer Dims : register(b0) { uint Rows; uint Cols; uint Pad0; uint Pad1; };
StructuredBuffer<float> In : register(t0);
StructuredBuffer<float> Unused : register(t1);
RWStructuredBuffer<float> Out : register(u0);
[numthreads(64, 1, 1)]
void CSMain(uint3 gid : SV_DispatchThreadID) {
    uint r = gid.x;
    if (r >= Rows || Cols == 0) return;
    uint base_idx = r * Cols;
    float mx = In[base_idx];
    for (uint c = 1; c < Cols; c++) {
        mx = max(mx, In[base_idx + c]);
    }
    float sum = 0.0f;
    for (uint c = 0; c < Cols; c++) {
        float e = exp(In[base_idx + c] - mx);
        Out[base_idx + c] = e;
        sum += e;
    }
    for (uint c = 0; c < Cols; c++) {
        Out[base_idx + c] = Out[base_idx + c] / sum;
    }
}
"#;

const HLSL_LAYERNORM: &str = r#"
cbuffer Dims : register(b0) { uint Rows; uint Cols; float Eps; uint Pad; };
StructuredBuffer<float> In : register(t0);
StructuredBuffer<float> Unused : register(t1);
RWStructuredBuffer<float> Out : register(u0);
[numthreads(64, 1, 1)]
void CSMain(uint3 gid : SV_DispatchThreadID) {
    uint r = gid.x;
    if (r >= Rows || Cols == 0) return;
    uint base_idx = r * Cols;
    float sum = 0.0f;
    for (uint c = 0; c < Cols; c++) {
        sum += In[base_idx + c];
    }
    float mean = sum / (float)Cols;
    float sq = 0.0f;
    for (uint c = 0; c < Cols; c++) {
        float d = In[base_idx + c] - mean;
        sq += d * d;
    }
    float inv_std = rsqrt(sq / (float)Cols + Eps);
    for (uint c = 0; c < Cols; c++) {
        Out[base_idx + c] = (In[base_idx + c] - mean) * inv_std;
    }
}
"#;

const HLSL_CONV2D: &str = r#"
cbuffer ConvDims : register(b0) {
    uint N; uint C; uint H; uint W;
    uint O; uint KH; uint KW; uint Stride;
    uint OH; uint OW; uint Pad0; uint Pad1;
};
StructuredBuffer<float> In : register(t0);
StructuredBuffer<float> Wt : register(t1);
RWStructuredBuffer<float> Out : register(u0);
[numthreads(64, 1, 1)]
void CSMain(uint3 gid : SV_DispatchThreadID) {
    uint total = N * O * OH * OW;
    uint idx = gid.x;
    if (idx >= total) return;
    uint ox = idx % OW;
    uint rem = idx / OW;
    uint oy = rem % OH;
    rem = rem / OH;
    uint oc = rem % O;
    uint b = rem / O;
    float acc = 0.0f;
    for (uint ic = 0; ic < C; ic++) {
        for (uint ky = 0; ky < KH; ky++) {
            uint iy = oy * Stride + ky;
            for (uint kx = 0; kx < KW; kx++) {
                uint ix = ox * Stride + kx;
                uint in_i = ((b * C + ic) * H + iy) * W + ix;
                uint w_i = ((oc * C + ic) * KH + ky) * KW + kx;
                acc += In[in_i] * Wt[w_i];
            }
        }
    }
    Out[idx] = acc;
}
"#;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolvedBackend {
    D3D11,
    Vulkan,
    Cpu,
}

fn resolve_backend() -> ResolvedBackend {
    match current_backend_pref() {
        BackendPref::Cpu => ResolvedBackend::Cpu,
        BackendPref::Vulkan => {
            if vulkan_backend::with_vulkan(|_| Ok(())).is_ok() {
                ResolvedBackend::Vulkan
            } else {
                ResolvedBackend::Cpu
            }
        }
        BackendPref::D3D11 => {
            #[cfg(windows)]
            if d3d11_backend::with_d3d11(|_| Ok(())).is_ok() {
                return ResolvedBackend::D3D11;
            }
            ResolvedBackend::Cpu
        }
        BackendPref::Auto => {
            #[cfg(windows)]
            if d3d11_backend::with_d3d11(|st| Ok(st.is_hardware)).unwrap_or(false) {
                return ResolvedBackend::D3D11;
            }
            if vulkan_backend::with_vulkan(|_| Ok(())).is_ok() {
                return ResolvedBackend::Vulkan;
            }
            #[cfg(windows)]
            if d3d11_backend::with_d3d11(|_| Ok(())).is_ok() {
                return ResolvedBackend::D3D11;
            }
            ResolvedBackend::Cpu
        }
    }
}

pub(crate) fn gpu_backends() -> Vec<String> {
    let mut out = Vec::new();
    #[cfg(windows)]
    if d3d11_backend::with_d3d11(|_| Ok(())).is_ok() {
        out.push("d3d11".to_string());
    }
    if vulkan_backend::with_vulkan(|_| Ok(())).is_ok() {
        out.push("vulkan".to_string());
    }
    out.push("cpu".to_string());
    out
}

pub(crate) fn gpu_set_backend(name: &str) -> Result<String, String> {
    let norm = name.trim().to_ascii_lowercase();
    let pref = match norm.as_str() {
        "auto" => BackendPref::Auto,
        "vulkan" => BackendPref::Vulkan,
        "d3d11" => BackendPref::D3D11,
        "cpu" => BackendPref::Cpu,
        _ => {
            return Err(format!(
                "unknown gpu backend '{name}' (expected 'auto', 'vulkan', 'd3d11', or 'cpu')"
            ));
        }
    };
    BACKEND_PREF.store(pref as u8, Ordering::Relaxed);
    Ok(norm)
}

pub(crate) fn gpu_precision() -> &'static str {
    if current_precision_pref() == PrecisionPref::F64 {
        "f64"
    } else {
        "f32"
    }
}

pub(crate) fn gpu_set_precision(name: &str) -> Result<String, String> {
    let norm = name.trim().to_ascii_lowercase();
    match norm.as_str() {
        "f32" => PRECISION_PREF.store(PrecisionPref::F32 as u8, Ordering::Relaxed),
        "f64" => PRECISION_PREF.store(PrecisionPref::F64 as u8, Ordering::Relaxed),
        _ => {
            return Err(format!(
                "unknown gpu precision '{name}' (expected 'f32' or 'f64')"
            ));
        }
    }
    Ok(norm)
}

fn adapters_to_val(adapters: &[AdapterSummary]) -> Vec<Val> {
    adapters
        .iter()
        .map(|a| {
            Val::Map(vec![
                (Val::Str("index".into()), Val::Num(a.index as f64)),
                (Val::Str("name".into()), Val::Str(a.name.clone())),
                (Val::Str("vendor_id".into()), Val::Num(a.vendor_id as f64)),
                (Val::Str("device_id".into()), Val::Num(a.device_id as f64)),
                (
                    Val::Str("dedicated_vram_mb".into()),
                    Val::Num(a.dedicated_vram_mb),
                ),
                (Val::Str("shared_mem_mb".into()), Val::Num(a.shared_mem_mb)),
                (Val::Str("is_software".into()), Val::Bool(a.is_software)),
            ])
        })
        .collect()
}

pub(crate) fn gpu_available() -> bool {
    #[cfg(windows)]
    if d3d11_backend::with_d3d11(|_| Ok(true)).unwrap_or(false) {
        return true;
    }
    vulkan_backend::with_vulkan(|_| Ok(true)).unwrap_or(false)
}

pub(crate) fn gpu_devices_val() -> Val {
    let active = resolve_backend();
    let mut devices = Vec::new();

    if let Ok(vk_list) = vulkan_backend::with_vulkan(|st| {
        Ok(st
            .adapters
            .iter()
            .map(|a| {
                let is_active =
                    matches!(active, ResolvedBackend::Vulkan) && a.index == st.active_adapter.index;
                Val::Map(vec![
                    (Val::Str("backend".into()), Val::Str("vulkan".into())),
                    (Val::Str("index".into()), Val::Num(a.index as f64)),
                    (Val::Str("name".into()), Val::Str(a.name.clone())),
                    (Val::Str("vendor_id".into()), Val::Num(a.vendor_id as f64)),
                    (Val::Str("device_id".into()), Val::Num(a.device_id as f64)),
                    (
                        Val::Str("dedicated_vram_mb".into()),
                        Val::Num(a.dedicated_vram_mb),
                    ),
                    (Val::Str("shared_mem_mb".into()), Val::Num(a.shared_mem_mb)),
                    (Val::Str("is_hardware".into()), Val::Bool(!a.is_software)),
                    (Val::Str("is_software".into()), Val::Bool(a.is_software)),
                    (Val::Str("supports_f64".into()), Val::Bool(false)),
                    (Val::Str("active".into()), Val::Bool(is_active)),
                ])
            })
            .collect::<Vec<_>>())
    }) {
        devices.extend(vk_list);
    }

    #[cfg(windows)]
    if let Ok(d3d_list) = d3d11_backend::with_d3d11(|st| {
        Ok(st
            .adapters
            .iter()
            .map(|a| {
                let is_active =
                    matches!(active, ResolvedBackend::D3D11) && a.index == st.active_adapter.index;
                Val::Map(vec![
                    (Val::Str("backend".into()), Val::Str("d3d11".into())),
                    (Val::Str("index".into()), Val::Num(a.index as f64)),
                    (Val::Str("name".into()), Val::Str(a.name.clone())),
                    (Val::Str("vendor_id".into()), Val::Num(a.vendor_id as f64)),
                    (Val::Str("device_id".into()), Val::Num(a.device_id as f64)),
                    (
                        Val::Str("dedicated_vram_mb".into()),
                        Val::Num(a.dedicated_vram_mb),
                    ),
                    (Val::Str("shared_mem_mb".into()), Val::Num(a.shared_mem_mb)),
                    (Val::Str("is_hardware".into()), Val::Bool(!a.is_software)),
                    (Val::Str("is_software".into()), Val::Bool(a.is_software)),
                    (Val::Str("supports_f64".into()), Val::Bool(st.supports_f64)),
                    (Val::Str("active".into()), Val::Bool(is_active)),
                ])
            })
            .collect::<Vec<_>>())
    }) {
        devices.extend(d3d_list);
    }

    devices.push(Val::Map(vec![
        (Val::Str("backend".into()), Val::Str("cpu-fallback".into())),
        (Val::Str("index".into()), Val::Num(0.0)),
        (Val::Str("name".into()), Val::Str("CPU Thread Pool".into())),
        (Val::Str("vendor_id".into()), Val::Num(0.0)),
        (Val::Str("device_id".into()), Val::Num(0.0)),
        (Val::Str("dedicated_vram_mb".into()), Val::Num(0.0)),
        (Val::Str("shared_mem_mb".into()), Val::Num(0.0)),
        (Val::Str("is_hardware".into()), Val::Bool(false)),
        (Val::Str("is_software".into()), Val::Bool(true)),
        (Val::Str("supports_f64".into()), Val::Bool(true)),
        (
            Val::Str("active".into()),
            Val::Bool(matches!(active, ResolvedBackend::Cpu)),
        ),
    ]));
    Val::List(devices)
}

pub(crate) fn gpu_info_val() -> Val {
    let avail_backends = gpu_backends().into_iter().map(Val::Str).collect::<Vec<_>>();
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    match resolve_backend() {
        ResolvedBackend::Vulkan => {
            if let Ok(val) = vulkan_backend::with_vulkan(|st| {
                let backend = if st.is_hardware {
                    "vulkan-compute"
                } else {
                    "vulkan-cpu"
                };
                Ok(Val::Map(vec![
                    (Val::Str("available".into()), Val::Bool(true)),
                    (Val::Str("backend".into()), Val::Str(backend.into())),
                    (
                        Val::Str("dtype".into()),
                        Val::Str(if want_f64 { "f64" } else { "f32" }.into()),
                    ),
                    (Val::Str("supports_f64".into()), Val::Bool(false)),
                    (
                        Val::Str("adapter_name".into()),
                        Val::Str(st.active_adapter.name.clone()),
                    ),
                    (
                        Val::Str("vendor_id".into()),
                        Val::Num(st.active_adapter.vendor_id as f64),
                    ),
                    (
                        Val::Str("device_id".into()),
                        Val::Num(st.active_adapter.device_id as f64),
                    ),
                    (
                        Val::Str("dedicated_vram_mb".into()),
                        Val::Num(st.active_adapter.dedicated_vram_mb),
                    ),
                    (
                        Val::Str("shared_mem_mb".into()),
                        Val::Num(st.active_adapter.shared_mem_mb),
                    ),
                    (
                        Val::Str("api_version".into()),
                        Val::Str(format!(
                            "{}.{}.{}",
                            st.api_version >> 22,
                            (st.api_version >> 12) & 0x3FF,
                            st.api_version & 0xFFF
                        )),
                    ),
                    (Val::Str("is_hardware".into()), Val::Bool(st.is_hardware)),
                    (
                        Val::Str("adapters".into()),
                        Val::List(adapters_to_val(&st.adapters)),
                    ),
                    (
                        Val::Str("backends".into()),
                        Val::List(avail_backends.clone()),
                    ),
                    (
                        Val::Str("dispatches".into()),
                        Val::Num(GPU_DISPATCHES.load(Ordering::Relaxed) as f64),
                    ),
                    (
                        Val::Str("shaders_compiled".into()),
                        Val::Num(GPU_SHADERS_COMPILED.load(Ordering::Relaxed) as f64),
                    ),
                    (
                        Val::Str("fused_kernels".into()),
                        Val::Num(GPU_FUSED_KERNELS.load(Ordering::Relaxed) as f64),
                    ),
                ]))
            }) {
                return val;
            }
        }
        ResolvedBackend::D3D11 => {
            #[cfg(windows)]
            if let Ok(val) = d3d11_backend::with_d3d11(|st| {
                let dtype = if want_f64 && st.supports_f64 {
                    "f64"
                } else {
                    "f32"
                };
                Ok(Val::Map(vec![
                    (Val::Str("available".into()), Val::Bool(true)),
                    (Val::Str("backend".into()), Val::Str(st.backend.clone())),
                    (Val::Str("dtype".into()), Val::Str(dtype.into())),
                    (Val::Str("supports_f64".into()), Val::Bool(st.supports_f64)),
                    (
                        Val::Str("adapter_name".into()),
                        Val::Str(st.active_adapter.name.clone()),
                    ),
                    (
                        Val::Str("vendor_id".into()),
                        Val::Num(st.active_adapter.vendor_id as f64),
                    ),
                    (
                        Val::Str("device_id".into()),
                        Val::Num(st.active_adapter.device_id as f64),
                    ),
                    (
                        Val::Str("dedicated_vram_mb".into()),
                        Val::Num(st.active_adapter.dedicated_vram_mb),
                    ),
                    (
                        Val::Str("shared_mem_mb".into()),
                        Val::Num(st.active_adapter.shared_mem_mb),
                    ),
                    (
                        Val::Str("feature_level".into()),
                        Val::Str(format!("0x{:04X}", st.feature_level)),
                    ),
                    (Val::Str("is_hardware".into()), Val::Bool(st.is_hardware)),
                    (
                        Val::Str("adapters".into()),
                        Val::List(adapters_to_val(&st.adapters)),
                    ),
                    (
                        Val::Str("backends".into()),
                        Val::List(avail_backends.clone()),
                    ),
                    (
                        Val::Str("dispatches".into()),
                        Val::Num(GPU_DISPATCHES.load(Ordering::Relaxed) as f64),
                    ),
                    (
                        Val::Str("shaders_compiled".into()),
                        Val::Num(GPU_SHADERS_COMPILED.load(Ordering::Relaxed) as f64),
                    ),
                    (
                        Val::Str("fused_kernels".into()),
                        Val::Num(GPU_FUSED_KERNELS.load(Ordering::Relaxed) as f64),
                    ),
                ]))
            }) {
                return val;
            }
        }
        ResolvedBackend::Cpu => {}
    }
    Val::Map(vec![
        (Val::Str("available".into()), Val::Bool(false)),
        (Val::Str("backend".into()), Val::Str("cpu-fallback".into())),
        (Val::Str("dtype".into()), Val::Str("f64".into())),
        (Val::Str("supports_f64".into()), Val::Bool(true)),
        (
            Val::Str("adapter_name".into()),
            Val::Str("CPU Thread Pool".into()),
        ),
        (Val::Str("is_hardware".into()), Val::Bool(false)),
        (Val::Str("adapters".into()), Val::List(Vec::new())),
        (Val::Str("backends".into()), Val::List(avail_backends)),
        (
            Val::Str("dispatches".into()),
            Val::Num(GPU_DISPATCHES.load(Ordering::Relaxed) as f64),
        ),
        (
            Val::Str("shaders_compiled".into()),
            Val::Num(GPU_SHADERS_COMPILED.load(Ordering::Relaxed) as f64),
        ),
        (
            Val::Str("fused_kernels".into()),
            Val::Num(GPU_FUSED_KERNELS.load(Ordering::Relaxed) as f64),
        ),
    ])
}

pub(crate) fn gpu_matmul_f64(a: &[f64], b: &[f64], m: usize, k: usize, n: usize) -> Vec<f64> {
    if m == 0 || k == 0 || n == 0 {
        return vec![0.0; m * n];
    }
    let gx = (n as u32).div_ceil(16);
    let gy = (m as u32).div_ceil(16);
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    match resolve_backend() {
        ResolvedBackend::D3D11 => {
            #[cfg(windows)]
            {
                if want_f64 {
                    if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                        st.dispatch_f64(
                            "veyra:matmul:f64:v1",
                            HLSL_MATMUL_F64,
                            "CSMain",
                            a,
                            b,
                            m * n,
                            &[m as u32, k as u32, n as u32, 0],
                            (gx, gy, 1),
                        )
                    }) {
                        return out;
                    }
                } else {
                    let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
                    let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
                    if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                        st.dispatch(
                            "veyra:matmul:v1",
                            HLSL_MATMUL,
                            "CSMain",
                            &af,
                            &bf,
                            m * n,
                            &[m as u32, k as u32, n as u32, 0],
                            (gx, gy, 1),
                        )
                    }) {
                        return out.into_iter().map(|x| x as f64).collect();
                    }
                }
            }
        }
        ResolvedBackend::Vulkan if !want_f64 => {
            let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
            let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
            let spv = crate::spirv::spirv_matmul();
            if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                st.dispatch(
                    "vk:matmul:v1",
                    &spv,
                    &af,
                    &bf,
                    m * n,
                    &[m as u32, k as u32, n as u32, 0],
                    (gx, gy, 1),
                )
            }) {
                return out.into_iter().map(|x| x as f64).collect();
            }
        }
        _ => {}
    }
    cpu_matmul_f64(a, b, m, k, n)
}

fn cpu_matmul_f64(a: &[f64], b: &[f64], m: usize, k: usize, n: usize) -> Vec<f64> {
    let mut out = vec![0.0; m * n];
    let ops = m.saturating_mul(k).saturating_mul(n);
    if ops >= 32_768 && m >= 8 {
        let threads = thread::available_parallelism()
            .map(|t| t.get())
            .unwrap_or(4)
            .min(m);
        let rows_per_chunk = m.div_ceil(threads);
        thread::scope(|s| {
            for (chunk_idx, out_chunk) in out.chunks_mut(rows_per_chunk * n).enumerate() {
                let row_start = chunk_idx * rows_per_chunk;
                let chunk_rows = out_chunk.len() / n;
                s.spawn(move || {
                    for local_i in 0..chunk_rows {
                        let i = row_start + local_i;
                        let out_row = &mut out_chunk[local_i * n..(local_i + 1) * n];
                        for p in 0..k {
                            let a_ip = a[i * k + p];
                            let r_row = &b[p * n..(p + 1) * n];
                            for j in 0..n {
                                out_row[j] += a_ip * r_row[j];
                            }
                        }
                    }
                });
            }
        });
    } else {
        for i in 0..m {
            for p in 0..k {
                let a_ip = a[i * k + p];
                let out_row = &mut out[i * n..(i + 1) * n];
                let r_row = &b[p * n..(p + 1) * n];
                for j in 0..n {
                    out_row[j] += a_ip * r_row[j];
                }
            }
        }
    }
    out
}

pub(crate) fn gpu_ewise_f64(a: &[f64], b: &[f64], op_code: u32, scalar: f64) -> Vec<f64> {
    if a.is_empty() {
        return Vec::new();
    }
    let groups = (a.len() as u32).div_ceil(256);
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    match resolve_backend() {
        ResolvedBackend::D3D11 => {
            #[cfg(windows)]
            {
                if want_f64 && matches!(op_code, 0 | 1 | 2 | 3 | 5) {
                    let s_bits = scalar.to_bits();
                    let cb = [
                        a.len() as u32,
                        op_code,
                        (s_bits & 0xFFFF_FFFF) as u32,
                        (s_bits >> 32) as u32,
                    ];
                    if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                        st.dispatch_f64(
                            "veyra:ewise:f64:v1",
                            HLSL_EWISE_F64,
                            "CSMain",
                            a,
                            b,
                            a.len(),
                            &cb,
                            (groups, 1, 1),
                        )
                    }) {
                        return out;
                    }
                } else {
                    let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
                    let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
                    let cb = [a.len() as u32, op_code, (scalar as f32).to_bits(), 0];
                    if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                        st.dispatch(
                            "veyra:ewise:v2",
                            HLSL_EWISE,
                            "CSMain",
                            &af,
                            &bf,
                            a.len(),
                            &cb,
                            (groups, 1, 1),
                        )
                    }) {
                        return out.into_iter().map(|x| x as f64).collect();
                    }
                }
            }
        }
        ResolvedBackend::Vulkan if !want_f64 => {
            let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
            let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
            let spv = crate::spirv::spirv_ewise();
            let pc = [a.len() as u32, op_code, (scalar as f32).to_bits(), 0];
            if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                st.dispatch("vk:ewise:v1", &spv, &af, &bf, a.len(), &pc, (groups, 1, 1))
            }) {
                return out.into_iter().map(|x| x as f64).collect();
            }
        }
        _ => {}
    }
    a.iter()
        .enumerate()
        .map(|(i, &x)| match op_code {
            0 => x + b.get(i).copied().unwrap_or(0.0),
            1 => x - b.get(i).copied().unwrap_or(0.0),
            2 => x * b.get(i).copied().unwrap_or(1.0),
            3 => x.max(0.0),
            4 => 1.0 / (1.0 + (-x).exp()),
            5 => x * scalar,
            6 => {
                let y = 0.797_884_560_802_865_4 * (x + 0.044_715 * x * x * x);
                0.5 * x * (1.0 + y.tanh())
            }
            _ => x,
        })
        .collect()
}

pub(crate) fn gpu_softmax_rows_f64(data: &[f64], rows: usize, cols: usize) -> Vec<f64> {
    if rows == 0 || cols == 0 {
        return Vec::new();
    }
    let groups = (rows as u32).div_ceil(64);
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    match resolve_backend() {
        ResolvedBackend::D3D11 if !want_f64 => {
            #[cfg(windows)]
            {
                let af: Vec<f32> = data.iter().map(|&x| x as f32).collect();
                if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                    st.dispatch(
                        "veyra:softmax:v1",
                        HLSL_SOFTMAX,
                        "CSMain",
                        &af,
                        &[],
                        rows * cols,
                        &[rows as u32, cols as u32, 0, 0],
                        (groups, 1, 1),
                    )
                }) {
                    return out.into_iter().map(|x| x as f64).collect();
                }
            }
        }
        ResolvedBackend::Vulkan if !want_f64 => {
            let af: Vec<f32> = data.iter().map(|&x| x as f32).collect();
            let spv = crate::spirv::spirv_softmax();
            if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                st.dispatch(
                    "vk:softmax:v1",
                    &spv,
                    &af,
                    &[],
                    rows * cols,
                    &[rows as u32, cols as u32, 0, 0],
                    (groups, 1, 1),
                )
            }) {
                return out.into_iter().map(|x| x as f64).collect();
            }
        }
        _ => {}
    }
    crate::tensor::softmax_rows(data, rows, cols)
}

pub(crate) fn gpu_layernorm_f64(data: &[f64], rows: usize, cols: usize, eps: f64) -> Vec<f64> {
    if rows == 0 || cols == 0 {
        return Vec::new();
    }
    let groups = (rows as u32).div_ceil(64);
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    let pc = [rows as u32, cols as u32, (eps as f32).to_bits(), 0];
    match resolve_backend() {
        ResolvedBackend::D3D11 if !want_f64 => {
            #[cfg(windows)]
            {
                let af: Vec<f32> = data.iter().map(|&x| x as f32).collect();
                if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                    st.dispatch(
                        "veyra:layernorm:v1",
                        HLSL_LAYERNORM,
                        "CSMain",
                        &af,
                        &[],
                        rows * cols,
                        &pc,
                        (groups, 1, 1),
                    )
                }) {
                    return out.into_iter().map(|x| x as f64).collect();
                }
            }
        }
        ResolvedBackend::Vulkan if !want_f64 => {
            let af: Vec<f32> = data.iter().map(|&x| x as f32).collect();
            let spv = crate::spirv::spirv_layernorm();
            if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                st.dispatch(
                    "vk:layernorm:v1",
                    &spv,
                    &af,
                    &[],
                    rows * cols,
                    &pc,
                    (groups, 1, 1),
                )
            }) {
                return out.into_iter().map(|x| x as f64).collect();
            }
        }
        _ => {}
    }
    let mut out = vec![0.0; rows * cols];
    for r in 0..rows {
        let base = r * cols;
        let row = &data[base..base + cols];
        let mean = row.iter().sum::<f64>() / cols as f64;
        let var = row.iter().map(|&x| (x - mean) * (x - mean)).sum::<f64>() / cols as f64;
        let inv_std = 1.0 / (var + eps).sqrt();
        for c in 0..cols {
            out[base + c] = (row[c] - mean) * inv_std;
        }
    }
    out
}

pub(crate) fn gpu_conv2d_f64(input: &[f64], weight: &[f64], d: &Conv2dDims) -> Vec<f64> {
    let total = d.n * d.o * d.oh * d.ow;
    if total == 0 {
        return Vec::new();
    }
    let groups = (total as u32).div_ceil(64);
    let cb = [
        d.n as u32,
        d.c as u32,
        d.h as u32,
        d.w as u32,
        d.o as u32,
        d.kh as u32,
        d.kw as u32,
        d.stride as u32,
        d.oh as u32,
        d.ow as u32,
        0,
        0,
    ];
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    match resolve_backend() {
        ResolvedBackend::D3D11 if !want_f64 => {
            #[cfg(windows)]
            {
                let af: Vec<f32> = input.iter().map(|&x| x as f32).collect();
                let bf: Vec<f32> = weight.iter().map(|&x| x as f32).collect();
                if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                    st.dispatch(
                        "veyra:conv2d:v1",
                        HLSL_CONV2D,
                        "CSMain",
                        &af,
                        &bf,
                        total,
                        &cb,
                        (groups, 1, 1),
                    )
                }) {
                    return out.into_iter().map(|x| x as f64).collect();
                }
            }
        }
        ResolvedBackend::Vulkan if !want_f64 => {
            let af: Vec<f32> = input.iter().map(|&x| x as f32).collect();
            let bf: Vec<f32> = weight.iter().map(|&x| x as f32).collect();
            let spv = crate::spirv::spirv_conv2d();
            if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                st.dispatch("vk:conv2d:v1", &spv, &af, &bf, total, &cb, (groups, 1, 1))
            }) {
                return out.into_iter().map(|x| x as f64).collect();
            }
        }
        _ => {}
    }
    crate::tensor::conv2d_forward(input, weight, d)
}

pub(crate) fn gpu_run_hlsl(
    source: &str,
    entry: &str,
    a: &[f64],
    b: &[f64],
    out_len: usize,
    gx: u32,
    gy: u32,
    gz: u32,
) -> Result<Vec<f64>, String> {
    #[cfg(windows)]
    {
        let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
        let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
        let key = format!("custom:{entry}:{}", source.len());
        let out = d3d11_backend::with_d3d11(|st| {
            st.dispatch(
                &key,
                source,
                entry,
                &af,
                &bf,
                out_len,
                &[a.len() as u32, b.len() as u32, out_len as u32, 0],
                (gx, gy, gz),
            )
        })?;
        Ok(out.into_iter().map(|x| x as f64).collect())
    }
    #[cfg(not(windows))]
    {
        let _ = (source, entry, a, b, out_len, gx, gy, gz);
        Err("HLSL cs_5_0 compute shaders require Windows Direct3D 11 (use gpu_run_spirv on Linux/macOS)".into())
    }
}

pub(crate) fn gpu_run_spirv(
    words: &[u32],
    a: &[f64],
    b: &[f64],
    out_len: usize,
    push_words: &[u32],
    gx: u32,
    gy: u32,
    gz: u32,
) -> Result<Vec<f64>, String> {
    let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
    let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &w in words.iter().take(64) {
        h ^= w as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    let key = format!("custom:spv:{}:{:016x}", words.len(), h);
    let out = vulkan_backend::with_vulkan(|st| {
        st.dispatch(&key, words, &af, &bf, out_len, push_words, (gx, gy, gz))
    })?;
    Ok(out.into_iter().map(|x| x as f64).collect())
}

pub(crate) fn hlsl_fused_ewise(ops: &[crate::spirv::FusedEwiseOp]) -> String {
    use crate::spirv::FusedEwiseOp;
    let mut s = String::from(
        "cbuffer Params : register(b0) { uint len; uint _p1; uint _p2; uint _p3; };\n\
         StructuredBuffer<float> A : register(t0);\n\
         StructuredBuffer<float> B : register(t1);\n\
         RWStructuredBuffer<float> Out : register(u0);\n\
         [numthreads(256, 1, 1)]\n\
         void CSMain(uint3 dtid : SV_DispatchThreadID) {\n\
           uint i = dtid.x;\n\
           if (i >= len) return;\n\
           float x = A[i];\n",
    );
    let needs_b = ops.iter().any(|op| {
        matches!(
            op,
            FusedEwiseOp::Add | FusedEwiseOp::Sub | FusedEwiseOp::Mul | FusedEwiseOp::Div
        )
    });
    if needs_b {
        s.push_str("  float b = B[i];\n");
    }
    for &op in ops {
        match op {
            FusedEwiseOp::Add => s.push_str("  x = x + b;\n"),
            FusedEwiseOp::Sub => s.push_str("  x = x - b;\n"),
            FusedEwiseOp::Mul => s.push_str("  x = x * b;\n"),
            FusedEwiseOp::Div => s.push_str("  x = x / b;\n"),
            FusedEwiseOp::AddScalar(v) => s.push_str(&format!("  x = x + {:.9};\n", v)),
            FusedEwiseOp::Scale(v) => s.push_str(&format!("  x = x * {:.9};\n", v)),
            FusedEwiseOp::Abs => s.push_str("  x = abs(x);\n"),
            FusedEwiseOp::Neg => s.push_str("  x = -x;\n"),
            FusedEwiseOp::Recip => s.push_str("  x = 1.0 / x;\n"),
            FusedEwiseOp::Sqrt => s.push_str("  x = sqrt(max(x, 0.0));\n"),
            FusedEwiseOp::Relu => s.push_str("  x = max(x, 0.0);\n"),
            FusedEwiseOp::Sigmoid => s.push_str("  x = 1.0 / (1.0 + exp(-x));\n"),
            FusedEwiseOp::Silu => s.push_str("  x = x / (1.0 + exp(-x));\n"),
            FusedEwiseOp::Clamp(lo, hi) => {
                let (a, b) = (lo.min(hi), lo.max(hi));
                s.push_str(&format!("  x = clamp(x, {:.9}, {:.9});\n", a, b));
            }
            FusedEwiseOp::PowScalar(p) => {
                if p == 0.0 {
                    s.push_str("  x = 1.0;\n");
                } else if p == 1.0 {
                } else if p == 2.0 {
                    s.push_str("  x = x * x;\n");
                } else if p == 3.0 {
                    s.push_str("  x = x * x * x;\n");
                } else {
                    s.push_str(&format!("  x = pow(max(x, 0.0), {:.9});\n", p));
                }
            }
            FusedEwiseOp::Tanh => {
                s.push_str("  float t2 = clamp(2.0 * x, -18.0, 18.0); x = 1.0 - 2.0 / (exp(t2) + 1.0);\n")
            }
            FusedEwiseOp::Gelu => {
                s.push_str("  float y = 0.79788456 * (x + 0.044715 * x * x * x); float y2 = clamp(2.0 * y, -18.0, 18.0); x = 0.5 * x * (2.0 - 2.0 / (exp(y2) + 1.0));\n")
            }
        }
    }
    s.push_str("  Out[i] = x;\n}\n");
    s
}

fn hlsl_fused_linear(act_code: u32, has_bias: bool) -> String {
    let mut s = String::from(
        "cbuffer Params : register(b0) { uint M; uint K; uint N; uint _p3; };\n\
         StructuredBuffer<float> A : register(t0);\n\
         StructuredBuffer<float> B : register(t1);\n\
         RWStructuredBuffer<float> Out : register(u0);\n\
         [numthreads(16, 16, 1)]\n\
         void CSMain(uint3 dtid : SV_DispatchThreadID) {\n\
           uint col = dtid.x; uint row = dtid.y;\n\
           if (row >= M || col >= N) return;\n\
           float acc = 0.0;\n\
           uint rk = row * K;\n\
           for (uint p = 0; p < K; p++) { acc += A[rk + p] * B[p * N + col]; }\n",
    );
    if has_bias {
        s.push_str("  acc += B[K * N + col];\n");
    }
    match act_code {
        1 => s.push_str("  acc = max(acc, 0.0);\n"),
        2 => s.push_str("  float y = 0.79788456 * (acc + 0.044715 * acc * acc * acc); float y2 = clamp(2.0 * y, -18.0, 18.0); acc = 0.5 * acc * (2.0 - 2.0 / (exp(y2) + 1.0));\n"),
        3 => s.push_str("  acc = acc / (1.0 + exp(-acc));\n"),
        _ => {}
    }
    s.push_str("  Out[row * N + col] = acc;\n}\n");
    s
}

pub(crate) fn hlsl_tiled_matmul() -> String {
    String::from(
        "cbuffer Params : register(b0) { uint M; uint K; uint N; uint _p3; };\n\
         StructuredBuffer<float> A : register(t0);\n\
         StructuredBuffer<float> B : register(t1);\n\
         RWStructuredBuffer<float> Out : register(u0);\n\
         groupshared float tileA[16][16];\n\
         groupshared float tileB[16][16];\n\
         [numthreads(16, 16, 1)]\n\
         void CSMain(uint3 dtid : SV_DispatchThreadID, uint3 gtid : SV_GroupThreadID) {\n\
           uint col = dtid.x; uint row = dtid.y;\n\
           uint lx = gtid.x; uint ly = gtid.y;\n\
           float acc = 0.0;\n\
           uint numTiles = (K + 15) / 16;\n\
           for (uint t = 0; t < numTiles; t++) {\n\
             uint ac = t * 16 + lx;\n\
             uint br = t * 16 + ly;\n\
             tileA[ly][lx] = (row < M && ac < K) ? A[row * K + ac] : 0.0;\n\
             tileB[ly][lx] = (br < K && col < N) ? B[br * N + col] : 0.0;\n\
             GroupMemoryBarrierWithGroupSync();\n\
             [unroll] for (uint k = 0; k < 16; k++) { acc += tileA[ly][k] * tileB[k][lx]; }\n\
             GroupMemoryBarrierWithGroupSync();\n\
           }\n\
           if (row < M && col < N) { Out[row * N + col] = acc; }\n\
         }\n",
    )
}

pub(crate) fn hlsl_attention(causal: bool) -> String {
    let limit_expr = if causal {
        "uint kLimit = min(SeqK, qRow + 1);"
    } else {
        "uint kLimit = SeqK;"
    };
    format!(
        "cbuffer Params : register(b0) {{ uint SeqQ; uint SeqK; uint HeadDim; uint Causal; }};\n\
         StructuredBuffer<float> Q : register(t0);\n\
         StructuredBuffer<float> KV : register(t1);\n\
         RWStructuredBuffer<float> Out : register(u0);\n\
         [numthreads(64, 1, 1)]\n\
         void CSMain(uint3 dtid : SV_DispatchThreadID) {{\n\
           uint gid = dtid.x;\n\
           if (gid >= SeqQ * HeadDim || HeadDim == 0) return;\n\
           uint qRow = gid / HeadDim; uint dCol = gid % HeadDim;\n\
           float invSqrtD = 1.0 / sqrt((float)HeadDim);\n\
           uint vOff = SeqK * HeadDim;\n\
           float mx = -1.0e30; float sumE = 0.0; float accO = 0.0;\n\
           {limit_expr}\n\
           for (uint k = 0; k < kLimit; k++) {{\n\
             float dot = 0.0;\n\
             for (uint p = 0; p < HeadDim; p++) {{ dot += Q[qRow * HeadDim + p] * KV[k * HeadDim + p]; }}\n\
             float score = dot * invSqrtD;\n\
             float newMax = max(mx, score);\n\
             float alpha = exp(mx - newMax);\n\
             float w = exp(score - newMax);\n\
             sumE = sumE * alpha + w;\n\
             accO = accO * alpha + w * KV[vOff + k * HeadDim + dCol];\n\
             mx = newMax;\n\
           }}\n\
           Out[gid] = (sumE > 0.0) ? (accO / sumE) : 0.0;\n\
         }}\n"
    )
}

pub(crate) fn hlsl_fused_layernorm_residual(act_code: u32, has_gamma_beta: bool) -> String {
    let mut s = String::from(
        "cbuffer Params : register(b0) { uint Rows; uint Cols; uint EpsBits; uint _p3; };\n\
         StructuredBuffer<float> A : register(t0);\n\
         StructuredBuffer<float> B : register(t1);\n\
         RWStructuredBuffer<float> Out : register(u0);\n\
         [numthreads(64, 1, 1)]\n\
         void CSMain(uint3 dtid : SV_DispatchThreadID) {\n\
           uint r = dtid.x; if (r >= Rows || Cols == 0) return;\n\
           float eps = asfloat(EpsBits);\n\
           uint base = r * Cols;\n\
           float sum = 0.0;\n\
           for (uint c = 0; c < Cols; c++) { sum += A[base + c] + B[base + c]; }\n\
           float mean = sum / (float)Cols;\n\
           float sq = 0.0;\n\
           for (uint c2 = 0; c2 < Cols; c2++) { float d = (A[base + c2] + B[base + c2]) - mean; sq += d * d; }\n\
           float invStd = 1.0 / sqrt(sq / (float)Cols + eps);\n",
    );
    if has_gamma_beta {
        s.push_str(
            "  uint gOff = Rows * Cols; uint bOff = gOff + Cols;\n\
             for (uint c3 = 0; c3 < Cols; c3++) {\n\
               float norm = ((A[base + c3] + B[base + c3]) - mean) * invStd * B[gOff + c3] + B[bOff + c3];\n",
        );
    } else {
        s.push_str(
            "  for (uint c3 = 0; c3 < Cols; c3++) {\n\
               float norm = ((A[base + c3] + B[base + c3]) - mean) * invStd;\n",
        );
    }
    match act_code {
        1 => s.push_str("    norm = max(norm, 0.0);\n"),
        2 => s.push_str("    float y = 0.79788456 * (norm + 0.044715 * norm * norm * norm); float y2 = clamp(2.0 * y, -18.0, 18.0); norm = 0.5 * norm * (2.0 - 2.0 / (exp(y2) + 1.0));\n"),
        3 => s.push_str("    norm = norm / (1.0 + exp(-norm));\n"),
        _ => {}
    }
    s.push_str("    Out[base + c3] = norm;\n  }\n}\n");
    s
}

pub(crate) fn parse_fused_act_code(act: &str) -> Result<u32, String> {
    match act {
        "none" | "" => Ok(0),
        "relu" => Ok(1),
        "gelu" => Ok(2),
        "silu" => Ok(3),
        other => Err(format!(
            "unsupported activation '{other}'; expected 'none', 'relu', 'gelu', or 'silu'"
        )),
    }
}

fn apply_act_f64(acc: f64, act_code: u32) -> f64 {
    match act_code {
        1 => acc.max(0.0),
        2 => {
            let y = 0.797_884_560_802_865_4 * (acc + 0.044_715 * acc * acc * acc);
            0.5 * acc * (1.0 + y.tanh())
        }
        3 => acc / (1.0 + (-acc).exp()),
        _ => acc,
    }
}

pub(crate) fn gpu_tiled_matmul_f64(a: &[f64], b: &[f64], m: usize, k: usize, n: usize) -> Vec<f64> {
    if m == 0 || k == 0 || n == 0 {
        return vec![0.0; m * n];
    }
    GPU_FUSED_KERNELS.fetch_add(1, Ordering::Relaxed);
    let gx = (n as u32).div_ceil(16);
    let gy = (m as u32).div_ceil(16);
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    if matches!(
        resolve_backend(),
        ResolvedBackend::D3D11 | ResolvedBackend::Vulkan
    ) && !want_f64
    {
        let af: Vec<f32> = a.iter().map(|&v| v as f32).collect();
        let bf: Vec<f32> = b.iter().map(|&v| v as f32).collect();
        match resolve_backend() {
            ResolvedBackend::D3D11 => {
                #[cfg(windows)]
                {
                    let src = hlsl_tiled_matmul();
                    if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                        st.dispatch(
                            "veyra:tiled_matmul",
                            &src,
                            "CSMain",
                            &af,
                            &bf,
                            m * n,
                            &[m as u32, k as u32, n as u32, 0],
                            (gx, gy, 1),
                        )
                    }) {
                        return out.into_iter().map(|v| v as f64).collect();
                    }
                }
            }
            ResolvedBackend::Vulkan => {
                let spv = crate::spirv::spirv_tiled_matmul();
                if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                    st.dispatch(
                        "vk:tiled_matmul",
                        &spv,
                        &af,
                        &bf,
                        m * n,
                        &[m as u32, k as u32, n as u32, 0],
                        (gx, gy, 1),
                    )
                }) {
                    return out.into_iter().map(|v| v as f64).collect();
                }
            }
            ResolvedBackend::Cpu => {}
        }
    }

    // Cache-blocked 16x16 tiled CPU fallback matching workgroup tile geometry
    const TILE: usize = 16;
    let mut out = vec![0.0; m * n];
    for r0 in (0..m).step_by(TILE) {
        let r_end = (r0 + TILE).min(m);
        for c0 in (0..n).step_by(TILE) {
            let c_end = (c0 + TILE).min(n);
            for p0 in (0..k).step_by(TILE) {
                let p_end = (p0 + TILE).min(k);
                for r in r0..r_end {
                    let rk = r * k;
                    let rn = r * n;
                    for p in p0..p_end {
                        let ap = a[rk + p];
                        let pn = p * n;
                        for c in c0..c_end {
                            out[rn + c] += ap * b[pn + c];
                        }
                    }
                }
            }
        }
    }
    out
}

pub(crate) fn gpu_attention_f64(
    q: &[f64],
    k_mat: &[f64],
    v: &[f64],
    seq_q: usize,
    seq_k: usize,
    head_dim: usize,
    causal: bool,
) -> Vec<f64> {
    if seq_q == 0 || seq_k == 0 || head_dim == 0 {
        return vec![0.0; seq_q * head_dim];
    }
    GPU_FUSED_KERNELS.fetch_add(1, Ordering::Relaxed);
    let total = seq_q * head_dim;
    let gx = (total as u32).div_ceil(64);
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    if matches!(
        resolve_backend(),
        ResolvedBackend::D3D11 | ResolvedBackend::Vulkan
    ) && !want_f64
    {
        let af: Vec<f32> = q.iter().map(|&x| x as f32).collect();
        let mut bf: Vec<f32> = Vec::with_capacity(k_mat.len() + v.len());
        bf.extend(k_mat.iter().map(|&x| x as f32));
        bf.extend(v.iter().map(|&x| x as f32));
        match resolve_backend() {
            ResolvedBackend::D3D11 => {
                #[cfg(windows)]
                {
                    let src = hlsl_attention(causal);
                    let key = format!("veyra:attn:{}", u8::from(causal));
                    if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                        st.dispatch(
                            &key,
                            &src,
                            "CSMain",
                            &af,
                            &bf,
                            total,
                            &[
                                seq_q as u32,
                                seq_k as u32,
                                head_dim as u32,
                                u32::from(causal),
                            ],
                            (gx, 1, 1),
                        )
                    }) {
                        return out.into_iter().map(|x| x as f64).collect();
                    }
                }
            }
            ResolvedBackend::Vulkan => {
                let spv = crate::spirv::spirv_attention(causal);
                let key = format!("vk:attn:{}", u8::from(causal));
                if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                    st.dispatch(
                        &key,
                        &spv,
                        &af,
                        &bf,
                        total,
                        &[
                            seq_q as u32,
                            seq_k as u32,
                            head_dim as u32,
                            u32::from(causal),
                        ],
                        (gx, 1, 1),
                    )
                }) {
                    return out.into_iter().map(|x| x as f64).collect();
                }
            }
            ResolvedBackend::Cpu => {}
        }
    }

    // Online-softmax FlashAttention-style CPU path (single pass over K/V per query row)
    let inv_sqrt_d = 1.0 / (head_dim as f64).sqrt();
    let mut out = vec![0.0; seq_q * head_dim];
    for qr in 0..seq_q {
        let q_row = &q[qr * head_dim..(qr + 1) * head_dim];
        let k_limit = if causal { (qr + 1).min(seq_k) } else { seq_k };
        let mut mx = f64::NEG_INFINITY;
        let mut sum_e = 0.0;
        let out_row = &mut out[qr * head_dim..(qr + 1) * head_dim];
        for kr in 0..k_limit {
            let k_row = &k_mat[kr * head_dim..(kr + 1) * head_dim];
            let mut dot = 0.0;
            for d in 0..head_dim {
                dot += q_row[d] * k_row[d];
            }
            let score = dot * inv_sqrt_d;
            let new_max = mx.max(score);
            let alpha = if mx.is_finite() {
                (mx - new_max).exp()
            } else {
                0.0
            };
            let w = (score - new_max).exp();
            sum_e = sum_e * alpha + w;
            let v_row = &v[kr * head_dim..(kr + 1) * head_dim];
            for d in 0..head_dim {
                out_row[d] = out_row[d] * alpha + w * v_row[d];
            }
            mx = new_max;
        }
        if sum_e > 0.0 {
            let inv = 1.0 / sum_e;
            for v_o in out_row.iter_mut() {
                *v_o *= inv;
            }
        }
    }
    out
}

pub(crate) fn gpu_fused_layernorm_f64(
    x: &[f64],
    residual: &[f64],
    gamma: Option<&[f64]>,
    beta: Option<&[f64]>,
    rows: usize,
    cols: usize,
    eps: f64,
    act: &str,
) -> Result<Vec<f64>, String> {
    let act_code = parse_fused_act_code(act)?;
    if rows == 0 || cols == 0 {
        return Ok(vec![0.0; rows * cols]);
    }
    GPU_FUSED_KERNELS.fetch_add(1, Ordering::Relaxed);
    let has_gb = gamma.is_some() || beta.is_some();
    let gx = (rows as u32).div_ceil(64);
    let eps_bits = (eps as f32).to_bits();
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    if matches!(
        resolve_backend(),
        ResolvedBackend::D3D11 | ResolvedBackend::Vulkan
    ) && !want_f64
    {
        let af: Vec<f32> = x.iter().map(|&v| v as f32).collect();
        let mut bf: Vec<f32> =
            Vec::with_capacity(residual.len() + if has_gb { cols * 2 } else { 0 });
        bf.extend(residual.iter().map(|&v| v as f32));
        if has_gb {
            if let Some(g) = gamma {
                bf.extend(g.iter().map(|&v| v as f32));
            } else {
                bf.extend(std::iter::repeat_n(1.0f32, cols));
            }
            if let Some(b) = beta {
                bf.extend(b.iter().map(|&v| v as f32));
            } else {
                bf.extend(std::iter::repeat_n(0.0f32, cols));
            }
        }
        match resolve_backend() {
            ResolvedBackend::D3D11 => {
                #[cfg(windows)]
                {
                    let src = hlsl_fused_layernorm_residual(act_code, has_gb);
                    let key = format!("veyra:fused_ln:{act_code}:{}", u8::from(has_gb));
                    if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                        st.dispatch(
                            &key,
                            &src,
                            "CSMain",
                            &af,
                            &bf,
                            rows * cols,
                            &[rows as u32, cols as u32, eps_bits, 0],
                            (gx, 1, 1),
                        )
                    }) {
                        return Ok(out.into_iter().map(|v| v as f64).collect());
                    }
                }
            }
            ResolvedBackend::Vulkan => {
                let spv = crate::spirv::spirv_fused_layernorm_residual(act_code, has_gb);
                let key = format!("vk:fused_ln:{act_code}:{}", u8::from(has_gb));
                if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                    st.dispatch(
                        &key,
                        &spv,
                        &af,
                        &bf,
                        rows * cols,
                        &[rows as u32, cols as u32, eps_bits, 0],
                        (gx, 1, 1),
                    )
                }) {
                    return Ok(out.into_iter().map(|v| v as f64).collect());
                }
            }
            ResolvedBackend::Cpu => {}
        }
    }

    let mut out = vec![0.0; rows * cols];
    for r in 0..rows {
        let base = r * cols;
        let mut sum = 0.0;
        for c in 0..cols {
            sum += x[base + c] + residual[base + c];
        }
        let mean = sum / cols as f64;
        let mut sq = 0.0;
        for c in 0..cols {
            let d = (x[base + c] + residual[base + c]) - mean;
            sq += d * d;
        }
        let inv_std = 1.0 / (sq / cols as f64 + eps).sqrt();
        for c in 0..cols {
            let mut norm = ((x[base + c] + residual[base + c]) - mean) * inv_std;
            if let Some(g) = gamma {
                norm *= g[c];
            }
            if let Some(b) = beta {
                norm += b[c];
            }
            out[base + c] = apply_act_f64(norm, act_code);
        }
    }
    Ok(out)
}

fn fused_pipeline_hash(ops: &[crate::spirv::FusedEwiseOp]) -> u64 {
    use crate::spirv::FusedEwiseOp;
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &op in ops {
        let (tag, lo_bits, hi_bits) = match op {
            FusedEwiseOp::Add => (1u64, 0u32, 0u32),
            FusedEwiseOp::Sub => (2, 0, 0),
            FusedEwiseOp::Mul => (3, 0, 0),
            FusedEwiseOp::AddScalar(v) => (4, v.to_bits(), 0),
            FusedEwiseOp::Scale(v) => (5, v.to_bits(), 0),
            FusedEwiseOp::Abs => (6, 0, 0),
            FusedEwiseOp::Neg => (7, 0, 0),
            FusedEwiseOp::Sqrt => (8, 0, 0),
            FusedEwiseOp::Relu => (9, 0, 0),
            FusedEwiseOp::Sigmoid => (10, 0, 0),
            FusedEwiseOp::Gelu => (11, 0, 0),
            FusedEwiseOp::Tanh => (12, 0, 0),
            FusedEwiseOp::Div => (13, 0, 0),
            FusedEwiseOp::Silu => (14, 0, 0),
            FusedEwiseOp::Clamp(lo, hi) => (15, lo.to_bits(), hi.to_bits()),
            FusedEwiseOp::PowScalar(p) => (16, p.to_bits(), 0),
            FusedEwiseOp::Recip => (17, 0, 0),
        };
        h ^= (tag << 48) | (u64::from(hi_bits) << 32) | u64::from(lo_bits);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

pub(crate) fn gpu_fuse_f64(ops: &[crate::spirv::FusedEwiseOp], a: &[f64], b: &[f64]) -> Vec<f64> {
    use crate::spirv::FusedEwiseOp;
    if a.is_empty() {
        return Vec::new();
    }
    GPU_FUSED_KERNELS.fetch_add(1, Ordering::Relaxed);
    let groups = (a.len() as u32).div_ceil(256);
    let want_f64 = current_precision_pref() == PrecisionPref::F64;
    let hash = fused_pipeline_hash(ops);

    match resolve_backend() {
        ResolvedBackend::D3D11 if !want_f64 => {
            #[cfg(windows)]
            {
                let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
                let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
                let src = hlsl_fused_ewise(ops);
                let key = format!("veyra:fused:{hash:016x}");
                if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                    st.dispatch(
                        &key,
                        &src,
                        "CSMain",
                        &af,
                        &bf,
                        a.len(),
                        &[a.len() as u32, 0, 0, 0],
                        (groups, 1, 1),
                    )
                }) {
                    return out.into_iter().map(|x| x as f64).collect();
                }
            }
        }
        ResolvedBackend::Vulkan if !want_f64 => {
            let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
            let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
            let spv = crate::spirv::spirv_fused_ewise(ops);
            let key = format!("vk:fused:{hash:016x}");
            if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                st.dispatch(
                    &key,
                    &spv,
                    &af,
                    &bf,
                    a.len(),
                    &[a.len() as u32],
                    (groups, 1, 1),
                )
            }) {
                return out.into_iter().map(|x| x as f64).collect();
            }
        }
        _ => {}
    }

    a.iter()
        .enumerate()
        .map(|(i, &x0)| {
            let b_val = b.get(i).copied().unwrap_or(0.0);
            let mut x = x0;
            for &op in ops {
                x = match op {
                    FusedEwiseOp::Add => x + b_val,
                    FusedEwiseOp::Sub => x - b_val,
                    FusedEwiseOp::Mul => x * b_val,
                    FusedEwiseOp::Div => x / b_val,
                    FusedEwiseOp::AddScalar(s) => x + f64::from(s),
                    FusedEwiseOp::Scale(s) => x * f64::from(s),
                    FusedEwiseOp::Abs => x.abs(),
                    FusedEwiseOp::Neg => -x,
                    FusedEwiseOp::Recip => 1.0 / x,
                    FusedEwiseOp::Sqrt => x.max(0.0).sqrt(),
                    FusedEwiseOp::Relu => x.max(0.0),
                    FusedEwiseOp::Sigmoid => 1.0 / (1.0 + (-x).exp()),
                    FusedEwiseOp::Silu => x / (1.0 + (-x).exp()),
                    FusedEwiseOp::Tanh => x.tanh(),
                    FusedEwiseOp::Clamp(lo, hi) => {
                        x.clamp(f64::from(lo.min(hi)), f64::from(lo.max(hi)))
                    }
                    FusedEwiseOp::PowScalar(p) => x.powf(f64::from(p)),
                    FusedEwiseOp::Gelu => {
                        let y = 0.797_884_560_802_865_4 * (x + 0.044_715 * x * x * x);
                        0.5 * x * (1.0 + y.tanh())
                    }
                };
            }
            x
        })
        .collect()
}

pub(crate) fn gpu_fused_linear_f64(
    x: &[f64],
    w: &[f64],
    bias: Option<&[f64]>,
    m: usize,
    k: usize,
    n: usize,
    act: &str,
) -> Result<Vec<f64>, String> {
    let act_code: u32 = parse_fused_act_code(act)?;
    if m == 0 || k == 0 || n == 0 {
        return Ok(vec![0.0; m * n]);
    }
    GPU_FUSED_KERNELS.fetch_add(1, Ordering::Relaxed);
    let has_bias = bias.is_some();
    let gx = (n as u32).div_ceil(16);
    let gy = (m as u32).div_ceil(16);
    let want_f64 = current_precision_pref() == PrecisionPref::F64;

    if matches!(
        resolve_backend(),
        ResolvedBackend::D3D11 | ResolvedBackend::Vulkan
    ) && !want_f64
    {
        let af: Vec<f32> = x.iter().map(|&v| v as f32).collect();
        let mut bf: Vec<f32> = Vec::with_capacity(w.len() + bias.map(|b| b.len()).unwrap_or(0));
        bf.extend(w.iter().map(|&v| v as f32));
        if let Some(b) = bias {
            bf.extend(b.iter().map(|&v| v as f32));
        }
        match resolve_backend() {
            ResolvedBackend::D3D11 => {
                #[cfg(windows)]
                {
                    let src = hlsl_fused_linear(act_code, has_bias);
                    let key = format!("veyra:fused_linear:{act_code}:{}", usize::from(has_bias));
                    if let Ok(out) = d3d11_backend::with_d3d11(|st| {
                        st.dispatch(
                            &key,
                            &src,
                            "CSMain",
                            &af,
                            &bf,
                            m * n,
                            &[m as u32, k as u32, n as u32, 0],
                            (gx, gy, 1),
                        )
                    }) {
                        return Ok(out.into_iter().map(|v| v as f64).collect());
                    }
                }
            }
            ResolvedBackend::Vulkan => {
                let spv = crate::spirv::spirv_fused_linear(act_code, has_bias);
                let key = format!("vk:fused_linear:{act_code}:{}", usize::from(has_bias));
                if let Ok(out) = vulkan_backend::with_vulkan(|st| {
                    st.dispatch(
                        &key,
                        &spv,
                        &af,
                        &bf,
                        m * n,
                        &[m as u32, k as u32, n as u32, 0],
                        (gx, gy, 1),
                    )
                }) {
                    return Ok(out.into_iter().map(|v| v as f64).collect());
                }
            }
            ResolvedBackend::Cpu => {}
        }
    }

    let mut out = vec![0.0; m * n];
    for r in 0..m {
        let rk = r * k;
        let rn = r * n;
        for c in 0..n {
            let mut acc = 0.0;
            for p in 0..k {
                acc += x[rk + p] * w[p * n + c];
            }
            if let Some(b) = bias {
                acc += b[c];
            }
            out[rn + c] = match act_code {
                1 => acc.max(0.0),
                2 => {
                    let y = 0.797_884_560_802_865_4 * (acc + 0.044_715 * acc * acc * acc);
                    0.5 * acc * (1.0 + y.tanh())
                }
                3 => acc / (1.0 + (-acc).exp()),
                _ => acc,
            };
        }
    }
    Ok(out)
}
