// W3 test module — exercises the storage tier (`sload`/`sstore`/`sdelete`).
// Performs a deterministic sequence against one key `"a"`:
//   1. sstore "a" = "hello"          (new key)
//   2. sload  "a"                     -> "hello" (5 bytes)
//   3. sstore "a" = "hi"             (rewrite)
//   4. sload  "a"                     -> "hi" (2 bytes, read-your-writes)
//   5. sdelete "a"                    (tombstone)
//   6. sload  "a"                     -> "" (0 bytes, deleted)
// Emits the three loaded values length-prefixed: [5,h,e,l,l,o,2,h,i,0] (10 bytes).
// The host records the write-set; the final canonical `storage_delta` is `{"a": None}`
// (the last op, the tombstone). See `compile.sh` for how the `.wasm` is built.
#![no_std]

extern "C" {
    static __heap_base: u8;
    pub fn sload(key_ptr: i32, key_len: i32, out_ptr: i32) -> i32;
    pub fn sstore(key_ptr: i32, key_len: i32, value_ptr: i32, value_len: i32);
    pub fn sdelete(key_ptr: i32, key_len: i32);
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

/// Write a byte slice into a freshly `__alloc`'d module buffer; returns (ptr, len).
unsafe fn alloc_bytes(bytes: &[u8]) -> (i32, i32) {
    let ptr = __alloc(bytes.len() as i32);
    let dst = ptr as *mut u8;
    for (i, b) in bytes.iter().enumerate() {
        *dst.add(i) = *b;
    }
    (ptr, bytes.len() as i32)
}

#[no_mangle]
pub extern "C" fn execute(_input_ptr: i32, _input_len: i32, output_ptr: i32, _output_cap: i32) -> i32 {
    unsafe {
        let key_a: &[u8] = b"a";
        let val_hello: &[u8] = b"hello";
        let val_hi: &[u8] = b"hi";

        // 1. store "a" = "hello" (new key).
        let (kp1, kl1) = alloc_bytes(key_a);
        let (vp1, vl1) = alloc_bytes(val_hello);
        sstore(kp1, kl1, vp1, vl1);

        // 2. load "a" -> "hello" (5 bytes).
        let (kp2, kl2) = alloc_bytes(key_a);
        let out1 = __alloc(64);
        let len1 = sload(kp2, kl2, out1);

        // 3. store "a" = "hi" (rewrite).
        let (kp3, kl3) = alloc_bytes(key_a);
        let (vp3, vl3) = alloc_bytes(val_hi);
        sstore(kp3, kl3, vp3, vl3);

        // 4. load "a" -> "hi" (2 bytes, read-your-writes).
        let (kp4, kl4) = alloc_bytes(key_a);
        let out2 = __alloc(64);
        let len2 = sload(kp4, kl4, out2);

        // 5. delete "a" (tombstone).
        let (kp5, kl5) = alloc_bytes(key_a);
        sdelete(kp5, kl5);

        // 6. load "a" -> "" (0 bytes, deleted).
        let (kp6, kl6) = alloc_bytes(key_a);
        let out3 = __alloc(64);
        let len3 = sload(kp6, kl6, out3);

        // Emit the three loaded values, length-prefixed.
        let out = output_ptr as *mut u8;
        let mut o: usize = 0;
        *out.add(o) = len1 as u8;
        o += 1;
        for i in 0..len1 {
            *out.add(o) = *(out1 as *const u8).add(i as usize);
            o += 1;
        }
        *out.add(o) = len2 as u8;
        o += 1;
        for i in 0..len2 {
            *out.add(o) = *(out2 as *const u8).add(i as usize);
            o += 1;
        }
        *out.add(o) = len3 as u8;
        o += 1;
        for i in 0..len3 {
            *out.add(o) = *(out3 as *const u8).add(i as usize);
            o += 1;
        }
        o as i32
    }
}
