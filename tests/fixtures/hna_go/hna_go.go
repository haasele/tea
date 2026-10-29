// HNA v1 Go c-shared fixture (`hna_go`).
//
// Go cannot name a Go function in a C static initializer, so the descriptor is
// built in C and the call target is a C adapter that calls the //export'ed Go
// function. The three HNA entry points are //export'ed from Go so a
// `-buildmode=c-shared` image always publishes them; the module pointer comes
// from a C helper that returns the static descriptor.
package main

/*
#cgo CFLAGS: -I${SRCDIR}/../../../../hydro/include
#include <stdint.h>

typedef struct HydroNativeModule HydroNativeModule;
extern const HydroNativeModule *hnaGoModule(void);
*/
import "C"

func main() {}

//export hydro_native_abi
func hydro_native_abi() uint32 {
	return 1
}

//export hydro_native_abi_build
func hydro_native_abi_build() uint32 {
	return 1
}

//export hydro_native_module_v1
func hydro_native_module_v1() *C.HydroNativeModule {
	return C.hnaGoModule()
}

//export hydroGoAdd
func hydroGoAdd(a int64, b int64) int64 {
	return (a + b) << 3
}
