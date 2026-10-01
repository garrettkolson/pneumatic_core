#!/usr/bin/env python3
"""Hand-assemble `wasm_caller.wasm` — the Model X Wasm caller fixture (ADR-016).

Why hand-assembled instead of rustc: a rustc no_std module is ~552 KiB, so the
canonical `ExecutionInput` (which embeds the module's own bytecode) is ~644 KiB.
rustc's wasm linker grants the guest only ~512 KiB of heap headroom above
`__heap_base`, so the host's input buffer can never fit in a rustc-built caller
(no pad size works — the headroom is a constant while the input scales with
the module size). A ~0.4 KiB hand-assembled caller keeps the input at ~3 KiB,
which fits a 4-page module with room to spare.

The module:
  * imports `env.call` (11 x i32 -> i32)
  * exports `execute(ptr, len, out_ptr, out_cap) -> i32` and `__alloc(size) -> i32`
  * `execute` calls `env.call` with static arguments — target token `0xBB`,
    entry `"execute"`, payload `[1, 2, 3, 4]`, and the deterministic B-chain
    genesis snapshot ref (baked below, matching `det_block` in the tests) —
    passing its own output buffer straight through, so the callee's result
    lands in the output buffer; it returns the result length, or stores a
    single `0` byte and returns 1 when the call fails (result 0).

The output is deterministic: the same bytes on every run and every host.
"""

import os

# Deterministic B-chain genesis hash (fixed timestamp 1_700_000_000 + the
# canonical test signed transaction; see `det_block` in the engine tests).
REF_HASH = bytes(
    [
        0xDB, 0xE3, 0x1E, 0x79, 0x02, 0x0E, 0xBF, 0x0F, 0x1B, 0xBE, 0xE9, 0xBD,
        0xBA, 0x23, 0x58, 0x04, 0xA8, 0x36, 0x3D, 0x6A, 0x4D, 0x8A, 0xD6, 0x63,
        0x4A, 0xAD, 0x04, 0x63, 0x3D, 0xBE, 0xA8, 0xE8,
    ]
)

TARGET = b"\xBB"
ENTRY = b"execute"
PAYLOAD = bytes([1, 2, 3, 4])
OUT_BUF = 0x30  # 64-byte scratch buffer for the callee's result


def leb(n: int) -> bytes:
    out = b""
    while True:
        b7 = n & 0x7F
        n >>= 7
        out += bytes([b7 | 0x80]) if n else bytes([b7])
        if not n:
            break
    return out


def sleb_force2(n: int) -> bytes:
    """Two-byte SLEB128 for `i32.const` immediates (0 <= n < 0x4000).

    wasmparser 0.239.0 — the parser wasmi 1.1.0 reads operators with — has a
    bug in `read_var_i32`'s single-byte fast path: for a byte in 0x40..0x7F,
    `((byte as i32) << 25) >> 25` overflows the sign bit and the arithmetic
    shift yields `byte - 128` (e.g. 0x65 -> -27 -> low byte 0xE5). Encoding
    the immediate with the continuation bit set (two bytes) routes the decoder
    through the correct slow path (`read_var_i32_big`). Verified empirically:
    a minimal module storing `i32.const 0x65` lands 0xE5 in guest memory.
    """
    b0 = (n & 0x7F) | 0x80
    b1 = (n >> 7) & 0x7F
    return bytes([b0, b1])


def section(id_: int, payload: bytes) -> bytes:
    return bytes([id_]) + leb(len(payload)) + payload


# --- Type section -------------------------------------------------------------
# Function type: 0x60, vec(params), vec(results) — each vec is LEB-counted.
t_call = b"\x60" + leb(11) + b"\x7F" * 11 + leb(1) + b"\x7F"  # 11 x i32 -> i32
t_exec = b"\x60" + leb(4) + b"\x7F" * 4 + leb(1) + b"\x7F"  # 4 x i32 -> i32
t_alloc = b"\x60" + leb(1) + b"\x7F" + leb(1) + b"\x7F"  # i32 -> i32
sec_type = section(1, leb(3) + t_call + t_exec + t_alloc)

# --- Import section: env.call -------------------------------------------------
imp = b"\x03" + b"env" + b"\x04" + b"call" + b"\x00" + b"\x00"
sec_import = section(2, leb(1) + imp)

# --- Function section: execute (type 1), __alloc (type 2) ---------------------
# type 0 = t_call (the import), so: func 0 = execute -> type 1 (t_exec);
# func 1 = __alloc -> type 2 (t_alloc).
sec_func = section(3, leb(2) + b"\x01\x02")

# --- Memory section: 4 pages (256 KiB) ----------------------------------------
sec_mem = section(5, b"\x01\x00\x04")

