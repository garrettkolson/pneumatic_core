// W1 test module — pure computation. Sums all input bytes (mod 2^32) and emits the
// low 4 bytes little-endian. No host imports. Deterministic: same input bytes => same
// output. See `compile.sh` for how the `.wasm` fixtures are built.
#![no_std]

extern "C" {
    static __heap_base: u8;
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
pub extern "C" fn execute(input_ptr: i32, input_len: i32, output_ptr: i32, _output_cap: i32) -> i32 {
    unsafe {
        let inp = input_ptr as *const u8;
        let mut sum: u32 = 0;
        let mut i = 0;
        while i < input_len {
            sum = sum.wrapping_add(*inp.add(i as usize) as u32);
            i += 1;
        }
        let out = output_ptr as *mut u8;
        *out = (sum & 0xff) as u8;
        *out.add(1) = ((sum >> 8) & 0xff) as u8;
        *out.add(2) = ((sum >> 16) & 0xff) as u8;
        *out.add(3) = ((sum >> 24) & 0xff) as u8;
        4
    }
}
