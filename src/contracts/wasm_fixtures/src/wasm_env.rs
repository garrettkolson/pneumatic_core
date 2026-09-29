// W2 test module — calls the read-only host function `env.tx_amount()` and emits the
// result as a little-endian i64. Exercises the host-import (capability) path.
#![no_std]

extern "C" {
    static __heap_base: u8;
    pub fn tx_amount() -> i64;
}

static mut OFFSET: usize = 0;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn __alloc(size: i32) -> i32 {
    unsafe {
        let base = &__heap_base as *const u8 as usize;
        let start = base + OFFSET;
        OFFSET += size as usize;
        start as i32
    }
}

#[no_mangle]
pub extern "C" fn execute(_input_ptr: i32, _input_len: i32, output_ptr: i32, _output_cap: i32) -> i32 {
    unsafe {
        let amt = tx_amount();
        let out = output_ptr as *mut u8;
        for i in 0..8 {
            *out.add(i) = (((amt as u64) >> (8 * i as u64)) & 0xff) as u8;
        }
        8
    }
}
