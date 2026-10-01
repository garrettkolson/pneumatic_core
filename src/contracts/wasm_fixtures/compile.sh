#!/usr/bin/env bash
# Rebuild the WASM test-module fixtures used by the WasmEngine tests
# (src/contracts/wasm.rs). Requires the `wasm32-unknown-unknown` rust target
# (`rustup target add wasm32-unknown-unknown`). Output `.wasm` files are committed
# so tests run without a wasm toolchain; they are byte-stable for a given rustc.
set -euo pipefail
cd "$(dirname "$0")"
for src in src/*.rs; do
  name="$(basename "${src%.rs}")"
  # wasm_caller is hand-assembled (see generate_caller.py) — a rustc build is
  # ~552 KiB, too big for its own canonical ExecutionInput to fit in the module.
  [ "$name" = "wasm_caller" ] && continue
  rustc --target wasm32-unknown-unknown --edition 2021 --crate-type cdylib -O "$src" -o "${name}.wasm"
  echo "built ${name}.wasm"
done
python3 generate_caller.py
