// FP-disallow test module — exports `fp_echo(f32) -> f32`. The WasmEngine rejects any
// module whose ABI boundary uses f32/f64 (integer-only), so this module is rejected
// before instantiation (ContractError::BadBytecode).
#![no_std]

extern "C" {
    static __heap_base: u8;
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn __alloc(_size: i32) -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn execute(_a: i32, _b: i32, _c: i32, _d: i32) -> i32 {
    0
}

/// An exported function with an f32 param/result — triggers the FP-disallow check.
#[no_mangle]
pub extern "C" fn fp_echo(x: f32) -> f32 {
    x
}
