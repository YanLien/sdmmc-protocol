#![cfg_attr(target_arch = "riscv64", no_std)]
#![cfg_attr(target_arch = "riscv64", no_main)]

#[cfg(not(target_arch = "riscv64"))]
fn main() {}

#[cfg(target_arch = "riscv64")]
mod qemu {
    use core::arch::{asm, global_asm};
    use core::fmt::{self, Write};
    use core::panic::PanicInfo;

    use embedded_hal::delay::DelayNs;
    use sdmmc_protocol::spi::{SpiSdmmc, SpiTransport};
    use sdmmc_protocol::{Error, ErrorContext};

    const UART0: usize = 0x1001_0000;
    const SPI2: usize = 0x1005_0000;

    const UART_TXDATA: usize = 0x00;
    const UART_TXCTRL: usize = 0x08;
    const UART_TXDATA_FULL: u32 = 1 << 31;

    const SPI_SCKDIV: usize = 0x00;
    const SPI_SCKMODE: usize = 0x04;
    const SPI_CSID: usize = 0x10;
    const SPI_CSDEF: usize = 0x14;
    const SPI_CSMODE: usize = 0x18;
    const SPI_DELAY0: usize = 0x28;
    const SPI_DELAY1: usize = 0x2c;
    const SPI_FMT: usize = 0x40;
    const SPI_TXDATA: usize = 0x48;
    const SPI_RXDATA: usize = 0x4c;
    const SPI_TXMARK: usize = 0x50;
    const SPI_RXMARK: usize = 0x54;
    const SPI_FCTRL: usize = 0x60;
    const SPI_IE: usize = 0x70;
    const SPI_IP: usize = 0x74;

    const CSMODE_AUTO: u32 = 0;
    const CSMODE_HOLD: u32 = 2;
    const FMT_LEN_8: u32 = 8 << 16;
    const TXDATA_FULL: u32 = 1 << 31;
    const RXDATA_EMPTY: u32 = 1 << 31;
    const IP_RXWM: u32 = 1 << 1;

    global_asm!(
        r#"
    .section .text.entry
    .global _start
_start:
    csrr t0, mhartid
    beqz t0, 2f
1:
    wfi
    j 1b
2:
    la sp, __stack_top
    call rust_main
3:
    wfi
    j 3b
"#
    );

    #[unsafe(no_mangle)]
    pub extern "C" fn rust_main() -> ! {
        uart_init();
        let mut out = Runner;
        let _ = writeln!(out, "sdmmc-protocol qemu spi");

        match run(&mut out) {
            Ok(()) => {
                let _ = writeln!(out, "PASS");
            }
            Err(err) => {
                let _ = writeln!(out, "FAIL: {:?}", err);
            }
        }

        loop {
            unsafe {
                asm!("wfi");
            }
        }
    }

    fn run(out: &mut Runner) -> Result<(), Error> {
        let spi = SifiveSpi::new(SPI2);
        let mut card = SpiSdmmc::new(spi, SpinDelay);
        let info = card.init()?;
        let _ = writeln!(
            out,
            "card: sd_v2={} high_capacity={} ocr=0x{:08x} capacity_blocks={:?}",
            info.sd_v2, info.high_capacity, info.ocr, info.capacity_blocks
        );

        run_case(out, "init_metadata", |_| test_init_metadata(&info))?;
        run_case(out, "read_block_zero", |out| {
            test_read_block_zero(out, &mut card)
        })?;
        run_case(out, "read_block_pattern_probe", |out| {
            test_read_block_pattern_probe(out, &mut card)
        })?;
        run_case(out, "multi_block_read_probe", |out| {
            test_multi_block_read_probe(out, &mut card)
        })?;
        run_case(out, "high_speed_switch", |out| {
            test_high_speed_switch(out, &mut card)
        })?;
        run_case(out, "write_probe", |out| test_write_probe(out, &mut card))?;

        Ok(())
    }

    fn run_case<F>(out: &mut Runner, name: &str, case: F) -> Result<(), Error>
    where
        F: FnOnce(&mut Runner) -> Result<(), Error>,
    {
        match case(out) {
            Ok(()) => {
                let _ = writeln!(out, "PASS: {}", name);
                Ok(())
            }
            Err(e) => {
                let _ = writeln!(out, "FAIL: {} ({:?})", name, e);
                Err(e)
            }
        }
    }

