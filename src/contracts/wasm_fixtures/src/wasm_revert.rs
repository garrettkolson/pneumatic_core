// Revert test module — calls the `env.revert()` host import, which the WasmEngine
// links as a function that traps with a "reverted" marker. The engine maps that trap
// to `ContractError::Reverted`.
#![no_std]

extern "C" {
    static __heap_base: u8;
    pub fn revert() -> i32;
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
pub extern "C" fn execute(_input_ptr: i32, _input_len: i32, _output_ptr: i32, _output_cap: i32) -> i32 {
    unsafe {
        revert();
        0
    }
}
