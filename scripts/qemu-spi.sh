#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "$0")" && pwd)"
image="${1:-$script_dir/../tmp/sdmmc-protocol-qemu-spi.img}"
if [ ! -f "$image" ]; then
  truncate -s 64M "$image"
fi
printf 'sdmmc-protocol-qemu-spi\n' | dd of="$image" bs=512 count=1 conv=notrunc status=none

cargo build --example qemu_spi --target riscv64gc-unknown-none-elf

log="$(mktemp)"
fifo="$(mktemp -u)"
mkfifo "$fifo"

timeout 30s qemu-system-riscv64 \
  -machine sifive_u \
  -cpu rv64 \
  -smp 2 \
  -m 128M \
  -nographic \
  -monitor none \
  -serial stdio \
  -bios none \
  -kernel target/riscv64gc-unknown-none-elf/debug/examples/qemu_spi \
  -drive if=sd,format=raw,file="$image" \
  >"$fifo" 2>&1 &
qemu_pid=$!

status=1
while IFS= read -r line; do
  printf '%s\n' "$line" | tee -a "$log"
  case "$line" in
    PASS)
      status=0
      kill "$qemu_pid" 2>/dev/null || true
      break
      ;;
  esac
done <"$fifo"

wait "$qemu_pid" 2>/dev/null || true

rm -f "$fifo" "$log"
exit "$status"
