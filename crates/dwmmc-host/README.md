# dwmmc-host

`no_std` Synopsys DesignWare Mobile Storage Host Controller (DW_mshc)
backend for the [`sdmmc-protocol`](../..) driver crate.

This crate plugs the IP block known as `DWC_mobile_storage` / `dw_mshc` /
`dw_mmc` (Linux) into the `SdioHost` trait so
`sdmmc_protocol::sdio::SdioSdmmc` can drive real hardware. The same core
appears in Rockchip RK33xx/RK35xx, Allwinner A-series, StarFive JH7110,
and a long tail of mid-range SoCs.

PIO data path only — the controller's internal DMAC (IDMAC) is left
disabled.

## Status

- ✅ Compiles for `riscv64gc-unknown-none-elf`, `aarch64-unknown-none`, host
- ⚠️ **Not yet tested on real hardware**. Treat the capabilities below
  as the intent, not a guarantee.

## Scope

| Area                | Implemented |
|---------------------|-------------|
| PIO read / write (FIFO) | ✅      |
| 1-bit / 4-bit / 8-bit bus | ✅    |
| Default speed       | ✅          |
| High Speed (50 MHz) | ✅          |
| 32-bit / 136-bit responses | ✅   |
| R3 / R4 (no CRC) responses | ✅   |
| Software reset / clock setup | ✅ |
| Configurable FIFO offset | ✅     |
| IDMAC / internal DMA | ❌         |
| HS200 / SDR50 / SDR104 / DDR50 | ❌ |
| 1.8 V signaling switch (CMD11) | ❌ |
| Tuning (CMD19 / CMD21) | ❌      |

## Usage

```rust,no_run
use embedded_hal::delay::DelayNs;
use sdmmc_protocol::sdio::SdioSdmmc;
use dwmmc_host::DwMmc;

# fn make_delay() -> impl DelayNs { struct N; impl DelayNs for N { fn delay_ns(&mut self, _: u32) {} } N }
// SAFETY: 0xFE2B_0000 must point at a valid DW_mshc register file the
// caller has exclusive access to.
let mut host = unsafe { DwMmc::new(0xFE2B_0000) };
host.set_reference_clock(50_000_000);
host.reset_and_init().expect("controller reset");

let mut card = SdioSdmmc::new(host, make_delay());
// card.init()?;
```

### Bring-up checklist (for real-hardware validation)

1. Map the DW_mshc register file (e.g. RK3568 SDMMC0 at `0xFE2B_0000`).
2. Configure the platform clock so the controller has a viable
   reference clock before calling `DwMmc::new`. Most SoCs route a
   selectable mux through the CRU; pick a rate that divides cleanly
   to 400 kHz for ID mode.
3. Pass that rate to `DwMmc::set_reference_clock` so the divider
   programmed by `set_clock` lands on the right frequency.
4. `host.reset_and_init()?` — clears the controller / FIFO / DMA
   state and arms a 400 kHz ID-mode clock.
5. Build `SdioSdmmc::new(host, delay)` and call `init()`. The
   protocol layer will ramp the clock up via `set_clock`.

### FIFO offset

The data FIFO sits at a fixed offset that varies by IP revision /
integration:

- `0x100`: very old DWC_mobile_storage builds.
- `0x200` (default): Rockchip RK33xx/RK35xx, StarFive JH7110.
- `0x400`: some Allwinner integrations.

Use `DwMmc::new_with_fifo_offset` if your SoC differs from the default.

## License

Dual-licensed under MIT or Apache-2.0, same as the parent crate.
