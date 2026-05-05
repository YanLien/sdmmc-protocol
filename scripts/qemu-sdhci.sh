#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
image="${1:-$script_dir/../tmp/sdmmc-protocol-qemu-sd.img}"
if [ ! -f "$image" ]; then
  truncate -s 64M "$image"
fi
printf 'sdmmc-protocol-qemu-sdhci\n' | dd of="$image" bs=512 count=1 conv=notrunc status=none

cargo build --example qemu_sdhci --target riscv64gc-unknown-none-elf --no-default-features --features sdio

qemu-system-riscv64 \
  -machine virt \
  -cpu rv64 \
  -smp 1 \
  -m 128M \
  -nographic \
  -monitor none \
  -serial stdio \
  -bios none \
  -kernel target/riscv64gc-unknown-none-elf/debug/examples/qemu_sdhci \
  -drive id=sdcard,if=none,format=raw,file="$image" \
  -device sdhci-pci \
  -device sd-card,drive=sdcard
