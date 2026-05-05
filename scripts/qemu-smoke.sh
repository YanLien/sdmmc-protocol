#!/usr/bin/env bash
set -euo pipefail

cargo build --example qemu_smoke --target riscv64gc-unknown-none-elf

qemu-system-riscv64 \
  -machine virt \
  -cpu rv64 \
  -smp 1 \
  -m 128M \
  -nographic \
  -monitor none \
  -serial stdio \
  -bios none \
  -kernel target/riscv64gc-unknown-none-elf/debug/examples/qemu_smoke
