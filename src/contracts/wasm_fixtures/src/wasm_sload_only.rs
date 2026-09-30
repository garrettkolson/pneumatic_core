// W3 base-state read test module — `sload`s a single key (from the contract's base
// state, passed via `ExecutionInput::storage`) and emits the value. Verifies `sload`
// reads the base state, not just the write-set (read-your-writes).
#![no_std]

extern "C" {
    static __heap_base: u8;
    pub fn sload(key_ptr: i32, key_len: i32, out_ptr: i32) -> i32;
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
        let key: &[u8] = b"existing";
        let key_ptr = __alloc(key.len() as i32);
        for (i, b) in key.iter().enumerate() {
            *(key_ptr as *mut u8).add(i) = *b;
        }
        let out = __alloc(64);
        let len = sload(key_ptr, key.len() as i32, out);
        let o = output_ptr as *mut u8;
        for i in 0..len {
            *o.add(i as usize) = *(out as *const u8).add(i as usize);
        }
        len
    }
}
