# sdhci-host

`no_std` SD Host Controller (SDHCI v3.x) backend for the
[`sdmmc-protocol`](../..) driver crate.

This crate plugs an SDHCI register-level driver into the
`SdioHost` trait so `sdmmc_protocol::sdio::SdioSdmmc` can drive
real hardware. PIO data path only.

## Status

- ✅ Compiles for `riscv64gc-unknown-none-elf`, `aarch64-unknown-none`, host
- ✅ `cargo clippy --all-features -- -D warnings`
- ✅ `cargo doc --no-deps --all-features` (warnings as errors)
- ⚠️ **Not yet tested on real hardware**. The author will validate on a
  Firefly ROC-RK3568-PC. Until that is done, treat the published
  capabilities below as the intent, not a guarantee.

## Scope

| Area                | Implemented |
|---------------------|-------------|
| PIO read / write    | ✅          |
| 1-bit / 4-bit bus   | ✅          |
| Default speed       | ✅          |
| High Speed (50 MHz) | ✅          |
| 32-bit / 136-bit responses | ✅   |
| Software reset / clock setup | ✅ |
| DMA / ADMA2         | ❌          |
| 8-bit eMMC bus      | ❌ (returns `UnsupportedCommand`) |
| HS200 / SDR50 / SDR104 | ❌      |
| 1.8 V signaling switch (CMD11) | ❌ |
| Tuning (CMD19 / CMD21) | ❌      |
| eMMC EXT_CSD path   | ❌          |

## Usage

```rust,no_run
use embedded_hal::delay::DelayNs;
use sdmmc_protocol::sdio::SdioSdmmc;
use sdhci_host::Sdhci;

# fn make_delay() -> impl DelayNs { struct N; impl DelayNs for N { fn delay_ns(&mut self, _: u32) {} } N }
// SAFETY: 0xFE31_0000 must point at a valid SDHCI register file the
// caller has exclusive access to.
let host = unsafe { Sdhci::new(0xFE31_0000) };
let delay = make_delay();
let mut card = SdioSdmmc::new(host, delay);
// card.init()?;
```

### Bring-up checklist (for real-hardware validation)

1. Map the SDHCI register file (RK3568: `0xFE31_0000`).
2. Configure the platform clock so the controller has a viable reference
   clock before calling `Sdhci::new` (RK3568 needs the CRU bringing
   `CLK_EMMC_CORE` up at ≥ 25 MHz).
3. `host.reset_all()?` — clears CMD/DAT inhibits and the interrupt
   registers.
4. `host.set_power(POWER_330)` (or whatever your card needs).
5. `host.enable_interrupts()` — enables status flags. The driver polls;
   it does NOT enable signal-level IRQ delivery.
6. `host.enable_clock(base_hz, 400_000)` — start at 400 kHz for
   identification.
7. Build `SdioSdmmc::new(host, delay)` and call `init()`. The driver
   will ramp the clock up to 25 MHz / 50 MHz via `set_clock` for you.

## License

Dual-licensed under MIT or Apache-2.0, same as the parent crate.