# --- Global section: heap pointer, starting above the data region (0x70) ------
sec_global = section(6, b"\x01" + b"\x7F\x01" + b"\x41" + sleb_force2(0x100) + b"\x0B")


# --- Export section -------------------------------------------------------------
def export(name: bytes, kind: int, idx: int) -> bytes:
    return bytes([len(name)]) + name + bytes([kind]) + bytes([idx])


# The imported `env.call` occupies function index 0, so the locals are:
# execute = index 1, __alloc = index 2.
sec_export = section(
    7,
    leb(3)
    + export(b"execute", 0, 1)
    + export(b"__alloc", 0, 2)
    + export(b"memory", 2, 0),
)

# --- Code section ----------------------------------------------------------------
# execute(in_ptr=0, in_len=1, out_ptr=2, out_cap=3; local r=4)
#
# The callee's result is written directly into the caller's output buffer by
# `env.call` (out_ptr/out_cap are passed straight through), so `execute` just
# forwards the call's result length — or stores a single 0 byte and returns 1
# when the call fails (result 0).
body = b""
# Seed the argument bytes into guest memory (no data section: wasmparser
# 0.239 — used by wasmi 1.1.0 — swaps the spec section ids 11/12 for
# data/datacount, so a spec-compliant data section fails validation).
for off, val in (
    [(0x00, 0xBB)]
    + list(enumerate(ENTRY, start=0x01))
    + list(enumerate(PAYLOAD, start=0x08))
    + list(enumerate(REF_HASH, start=0x0C))
):
    body += b"\x41" + sleb_force2(off)
    body += b"\x41" + sleb_force2(val)
    body += b"\x36\x02\x00"  # i32.store (align=2, offset=0)
# push the 11 call arguments: target, entry, payload, ref, out_ptr, out_cap
for a in (0x00, 0x01, 0x01, 0x07, 0x08, 0x04, 0x00, 0x0C, 0x20):
    body += b"\x41" + sleb_force2(a)
body += b"\x20\x02"  # out_ptr
body += b"\x20\x03"  # out_cap
body += b"\x10\x00"  # call $call
body += b"\x22\x04"  # local.tee $r
body += b"\x1A"  # drop r (reconstruct via local.get so the if-frame baseline is empty)
body += b"\x20\x04"  # local.get $r
body += b"\x41\x00"
body += b"\x4A"  # i32.gt_s  (r > 0?)
body += b"\x04\x7f"  # if (blocktype i32: both branches yield one i32)
body += b"\x20\x04"  #   success: push r (the result length)
body += b"\x05"  # else (failure: r == 0)
body += b"\x20\x02"  #   out_ptr
body += b"\x41\x00"
body += b"\x3A\x00\x00"  #   i32.store8 (align=0, offset=0) — write the 0 byte
body += b"\x41\x01"
body += b"\x0B"  # end if
body += b"\x0B"  # end function

# __alloc(size=0): heap += size; return new heap
abody = b""
abody += b"\x23\x00"  # global.get $heap
abody += b"\x20\x00"  # local.get $sz
abody += b"\x6A"  # +
abody += b"\x24\x00"  # global.set $heap
abody += b"\x23\x00"  # global.get $heap (result)
abody += b"\x0B"  # end

# The body size includes the locals-declaration header (spec: the body vector
# is locals + instruction stream).
exec_body = b"\x01" + b"\x01\x7F" + body
alloc_body = b"\x00" + abody
codes = leb(2)
codes += leb(len(exec_body)) + exec_body
codes += leb(len(alloc_body)) + alloc_body
sec_code = section(10, codes)


# --- Data section ----------------------------------------------------------------
def seg(off: int, data: bytes) -> bytes:
    return b"\x00" + b"\x41" + leb(off) + bytes([len(data)]) + data


sec_data = section(
    12,
    leb(4)
    + seg(0x00, TARGET)
    + seg(0x01, ENTRY)
    + seg(0x08, PAYLOAD)
    + seg(0x0C, REF_HASH),
)

wasm = (
    b"\x00asm\x01\x00\x00\x00"
    + sec_type
    + sec_import
    + sec_func
    + sec_mem
    + sec_global
    + sec_export
    + sec_code
    # No data section: wasmparser 0.239 (used by wasmi 1.1.0) assigns the
    # spec's data section id 12 to *datacount*, so a spec-compliant data
    # section fails validation. The argument bytes are seeded into guest
    # memory with i32.store instructions in `execute` instead.
)

here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "wasm_caller.wasm"), "wb") as f:
    f.write(wasm)
print(f"built wasm_caller.wasm ({len(wasm)} bytes)")
