/*
 * C-ABI adapter for the Go `hna_go` fixture.
 *
 * The static descriptor must be a C object: Go cannot place a Go function
 * pointer in a C static initializer. The descriptor's `add` entry therefore
 * points at `hna_go_add`, which validates the tagged operands and forwards the
 * untagged values to the //export'ed Go function `hydroGoAdd`.
 *
 * The three HNA entry points are //export'ed from hna_go.go; this file only
 * owns the descriptor and the adapter.
 */

#include "hydro_native.h"

#include <stdint.h>

/* Provided by the cgo-generated export wrapper for `hydroGoAdd`. */
extern long long hydroGoAdd(long long a, long long b);

#define HNA_GO_TYPE_ERROR 1u

static HydroValue hna_go_add(HydroValue self, uint64_t argc,
                             const HydroValue *argv) {
    static const uint8_t message[] = "add() requires int arguments";
    (void)self;
    if (argc != 2u || argv == NULL) {
        return hydro_raise_msg(HNA_GO_TYPE_ERROR, message,
                               sizeof(message) - 1u);
    }
    if ((argv[0] & 7) != 0 || (argv[1] & 7) != 0) {
        return hydro_raise_msg(HNA_GO_TYPE_ERROR, message,
                               sizeof(message) - 1u);
    }
    return hydroGoAdd(argv[0] >> 3, argv[1] >> 3);
}

static int32_t hna_go_init(void) {
    return 0;
}

static const HydroFunction hna_go_functions[] = {
    {"add", 2u, 2u, 0u, "a,b", hna_go_add},
};

static const HydroNativeModule hna_go_module = {
    sizeof(HydroNativeModule),
    HYDRO_HNA_VERSION,
    HYDRO_HNA_RUNTIME_ABI,
    "hna_go",
    "hydro-hna-fixture-go 0.1.0",
    "linux-x86_64",
    HYDRO_HNA_FLAG_THREAD_SAFE,
    hna_go_init,
    1u,
    hna_go_functions,
    0u,
    NULL,
};

const HydroNativeModule *hnaGoModule(void) {
    return &hna_go_module;
}
