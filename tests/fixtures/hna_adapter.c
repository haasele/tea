/*
 * HNA v1 C adapter fixture (`hna_adapter`) for tea/tests/sidecar.rs (NB-6).
 *
 * A real Hydro Native ABI module built from source at test time:
 *
 *   - init()     sets a static `initialized` flag and returns 0,
 *   - state()    reports the flag as a Python bool,
 *   - shutdown() clears the flag and returns None.
 *
 * Compile-time variants:
 *   -DHNA_ADAPTER_INIT_FAIL=1  init returns 7 without setting an exception,
 *                              so the loader must reject the import and name
 *                              the module/provider,
 *   -DHNA_ADAPTER_NO_INIT=1    descriptor init is NULL (the runtime treats a
 *                              missing initializer as success; state() stays
 *                              False because the flag was never set).
 *
 * Only `hydro_native.h` is included: no CPython header or symbol, and the
 * shared library therefore never has a libpython dependency.
 */

#include "hydro_native.h"

#include <stdint.h>

#ifndef HNA_ADAPTER_INIT_FAIL
#define HNA_ADAPTER_INIT_FAIL 0
#endif

#ifndef HNA_ADAPTER_NO_INIT
#define HNA_ADAPTER_NO_INIT 0
#endif

static int32_t adapter_initialized = 0;

#if !HNA_ADAPTER_NO_INIT
static int32_t adapter_init(void) {
    adapter_initialized = 1;
#if HNA_ADAPTER_INIT_FAIL
    return 7;
#else
    return 0;
#endif
}
#endif

static HydroValue adapter_state(HydroValue self, uint64_t argc,
                                const HydroValue *argv) {
    (void)self;
    (void)argc;
    (void)argv;
    return adapter_initialized ? hydro_true() : hydro_false();
}

static HydroValue adapter_shutdown(HydroValue self, uint64_t argc,
                                   const HydroValue *argv) {
    (void)self;
    (void)argc;
    (void)argv;
    adapter_initialized = 0;
    return hydro_none();
}

static const HydroFunction adapter_functions[] = {
    {"state", 0u, 0u, 0u, NULL, adapter_state},
    {"shutdown", 0u, 0u, 0u, NULL, adapter_shutdown},
};

static const HydroNativeModule adapter_module = {
    sizeof(HydroNativeModule),
    HYDRO_HNA_VERSION,
    HYDRO_HNA_RUNTIME_ABI,
    "hna_adapter",
    "tea-hna-adapter-fixture 0.1.0",
    "linux-x86_64",
    HYDRO_HNA_FLAG_THREAD_SAFE,
#if HNA_ADAPTER_NO_INIT
    NULL,
#else
    adapter_init,
#endif
    2u,
    adapter_functions,
    0u,
    NULL,
};

uint32_t hydro_native_abi(void) {
    return HYDRO_HNA_VERSION;
}

uint32_t hydro_native_abi_build(void) {
    return 1u;
}

const HydroNativeModule *hydro_native_module_v1(void) {
    return &adapter_module;
}
