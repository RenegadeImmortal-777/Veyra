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
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};

use crate::tensor::Conv2dDims;
use crate::value::Val;

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

            Ok(Self {
                _dxgi: dxgi,
                _d3d11: d3d11,
                _d3dcompiler: d3dcompiler,
                d3d_compile,
                device: ComPtr(dev_raw),
                context: ComPtr(ctx_raw),
                backend,
                active_adapter,
                adapters,
                feature_level: feat,
                is_hardware,
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

        fn create_structured_srv(&self, data: &[f32]) -> Result<(ComPtr, ComPtr), String> {
            let safe_data;
            let slice = if data.is_empty() {
                safe_data = [0.0f32];
                &safe_data[..]
            } else {
                data
            };
            let desc = D3D11BufferDesc {
                byte_width: (slice.len() * 4) as u32,
                usage: D3D11_USAGE_DEFAULT,
                bind_flags: D3D11_BIND_SHADER_RESOURCE,
                cpu_access_flags: 0,
                misc_flags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED,
                structure_byte_stride: 4,
            };
            let init = D3D11SubresourceData {
                sys_mem: slice.as_ptr() as *const c_void,
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
                num_elements: slice.len() as u32,
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

        fn create_structured_uav(&self, count: usize) -> Result<(ComPtr, ComPtr), String> {
            let safe_count = count.max(1);
            let desc = D3D11BufferDesc {
                byte_width: (safe_count * 4) as u32,
                usage: D3D11_USAGE_DEFAULT,
                bind_flags: D3D11_BIND_UNORDERED_ACCESS | D3D11_BIND_SHADER_RESOURCE,
                cpu_access_flags: 0,
                misc_flags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED,
                structure_byte_stride: 4,
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

        fn read_back_uav(&self, out_buf: ComPtr, count: usize) -> Result<Vec<f32>, String> {
            let safe_count = count.max(1);
            let desc = D3D11BufferDesc {
                byte_width: (safe_count * 4) as u32,
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
            let out =
                unsafe { core::slice::from_raw_parts(mapped.data as *const f32, count).to_vec() };
            unsafe {
                unmap_fn(self.context.0, staging.0, 0);
                staging.release();
            }
            Ok(out)
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
            let shader = self.compile_or_get_shader(cache_key, hlsl, entry)?;
            let (buf_a, srv_a) = self.create_structured_srv(a)?;
            let (buf_b, srv_b) = match self.create_structured_srv(b) {
                Ok(v) => v,
                Err(e) => {
                    unsafe {
                        srv_a.release();
                        buf_a.release();
                    }
                    return Err(e);
                }
            };
            let (buf_out, uav_out) = match self.create_structured_uav(out_len) {
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
                self.read_back_uav(buf_out, out_len)
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

pub(crate) fn gpu_available() -> bool {
    #[cfg(windows)]
    {
        d3d11_backend::with_d3d11(|_| Ok(true)).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub(crate) fn gpu_info_val() -> Val {
    #[cfg(windows)]
    {
        if let Ok(val) = d3d11_backend::with_d3d11(|st| {
            let adapters = st
                .adapters
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
                .collect::<Vec<_>>();
            Ok(Val::Map(vec![
                (Val::Str("available".into()), Val::Bool(true)),
                (Val::Str("backend".into()), Val::Str(st.backend.clone())),
                (Val::Str("dtype".into()), Val::Str("f32".into())),
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
                (Val::Str("adapters".into()), Val::List(adapters)),
                (
                    Val::Str("dispatches".into()),
                    Val::Num(GPU_DISPATCHES.load(Ordering::Relaxed) as f64),
                ),
                (
                    Val::Str("shaders_compiled".into()),
                    Val::Num(GPU_SHADERS_COMPILED.load(Ordering::Relaxed) as f64),
                ),
            ]))
        }) {
            return val;
        }
    }
    Val::Map(vec![
        (Val::Str("available".into()), Val::Bool(false)),
        (Val::Str("backend".into()), Val::Str("cpu-fallback".into())),
        (Val::Str("dtype".into()), Val::Str("f64".into())),
        (
            Val::Str("adapter_name".into()),
            Val::Str("CPU Thread Pool".into()),
        ),
        (Val::Str("is_hardware".into()), Val::Bool(false)),
        (Val::Str("adapters".into()), Val::List(Vec::new())),
        (
            Val::Str("dispatches".into()),
            Val::Num(GPU_DISPATCHES.load(Ordering::Relaxed) as f64),
        ),
        (
            Val::Str("shaders_compiled".into()),
            Val::Num(GPU_SHADERS_COMPILED.load(Ordering::Relaxed) as f64),
        ),
    ])
}

pub(crate) fn gpu_matmul_f64(a: &[f64], b: &[f64], m: usize, k: usize, n: usize) -> Vec<f64> {
    if m == 0 || k == 0 || n == 0 {
        return vec![0.0; m * n];
    }
    #[cfg(windows)]
    {
        let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
        let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
        let gx = (n as u32).div_ceil(16);
        let gy = (m as u32).div_ceil(16);
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
    #[cfg(windows)]
    {
        let af: Vec<f32> = a.iter().map(|&x| x as f32).collect();
        let bf: Vec<f32> = b.iter().map(|&x| x as f32).collect();
        let groups = (a.len() as u32).div_ceil(256);
        let cb = [a.len() as u32, op_code, (scalar as f32).to_bits(), 0];
        if let Ok(out) = d3d11_backend::with_d3d11(|st| {
            st.dispatch(
                "veyra:ewise:v1",
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
    a.iter()
        .enumerate()
        .map(|(i, &x)| match op_code {
            0 => x + b.get(i).copied().unwrap_or(0.0),
            1 => x - b.get(i).copied().unwrap_or(0.0),
            2 => x * b.get(i).copied().unwrap_or(1.0),
            3 => x.max(0.0),
            4 => 1.0 / (1.0 + (-x).exp()),
            5 => x * scalar,
            _ => x,
        })
        .collect()
}

pub(crate) fn gpu_softmax_rows_f64(data: &[f64], rows: usize, cols: usize) -> Vec<f64> {
    if rows == 0 || cols == 0 {
        return Vec::new();
    }
    #[cfg(windows)]
    {
        let af: Vec<f32> = data.iter().map(|&x| x as f32).collect();
        let groups = (rows as u32).div_ceil(64);
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
    crate::tensor::softmax_rows(data, rows, cols)
}

pub(crate) fn gpu_conv2d_f64(input: &[f64], weight: &[f64], d: &Conv2dDims) -> Vec<f64> {
    let total = d.n * d.o * d.oh * d.ow;
    if total == 0 {
        return Vec::new();
    }
    #[cfg(windows)]
    {
        let af: Vec<f32> = input.iter().map(|&x| x as f32).collect();
        let bf: Vec<f32> = weight.iter().map(|&x| x as f32).collect();
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
        Err("HLSL cs_5_0 compute shaders require Windows Direct3D 11".into())
    }
}
