// Internal-float test module — the exports (`__alloc`, `execute`) are integer-only,
// so the WasmEngine's ABI-boundary check (`validate_abi`) lets it through. But the
// `execute` *body* performs f32/f64 arithmetic, which the deploy-time contract
// scanner's opcode-level static walk rejects (WasmFloatOpcode) — closing the gap the
// ABI-boundary check alone misses. See `compile.sh` for how the `.wasm` is built.
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
pub extern "C" fn execute(input_ptr: i32, _input_len: i32, output_ptr: i32, _output_cap: i32) -> i32 {
    unsafe {
        // Read one u32 from the input and promote it to f32; do float math (the
        // internal float opcodes the scanner flags); write the low 4 bytes back.
        let inp = input_ptr as *const u32;
        let v = *inp as f32;
        let r = (v * v) + 1.0;
        let out = output_ptr as *mut u32;
        *out = r as u32;
        4
    }
}