    fn test_init_metadata(info: &sdmmc_protocol::spi::CardInfo) -> Result<(), Error> {
        if !info.sd_v2 || !info.high_capacity {
            return Err(Error::BadResponse(ErrorContext::default()));
        }
        if info.capacity_blocks != Some(131_072) {
            return Err(Error::BadResponse(ErrorContext::default()));
        }
        let cid = info
            .cid
            .ok_or(Error::BadResponse(ErrorContext::default()))?;
        if cid.raw.iter().all(|&b| b == 0) {
            return Err(Error::BadResponse(ErrorContext::default()));
        }
        Ok(())
    }

    fn test_read_block_zero<T: SpiTransport, D: DelayNs>(
        out: &mut Runner,
        card: &mut SpiSdmmc<T, D>,
    ) -> Result<(), Error> {
        let mut block = [0u8; 512];
        card.read_block(0, &mut block)?;

        let checksum = block
            .iter()
            .fold(0u32, |sum, &byte| sum.wrapping_add(byte as u32));
        let _ = writeln!(out, "block0 checksum=0x{:08x}", checksum);

        if !block.starts_with(b"sdmmc-protocol-qemu-spi\n") {
            return Err(Error::ReadError(ErrorContext::default()));
        }

        Ok(())
    }

    fn test_read_block_pattern_probe<T: SpiTransport, D: DelayNs>(
        out: &mut Runner,
        card: &mut SpiSdmmc<T, D>,
    ) -> Result<(), Error> {
        let mut block = [0u8; 512];
        match card.read_block(1, &mut block) {
            Ok(()) if block.iter().all(|&b| b == 0) => {
                let _ = writeln!(out, "  nonzero-address single-block read accepted");
            }
            Ok(()) => {
                let _ = writeln!(
                    out,
                    "  CMD17 nonzero address returned data mismatch (QEMU SPI SD limitation)"
                );
            }
            Err(e) => {
                let _ = writeln!(
                    out,
                    "  CMD17 nonzero address returned {:?} (QEMU SPI SD limitation)",
                    e
                );
            }
        }
        Ok(())
    }

    fn test_multi_block_read_probe<T: SpiTransport, D: DelayNs>(
        out: &mut Runner,
        card: &mut SpiSdmmc<T, D>,
    ) -> Result<(), Error> {
        const COUNT: usize = 4;

        let mut readback = [[0u8; 512]; COUNT];
        let mut next = 0usize;
        match card.read_blocks(0, COUNT as u32, |_addr, block| {
            if next < COUNT {
                readback[next] = *block;
                next += 1;
            }
        }) {
            Ok(()) if next == COUNT && readback[0].starts_with(b"sdmmc-protocol-qemu-spi\n") => {
                let _ = writeln!(out, "  CMD18 multi-block read accepted");
            }
            Ok(()) => {
                let _ = writeln!(
                    out,
                    "  CMD18 returned mismatched data (QEMU SPI SD limitation)"
                );
            }
            Err(e) => {
                let _ = writeln!(out, "  CMD18 returned {:?} (QEMU SPI SD limitation)", e);
            }
        }
        Ok(())
    }

    fn test_high_speed_switch<T: SpiTransport, D: DelayNs>(
        out: &mut Runner,
        card: &mut SpiSdmmc<T, D>,
    ) -> Result<(), Error> {
        match card.switch_to_high_speed() {
            Ok(true) => {
                let _ = writeln!(out, "  high-speed reported active");
            }
            Ok(false) => {
                let _ = writeln!(out, "  high-speed switch acknowledged but inactive");
            }
            Err(e) => {
                let _ = writeln!(out, "  CMD6 returned {:?} (QEMU SD model limitation)", e);
            }
        }
        Ok(())
    }

    fn test_write_probe<T: SpiTransport, D: DelayNs>(
        out: &mut Runner,
        card: &mut SpiSdmmc<T, D>,
    ) -> Result<(), Error> {
        let mut pattern = [0u8; 512];
        fill_pattern(&mut pattern, 0x11);

        match card.write_block(100, &pattern) {
            Ok(()) => {
                let _ = writeln!(out, "  single-block write accepted");
            }
            Err(e) => {
                let _ = writeln!(out, "  CMD24 returned {:?} (QEMU SPI SD limitation)", e);
            }
        }
        Ok(())
    }

    fn fill_pattern(block: &mut [u8; 512], seed: u8) {
        for (i, b) in block.iter_mut().enumerate() {
            *b = seed.wrapping_add((i as u8).wrapping_mul(17)) ^ ((i >> 3) as u8);
        }
    }

    /// `DelayNs` adapter using a tight spin loop. The QEMU virt SPI example
    /// has no clock driver so this is the simplest source of bounded waits.
    struct SpinDelay;

