// Fuel-exhaustion test module — `execute` loops forever, incrementing a counter (kept
// live with `black_box` so `-O` can't eliminate it), burning WASM fuel until the
// engine's fuel budget (`gas_limit`) is exhausted → ContractError::GasExhausted.
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
    let mut i: i32 = 0;
    loop {
        i = i.wrapping_add(1);
        core::hint::black_box(i);
    }
}
