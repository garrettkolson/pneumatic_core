---
id: fact-wasmparser-read-var-i32-bug
title: "wasmparser 0.239.0 read_var_i32 corrupts single-byte 0x40..0x7F immediates"
type: fact
namespace: pneumatic
visibility: namespace
summary: "wasmparser 0.239.0's read_var_i32 single-byte fast path mis-decodes i32.const immediates of 0x40..0x7F (yields byte-128). Workaround for hand-assembled Wasm fixtures: encode every i32.const as 2-byte SLEB128 (sleb_force2) to force the correct slow path. Also: the wasmparser validator rejects popping a value at a control frame's baseline, and hand-assembled fixtures must omit the data section (section-id swap 11/12)."
auto_inject: false
applicable_when: "Hand-assembling Wasm fixtures for the WasmEngine, debugging wasmi/wasmparser BadBytecode, or adding a new Wasm test module"
confidence: 0.95
verified_at: "09/30/2026"
verified_by: "dsh-agent"
staleness_signal: "Verified against wasmparser 0.239.0 in the cargo registry 09/30/2026. Re-verify if wasmparser/wasmi is re-pinned to a version with a fixed read_var_i32 or a changed validator"
tags: [wasm, wasmparser, wasmi, fixture, bug, sleb128, determinism, contract-execution]
edges:
  - target: decision-wasm-engine-tier2
    type: related_to
    weight: 0.7
    note: "The WasmEngine consumes hand-assembled binary WASM fixtures; this bug shapes how those fixtures must be encoded"
  - target: decision-cross-contract-calls
    type: related_to
    weight: 0.6
    note: "The Wasm->Wasm cross-contract round-trip fixture (wasm_caller.wasm) depends on the sleb_force2 workaround"
related: ["[[src/contracts/wasm_fixtures/generate_caller.py]]"]
source_url: "Empty"
---

# wasmparser 0.239.0 `read_var_i32` corrupts single-byte 0x40..0x7F immediates

**Verified 2026-09-30** against `wasmparser 0.239.0` (`binary_reader.rs:555`) in the cargo
registry. `wasmi 1.1.0` uses wasmparser's validator, so this bites the `WasmEngine` directly.

**The bug.** `read_var_i32`'s single-byte fast path is `((byte as i32) << 25) >> 25`. For bytes
`0x40..0x7F` (the top bit of the 7-bit payload is set) that sign-extension yields `byte − 128`
instead of `byte`. So an `i32.const` immediate encoded as a single byte in `0x40..0x7F` (i.e.
values 64..127) decodes to a negative value — the module fails to validate with a
`BadBytecode` type mismatch deep in the body, not at the constant.

**Workaround (mandatory for hand-assembled fixtures).** Encode **every** `i32.const` immediate
as a **2-byte SLEB128** so the reader takes the correct slow path (`read_var_i32_big`,
`binary_reader.rs:565`). `sleb_force2(n)` for `0 ≤ n < 0x4000`: `b0 = (n & 0x7F) | 0x80`,
`b1 = (n >> 7) & 0x7F`. This is why `src/contracts/wasm_fixtures/generate_caller.py` uses
`sleb_force2` for all constants. The Wasm caller fixture **must stay hand-assembled** (a
rustc `no_std` caller is ≈ 552 KiB → > 644 KiB, over the wasmi ~512 KiB heap headroom).

**Two related validator constraints (same wasmparser version):**
- **`control.height` rule** (`validator/operators.rs:721`, `_pop_operand`): a value sitting
  **at** a control frame's baseline **cannot** be popped/dropped inside that frame (bails
  "type mismatch: expected {desc} but nothing on stack"). Fix: drop the value *before* the
  `if`, reconstruct via `local.get` in each branch (empty if-frame baseline).
- **`if` blocktype:** an `if` with an empty (`0x40`) blocktype rejects a branch that leaves a
  value ("values remaining on stack at end of block" at the `else`). If both branches yield one
  `i32`, the blocktype must be `0x7f`.

**Hand-assembled fixtures must omit the data section.** wasmparser 0.239.0 swapped the
section ids (`parser.rs:370-371`: `DATA_SECTION = 11`, `DATA_COUNT_SECTION = 12`); a data
section in a fixture is misread. Seed memory with an `i32.store` preamble instead. With an
import present, exported function indices are offset by the import count
(`env.call` = global 0 → `execute` = 1, `__alloc` = 2).
