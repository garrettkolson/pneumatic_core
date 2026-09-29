// Forbidden-import test module — imports `env.forbidden()`, which is NOT in the
// WasmEngine's allow-list. The engine's import validation must reject this module
// before instantiation (`ContractError::BadBytecode`), so it never runs.
#![no_std]

extern "C" {
    static __heap_base: u8;
    pub fn forbidden() -> i32;
}

static mut OFFSET: usize = 0;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn __alloc(_size: i32) -> i32 {
    unsafe { &__heap_base as *const u8 as i32 }
}

#[no_mangle]
pub extern "C" fn execute(_input_ptr: i32, _input_len: i32, output_ptr: i32, _output_cap: i32) -> i32 {
    unsafe {
        let _ = forbidden();
        *(output_ptr as *mut u8) = 0;
        1
    }
}
