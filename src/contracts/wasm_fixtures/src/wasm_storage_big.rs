// W3 cap test module — stores a single 1.5 MiB value under key "big", exceeding the
// 1 MiB per-contract storage cap. The engine must revert the call so no oversized
// state is committed. The `.wasm` is tiny (a fill loop); only the *runtime* linear
// memory is large.
#![no_std]

extern "C" {
    static __heap_base: u8;
    pub fn sstore(key_ptr: i32, key_len: i32, value_ptr: i32, value_len: i32);
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
pub extern "C" fn execute(_input_ptr: i32, _input_len: i32, _output_ptr: i32, _output_cap: i32) -> i32 {
    unsafe {
        let val_len: usize = 1536 * 1024; // 1.5 MiB
        let key: &[u8] = b"big";
        let key_ptr = __alloc(key.len() as i32);
        for (i, b) in key.iter().enumerate() {
            *(key_ptr as *mut u8).add(i) = *b;
        }
        let val_ptr = __alloc(val_len as i32);
        for i in 0..val_len {
            *(val_ptr as *mut u8).add(i) = 0x42;
        }
        sstore(key_ptr, key.len() as i32, val_ptr, val_len as i32);
        0
    }
}
