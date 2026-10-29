/*
 * HNA v1 C fixture (`hna_c`) for tea/tests/hna.rs.
 *
 * Exports the three HNA entry points and a static descriptor with:
 *   - mul(a, b)      -> a * b through hydro_binop,
 *   - tag()          -> the string "C",
 *   - constant C     -> the string "C",
 *   - init()         -> 0.
 *
 * The same source is rebuilt with -D flags to produce the rejection fixtures:
 *   -DHNA_FIXTURE_HNA_VERSION=2          wrong major version
 *   -DHNA_FIXTURE_RUNTIME_ABI_MIN=2      unsupported runtime ABI
 *   -DHNA_FIXTURE_PLATFORM="linux-aarch64"
 *   -DHNA_FIXTURE_MODULE_NAME="hna_wrong"
 *   -DHNA_FIXTURE_ABORT=1                init aborts (SIGABRT) instead of 0
 *
 * No CPython header or symbol is used.
 */

#include "hydro_native.h"

#include <stdint.h>
#include <stdlib.h>

#ifndef HNA_FIXTURE_MODULE_NAME
#define HNA_FIXTURE_MODULE_NAME "hna_c"
#endif

#ifndef HNA_FIXTURE_PLATFORM
#define HNA_FIXTURE_PLATFORM "linux-x86_64"
#endif

#ifndef HNA_FIXTURE_HNA_VERSION
#define HNA_FIXTURE_HNA_VERSION HYDRO_HNA_VERSION
#endif

#ifndef HNA_FIXTURE_RUNTIME_ABI_MIN
#define HNA_FIXTURE_RUNTIME_ABI_MIN HYDRO_HNA_RUNTIME_ABI
#endif

#ifndef HNA_FIXTURE_FLAGS
#define HNA_FIXTURE_FLAGS HYDRO_HNA_FLAG_THREAD_SAFE
#endif

#define HNA_FIXTURE_TYPE_ERROR 1u
#define HNA_FIXTURE_OP_MUL 2u

static HydroValue fixture_mul(HydroValue self, uint64_t argc,
                              const HydroValue *argv) {
    static const uint8_t message[] = "mul() requires int arguments";
    (void)self;
    if (argc != 2u || argv == NULL) {
        return hydro_raise_msg(HNA_FIXTURE_TYPE_ERROR, message,
                               sizeof(message) - 1u);
    }
    return hydro_binop(HNA_FIXTURE_OP_MUL, argv[0], argv[1]);
}

static HydroValue fixture_tag(HydroValue self, uint64_t argc,
                              const HydroValue *argv) {
    static const uint8_t tag[] = "C";
    (void)self;
    (void)argc;
    (void)argv;
    return hydro_str_new(tag, 1u);
}

static HydroValue fixture_tag_value(void) {
    static const uint8_t tag[] = "C";
    return hydro_str_new(tag, 1u);
}

static int32_t fixture_init(void) {
#ifdef HNA_FIXTURE_ABORT
    abort();
#else
    return 0;
#endif
}

static const HydroFunction fixture_functions[] = {
    {"mul", 2u, 2u, 0u, "a,b", fixture_mul},
    {"tag", 0u, 0u, 0u, NULL, fixture_tag},
};

static const HydroConstant fixture_constants[] = {
    {"C", fixture_tag_value},
};

static const HydroNativeModule fixture_module = {
    sizeof(HydroNativeModule),
    HNA_FIXTURE_HNA_VERSION,
    HNA_FIXTURE_RUNTIME_ABI_MIN,
    HNA_FIXTURE_MODULE_NAME,
    "hydro-hna-fixture-c 0.1.0",
    HNA_FIXTURE_PLATFORM,
    HNA_FIXTURE_FLAGS,
    fixture_init,
    2u,
    fixture_functions,
    1u,
    fixture_constants,
};

uint32_t hydro_native_abi(void) {
    return HYDRO_HNA_VERSION;
}

uint32_t hydro_native_abi_build(void) {
    return 1u;
}

const HydroNativeModule *hydro_native_module_v1(void) {
    return &fixture_module;
}