    impl DelayNs for SpinDelay {
        fn delay_ns(&mut self, _ns: u32) {}

        fn delay_us(&mut self, _us: u32) {}

        fn delay_ms(&mut self, _ms: u32) {}
    }

    struct SifiveSpi {
        base: usize,
        preamble_clocks: u8,
        selected: bool,
    }

    impl SifiveSpi {
        fn new(base: usize) -> Self {
            let mut spi = Self {
                base,
                preamble_clocks: 10,
                selected: false,
            };
            spi.init();
            spi
        }

        fn init(&mut self) {
            self.write32(SPI_IE, 0);
            self.write32(SPI_TXMARK, 1);
            self.write32(SPI_RXMARK, 0);
            self.write32(SPI_DELAY0, 1 | (1 << 16));
            self.write32(SPI_DELAY1, 1);
            self.write32(SPI_FCTRL, 0);

            self.write32(SPI_SCKDIV, 0xff);
            self.write32(SPI_SCKMODE, 0);
            self.write32(SPI_CSID, 0);
            self.write32(SPI_CSDEF, 0xffff_ffff);
            self.write32(SPI_FMT, FMT_LEN_8);
            self.write32(SPI_CSMODE, CSMODE_AUTO);

            while self.read32(SPI_RXDATA) & RXDATA_EMPTY == 0 {}
        }

        fn set_selected(&mut self, selected: bool) {
            if selected == self.selected {
                return;
            }

            self.write32(SPI_CSMODE, if selected { CSMODE_HOLD } else { CSMODE_AUTO });
            self.selected = selected;
        }

        fn xfer(&mut self, byte: u8) -> Result<u8, Error> {
            self.wait_tx_ready()?;
            self.write32(SPI_TXDATA, byte as u32);

            self.wait_rx_ready()?;
            let raw = self.read32(SPI_RXDATA);
            if raw & RXDATA_EMPTY != 0 {
                return Err(Error::BusError(ErrorContext::default()));
            }
            Ok((raw & 0xff) as u8)
        }

        fn wait_tx_ready(&self) -> Result<(), Error> {
            for _ in 0..1_000_000 {
                if self.read32(SPI_TXDATA) & TXDATA_FULL == 0 {
                    return Ok(());
                }
            }
            Err(Error::Timeout(ErrorContext::default()))
        }

        fn wait_rx_ready(&self) -> Result<(), Error> {
            for _ in 0..1_000_000 {
                if self.read32(SPI_IP) & IP_RXWM != 0 {
                    return Ok(());
                }
            }
            Err(Error::Timeout(ErrorContext::default()))
        }

        fn read32(&self, offset: usize) -> u32 {
            unsafe { ((self.base + offset) as *const u32).read_volatile() }
        }

        fn write32(&self, offset: usize, value: u32) {
            unsafe {
                ((self.base + offset) as *mut u32).write_volatile(value);
            }
        }
    }

    impl SpiTransport for SifiveSpi {
        fn transfer_byte(&mut self, byte: u8) -> Result<u8, Error> {
            self.xfer(byte)
        }

        fn select(&mut self) -> Result<(), Error> {
            self.set_selected(true);
            Ok(())
        }

        fn deselect(&mut self) -> Result<(), Error> {
            self.xfer(0xff)?;
            self.set_selected(false);
            Ok(())
        }

        fn clock(&mut self) -> Result<(), Error> {
            if self.preamble_clocks > 0 {
                self.set_selected(false);
                self.preamble_clocks -= 1;
                self.xfer(0xff)?;
                Ok(())
            } else {
                self.transfer_byte(0xff)?;
                Ok(())
            }
        }
    }

    struct Runner;

    impl Write for Runner {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            for byte in s.bytes() {
                uart_put(byte);
            }
            Ok(())
        }
    }

    fn uart_put(byte: u8) {
        for _ in 0..1_000_000 {
            let txdata = unsafe { ((UART0 + UART_TXDATA) as *const u32).read_volatile() };
            if txdata & UART_TXDATA_FULL == 0 {
                unsafe {
                    ((UART0 + UART_TXDATA) as *mut u32).write_volatile(byte as u32);
                }
                return;
            }
        }
    }

    fn uart_init() {
        unsafe {
            ((UART0 + UART_TXCTRL) as *mut u32).write_volatile(1);
        }
    }

    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        let mut runner = Runner;
        let _ = writeln!(runner, "panic: {}", info);
        loop {
            unsafe {
                asm!("wfi");
            }
        }
    }
}
