#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/*
 * Deliberately use the stable CUDA Driver ABI directly. The probe is compiled
 * inside a release guest, which receives the selected host driver's runtime
 * libraries but must not require a CUDA SDK or compiler toolkit.
 */
typedef int CUresult;
typedef int CUdevice;
typedef unsigned long long CUdeviceptr;
typedef struct CUctx_st *CUcontext;
typedef struct CUmod_st *CUmodule;
typedef struct CUfunc_st *CUfunction;
typedef struct CUstream_st *CUstream;

enum { CUDA_SUCCESS = 0 };

extern CUresult cuInit(unsigned int flags);
extern CUresult cuDeviceGet(CUdevice *device, int ordinal);
extern CUresult cuDeviceGetName(char *name, int length, CUdevice device);
extern CUresult cuCtxCreate_v2(
    CUcontext *context,
    unsigned int flags,
    CUdevice device
);
extern CUresult cuCtxDestroy_v2(CUcontext context);
extern CUresult cuCtxSynchronize(void);
extern CUresult cuModuleLoadData(CUmodule *module, const void *image);
extern CUresult cuModuleUnload(CUmodule module);
extern CUresult cuModuleGetFunction(
    CUfunction *function,
    CUmodule module,
    const char *name
);
extern CUresult cuMemAlloc_v2(CUdeviceptr *device_pointer, size_t bytes);
extern CUresult cuMemFree_v2(CUdeviceptr device_pointer);
extern CUresult cuMemcpyHtoD_v2(
    CUdeviceptr destination,
    const void *source,
    size_t bytes
);
extern CUresult cuMemcpyDtoH_v2(
    void *destination,
    CUdeviceptr source,
    size_t bytes
);
extern CUresult cuLaunchKernel(
    CUfunction function,
    unsigned int grid_width,
    unsigned int grid_height,
    unsigned int grid_depth,
    unsigned int block_width,
    unsigned int block_height,
    unsigned int block_depth,
    unsigned int shared_memory_bytes,
    CUstream stream,
    void **kernel_parameters,
    void **extra
);
extern CUresult cuGetErrorName(CUresult error, const char **name);
extern CUresult cuGetErrorString(CUresult error, const char **message);

static void require_cuda(CUresult result, const char *operation) {
    if (result == CUDA_SUCCESS) {
        return;
    }
    const char *name = "CUDA_ERROR_UNKNOWN";
    const char *message = "unknown CUDA driver error";
    cuGetErrorName(result, &name);
    cuGetErrorString(result, &message);
    fprintf(stderr, "%s failed: %s: %s\n", operation, name, message);
    exit(EXIT_FAILURE);
}

static const char probe_ptx[] =
    ".version 8.0\n"
    ".target sm_80\n"
    ".address_size 64\n"
    ".visible .entry add_one(\n"
    "    .param .u64 values,\n"
    "    .param .u32 count\n"
    ")\n"
    "{\n"
    "    .reg .pred %outside;\n"
    "    .reg .b32 %index, %count_value, %value;\n"
    "    .reg .b64 %base, %offset, %address;\n"
    "    ld.param.u64 %base, [values];\n"
    "    ld.param.u32 %count_value, [count];\n"
    "    mov.u32 %index, %tid.x;\n"
    "    setp.ge.u32 %outside, %index, %count_value;\n"
    "    @%outside bra DONE;\n"
    "    mul.wide.u32 %offset, %index, 4;\n"
    "    add.s64 %address, %base, %offset;\n"
    "    ld.global.u32 %value, [%address];\n"
    "    add.u32 %value, %value, 1;\n"
    "    st.global.u32 [%address], %value;\n"
    "DONE:\n"
    "    ret;\n"
    "}\n";

int main(void) {
    enum { value_count = 4 };
    unsigned int values[value_count] = {1, 2, 3, 4};
    const unsigned int expected[value_count] = {2, 3, 4, 5};
    CUdevice device;
    CUcontext context;
    CUmodule module;
    CUfunction function;
    CUdeviceptr device_values;
    char device_name[256] = {0};

    require_cuda(cuInit(0), "cuInit");
    require_cuda(cuDeviceGet(&device, 0), "cuDeviceGet");
    require_cuda(
        cuDeviceGetName(device_name, sizeof(device_name), device),
        "cuDeviceGetName"
    );
    require_cuda(cuCtxCreate_v2(&context, 0, device), "cuCtxCreate_v2");
    require_cuda(cuModuleLoadData(&module, probe_ptx), "cuModuleLoadData");
    require_cuda(cuModuleGetFunction(&function, module, "add_one"), "cuModuleGetFunction");
    require_cuda(
        cuMemAlloc_v2(&device_values, sizeof(values)),
        "cuMemAlloc_v2"
    );
    require_cuda(
        cuMemcpyHtoD_v2(device_values, values, sizeof(values)),
        "cuMemcpyHtoD_v2"
    );

    unsigned int count = value_count;
    void *arguments[] = {&device_values, &count};
    require_cuda(
        cuLaunchKernel(
            function,
            1, 1, 1,
            value_count, 1, 1,
            0,
            NULL,
            arguments,
            NULL
        ),
        "cuLaunchKernel"
    );
    require_cuda(cuCtxSynchronize(), "cuCtxSynchronize");
    require_cuda(
        cuMemcpyDtoH_v2(values, device_values, sizeof(values)),
        "cuMemcpyDtoH_v2"
    );

    if (memcmp(values, expected, sizeof(values)) != 0) {
        fprintf(
            stderr,
            "CUDA_COMPUTE_PROBE_FAILED result=%u,%u,%u,%u\n",
            values[0], values[1], values[2], values[3]
        );
        return EXIT_FAILURE;
    }

    printf(
        "CUDA_COMPUTE_PROBE_OK device=%s result=%u,%u,%u,%u\n",
        device_name,
        values[0], values[1], values[2], values[3]
    );
    require_cuda(cuMemFree_v2(device_values), "cuMemFree_v2");
    require_cuda(cuModuleUnload(module), "cuModuleUnload");
    require_cuda(cuCtxDestroy_v2(context), "cuCtxDestroy_v2");
    return EXIT_SUCCESS;
}
