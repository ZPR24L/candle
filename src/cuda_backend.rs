use crate::{CpuStorage, DType, Error, Result, Shape};
use cudarc::driver::{CudaSlice, LaunchAsync, LaunchConfig};

pub type CudaError = cudarc::driver::DriverError;

#[derive(Debug, Clone)]
pub struct CudaDevice(std::sync::Arc<cudarc::driver::CudaDevice>);

// TODO: Switch to pre-compiled PTX kernels rather than compiling on the fly.
const AFFINE_CU: &str = r#"
extern "C" __global__ void affine_f32( 
    const size_t numel, 
    const float *x,
    float *y,
    const float mul,
    const float add
) { 
    unsigned int i = blockIdx.x * blockDim.x + threadIdx.x; 
    if (i >= numel) { 
        return; 
    } 
    y[i] = x[i] * mul + add;
} 
"#;

const FILL_CU: &str = r#"
template<typename T>
__device__ void fill_with(T *buf, T value, const size_t numel) {
    for (unsigned int i = blockIdx.x * blockDim.x + threadIdx.x; i < numel; i += blockDim.x * gridDim.x) {
        buf[i] = value;
    }
}
extern "C" __global__ void fill_f16(__half *buf, __half value, const size_t numel) { fill_with(buf, value, numel); }
extern "C" __global__ void fill_f32(float *buf, float value, const size_t numel) { fill_with(buf, value, numel); }
extern "C" __global__ void fill_f64(double *buf, double value, const size_t numel) { fill_with(buf, value, numel); }
"#;

impl CudaDevice {
    pub(crate) fn new(ordinal: usize) -> Result<Self> {
        let device = cudarc::driver::CudaDevice::new(ordinal)?;
        Ok(Self(device))
    }

    pub(crate) fn ordinal(&self) -> usize {
        self.0.ordinal()
    }

    pub(crate) fn zeros_impl(&self, shape: &Shape, dtype: DType) -> Result<CudaStorage> {
        let elem_count = shape.elem_count();
        match dtype {
            DType::F32 => {
                let data = self.0.alloc_zeros::<f32>(elem_count)?;
                Ok(CudaStorage::F32(data))
            }
            DType::F64 => {
                let data = self.0.alloc_zeros::<f64>(elem_count)?;
                Ok(CudaStorage::F64(data))
            }
        }
    }

    pub(crate) fn const_impl(&self, v: f64, shape: &Shape, dtype: DType) -> Result<CudaStorage> {
        let elem_count = shape.elem_count();
        let dev = &self.0;
        match dtype {
            DType::F32 => {
                // SAFETY: Set later by running the fill kernel.
                let data = unsafe { dev.alloc::<f32>(elem_count) }?;
                let module_name = "fill_f32";
                if !dev.has_func(module_name, module_name) {
                    let ptx = cudarc::nvrtc::compile_ptx(FILL_CU).unwrap();
                    dev.load_ptx(ptx, module_name, &[module_name])?;
                }
                let fwd_fn = dev.get_func(module_name, module_name).unwrap();
                let cfg = LaunchConfig::for_num_elems(elem_count as u32);
                let params = (&data, v as f32, elem_count);
                unsafe { fwd_fn.launch(cfg, params) }?;
                Ok(CudaStorage::F32(data))
            }
            DType::F64 => {
                // SAFETY: Set later by running the fill kernel.
                let data = unsafe { dev.alloc::<f64>(elem_count) }?;
                let module_name = "fill_f64";
                if !dev.has_func(module_name, module_name) {
                    let ptx = cudarc::nvrtc::compile_ptx(FILL_CU).unwrap();
                    dev.load_ptx(ptx, module_name, &[module_name])?;
                }
                let fwd_fn = dev.get_func(module_name, module_name).unwrap();
                let cfg = LaunchConfig::for_num_elems(elem_count as u32);
                let params = (&data, v, elem_count);
                unsafe { fwd_fn.launch(cfg, params) }?;
                Ok(CudaStorage::F64(data))
            }
        }
    }

    pub(crate) fn ones_impl(&self, shape: &Shape, dtype: DType) -> Result<CudaStorage> {
        self.const_impl(1., shape, dtype)
    }

    pub(crate) fn cuda_from_cpu_storage(&self, storage: &CpuStorage) -> Result<CudaStorage> {
        match storage {
            CpuStorage::F32(storage) => {
                let data = self.0.htod_sync_copy(storage)?;
                Ok(CudaStorage::F32(data))
            }
            CpuStorage::F64(storage) => {
                let data = self.0.htod_sync_copy(storage)?;
                Ok(CudaStorage::F64(data))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum CudaStorage {
    F32(CudaSlice<f32>),
    F64(CudaSlice<f64>),
}

impl CudaStorage {
    pub fn dtype(&self) -> DType {
        match self {
            Self::F32(_) => DType::F32,
            Self::F64(_) => DType::F64,
        }
    }

    pub fn device(&self) -> CudaDevice {
        match self {
            Self::F32(slice) => CudaDevice(slice.device()),
            Self::F64(slice) => CudaDevice(slice.device()),
        }
    }

    pub(crate) fn affine_impl(
        &self,
        shape: &Shape,
        stride: &[usize],
        mul: f64,
        add: f64,
    ) -> Result<Self> {
        match self {
            Self::F32(arg) => {
                if !shape.is_contiguous(stride) {
                    return Err(Error::RequiresContiguous { op: "affine" });
                }
                let dev = arg.device();
                let module_name = "affine_f32";
                if !dev.has_func(module_name, module_name) {
                    let ptx = cudarc::nvrtc::compile_ptx(AFFINE_CU).unwrap();
                    dev.load_ptx(ptx, module_name, &[module_name])?;
                }
                let elem_count = shape.elem_count();
                let fwd_fn = dev.get_func(module_name, module_name).unwrap();
                let cfg = LaunchConfig::for_num_elems(elem_count as u32);
                // SAFETY: if this function returns Ok(..), the kernel has been applied
                // and has set the initially unset memory.
                let out = unsafe { dev.alloc::<f32>(elem_count) }?;
                let params = (elem_count, arg, &out, mul as f32, add as f32);
                // SAFETY: well, well, well...
                unsafe { fwd_fn.launch(cfg, params) }?;
                Ok(Self::F32(out))
            }
            Self::F64(_) => {
                todo!()
            }
        }
    }

    pub(crate) fn to_cpu_storage(&self) -> Result<CpuStorage> {
        match self {
            Self::F32(slice) => {
                let dev = slice.device();
                let cpu_storage = dev.dtoh_sync_copy(slice)?;
                Ok(CpuStorage::F32(cpu_storage))
            }
            Self::F64(slice) => {
                let dev = slice.device();
                let cpu_storage = dev.dtoh_sync_copy(slice)?;
                Ok(CpuStorage::F64(cpu_storage))
            }
        }
    }
}