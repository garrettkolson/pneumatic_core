#!/usr/bin/env bash
# Rebuild the WASM test-module fixtures used by the WasmEngine tests
# (src/contracts/wasm.rs). Requires the `wasm32-unknown-unknown` rust target
# (`rustup target add wasm32-unknown-unknown`). Output `.wasm` files are committed
# so tests run without a wasm toolchain; they are byte-stable for a given rustc.
set -euo pipefail
cd "$(dirname "$0")"
for src in src/*.rs; do
  name="$(basename "${src%.rs}")"
  rustc --target wasm32-unknown-unknown --edition 2021 --crate-type cdylib -O "$src" -o "${name}.wasm"
  echo "built ${name}.wasm"
done
