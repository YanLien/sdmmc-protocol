RUSTC_TARGET_RV64 := riscv64gc-unknown-none-elf
IMAGE_SDHCI := tmp/sdmmc-protocol-qemu-sd.img
IMAGE_SPI := tmp/sdmmc-protocol-qemu-spi.img

.PHONY: check build test \
        qemu-smoke qemu-sdhci qemu-spi qemu qemu-test \
        clean

# ── Host ──────────────────────────────────────────────────────

check:
	cargo check

build:
	cargo build

test:
	cargo test

# ── QEMU (RISC-V bare-metal) ─────────────────────────────────

qemu-smoke:
	cargo build --example qemu_smoke --target $(RUSTC_TARGET_RV64)
	qemu-system-riscv64 \
	  -machine virt -cpu rv64 -smp 1 -m 128M \
	  -nographic -monitor none -serial stdio \
	  -bios none \
	  -kernel target/$(RUSTC_TARGET_RV64)/debug/examples/qemu_smoke

qemu-sdhci: $(IMAGE_SDHCI)
	cargo build --example qemu_sdhci --target $(RUSTC_TARGET_RV64) \
	  --no-default-features --features sdio
	qemu-system-riscv64 \
	  -machine virt -cpu rv64 -smp 1 -m 128M \
	  -nographic -monitor none -serial stdio \
	  -bios none \
	  -kernel target/$(RUSTC_TARGET_RV64)/debug/examples/qemu_sdhci \
	  -drive id=sdcard,if=none,format=raw,file=$(IMAGE_SDHCI) \
	  -device sdhci-pci \
	  -device sd-card,drive=sdcard

qemu-spi: $(IMAGE_SPI)
	cargo build --example qemu_spi --target $(RUSTC_TARGET_RV64)
	qemu-system-riscv64 \
	  -machine sifive_u -cpu rv64 -smp 2 -m 128M \
	  -nographic -monitor none -serial stdio \
	  -bios none \
	  -kernel target/$(RUSTC_TARGET_RV64)/debug/examples/qemu_spi \
	  -drive if=sd,format=raw,file=$(IMAGE_SPI)

$(IMAGE_SDHCI):
	mkdir -p tmp
	truncate -s 64M $(IMAGE_SDHCI)
	printf 'sdmmc-protocol-qemu-sdhci\n' | dd of=$(IMAGE_SDHCI) bs=512 count=1 conv=notrunc status=none

$(IMAGE_SPI):
	mkdir -p tmp
	truncate -s 64M $(IMAGE_SPI)
	printf 'sdmmc-protocol-qemu-spi\n' | dd of=$(IMAGE_SPI) bs=512 count=1 conv=notrunc status=none

qemu: qemu-smoke qemu-sdhci qemu-spi

# Aggregate harness: run smoke + sdhci + spi back-to-back, capture each output, and
# print a final pass/fail summary. Individual targets above remain available
# for focused debugging.
qemu-test: $(IMAGE_SDHCI) $(IMAGE_SPI)
	@mkdir -p tmp
	@cargo build --example qemu_smoke --target $(RUSTC_TARGET_RV64) >/dev/null
	@cargo build --example qemu_sdhci --target $(RUSTC_TARGET_RV64) \
	  --no-default-features --features sdio >/dev/null
	@cargo build --example qemu_spi --target $(RUSTC_TARGET_RV64) >/dev/null
	@echo "── qemu_smoke ────────────────────────────────────────────"
	@qemu-system-riscv64 \
	  -machine virt -cpu rv64 -smp 1 -m 128M \
	  -nographic -monitor none -serial stdio \
	  -bios none \
	  -kernel target/$(RUSTC_TARGET_RV64)/debug/examples/qemu_smoke \
	  | tee tmp/qemu-smoke.log
	@echo "── qemu_sdhci ────────────────────────────────────────────"
	@qemu-system-riscv64 \
	  -machine virt -cpu rv64 -smp 1 -m 128M \
	  -nographic -monitor none -serial stdio \
	  -bios none \
	  -kernel target/$(RUSTC_TARGET_RV64)/debug/examples/qemu_sdhci \
	  -drive id=sdcard,if=none,format=raw,file=$(IMAGE_SDHCI) \
	  -device sdhci-pci \
	  -device sd-card,drive=sdcard \
	  | tee tmp/qemu-sdhci.log
	@echo "── qemu_spi ──────────────────────────────────────────────"
	@scripts/qemu-spi.sh $(IMAGE_SPI) | tee tmp/qemu-spi.log
	@echo "── summary ───────────────────────────────────────────────"
	@if grep -q '^PASS$$' tmp/qemu-smoke.log && grep -q '^PASS$$' tmp/qemu-sdhci.log && grep -q '^PASS$$' tmp/qemu-spi.log; then \
	  echo "qemu-test: ALL PASS"; \
	else \
	  echo "qemu-test: FAIL"; \
	  exit 1; \
	fi

# ── Cleanup ───────────────────────────────────────────────────

clean:
	cargo clean
	rm -f $(IMAGE_SDHCI)
	rm -f $(IMAGE_SPI)
	rm -f tmp/qemu-*.log
