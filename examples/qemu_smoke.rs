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
    use sdmmc_protocol::cmd;
    use sdmmc_protocol::response::R1Response;
    use sdmmc_protocol::spi::{SpiSdmmc, SpiTransport};
    use sdmmc_protocol::{Error, ErrorContext};

    const UART0: usize = 0x1000_0000;
    const TEST_FINISHER: usize = 0x100000;

    /// `DelayNs` implementation that just spins the CPU. Good enough for the
    /// smoke test where wall-clock accuracy doesn't matter and we have no
    /// CLINT/timer driver.
    struct SpinDelay;

    impl DelayNs for SpinDelay {
        fn delay_ns(&mut self, ns: u32) {
            for _ in 0..ns.max(1) {
                core::hint::spin_loop();
            }
        }
    }

    global_asm!(
        r#"
    .section .text.entry
    .global _start
_start:
    la sp, __stack_top
    call rust_main
1:
    wfi
    j 1b
"#
    );

    #[unsafe(no_mangle)]
    pub extern "C" fn rust_main() -> ! {
        let mut runner = Runner;
        let _ = writeln!(runner, "sdmmc-protocol qemu smoke");

        match run() {
            Ok(()) => {
                let _ = writeln!(runner, "PASS");
                exit(0);
            }
            Err(err) => {
                let _ = writeln!(runner, "FAIL: {:?}", err);
                exit(1);
            }
        }
    }

    fn run() -> Result<(), Error> {
        assert_or_error(cmd::CMD0.to_spi_bytes() == [0x40, 0, 0, 0, 0, 0x95])?;

        let r1 = R1Response::from_spi_byte(0x05)?;
        assert_or_error(r1.idle())?;
        assert_or_error(r1.illegal_command())?;

        let mut init_transport = ScriptedTransport::new();
        init_transport.push_ignored(10)?;
        init_transport.push_command_response(0x01, &[])?;
        init_transport.push_command_response(0x01, &[0x00, 0x00, 0x01, 0xAA])?;
        init_transport.push_command_response(0x01, &[])?;
        init_transport.push_command_response(0x01, &[])?;
        init_transport.push_command_response(0x01, &[])?;
        init_transport.push_command_response(0x00, &[])?;
        init_transport.push_command_response(0x00, &[0xC0, 0xFF, 0x80, 0x00])?;
        // CMD9 (CSD) response — 16 zero bytes + 2 trailer bytes.
        init_transport.push_command_response(0x00, &[])?;
        init_transport.push_byte(0xFE)?;
        for _ in 0..16 {
            init_transport.push_byte(0x00)?;
        }
        init_transport.push_byte(0xFF)?;
        init_transport.push_byte(0xFF)?;
        // CMD10 (CID) response — same shape as CSD.
        init_transport.push_command_response(0x00, &[])?;
        init_transport.push_byte(0xFE)?;
        for _ in 0..16 {
            init_transport.push_byte(0x00)?;
        }
        init_transport.push_byte(0xFF)?;
        init_transport.push_byte(0xFF)?;

        let mut card = SpiSdmmc::new(init_transport, SpinDelay);
        // Scripted transports use placeholder CRC trailers; skip verification
        // so the smoke test can focus on the higher-level command flow.
        card.set_verify_data_crc(false);
        let info = card.init()?;
        assert_or_error(info.sd_v2)?;
        assert_or_error(info.high_capacity)?;
        assert_or_error(info.ocr == 0xC0FF_8000)?;

        let mut read_transport = ScriptedTransport::new();
        read_transport.push_command_response(0x00, &[])?;
        read_transport.push_byte(0xFF)?;
        read_transport.push_byte(0xFE)?;
        for i in 0..512 {
            read_transport.push_byte((i & 0xFF) as u8)?;
        }
        read_transport.push_byte(0x12)?;
        read_transport.push_byte(0x34)?;

        let mut card = SpiSdmmc::new(read_transport, SpinDelay);
        card.set_verify_data_crc(false);
        let mut block = [0u8; 512];
        card.read_block(7, &mut block)?;
        assert_or_error(block[0] == 0)?;
        assert_or_error(block[255] == 255)?;
        assert_or_error(block[256] == 0)?;
        assert_or_error(block[511] == 255)?;

        Ok(())
    }

    fn assert_or_error(ok: bool) -> Result<(), Error> {
        if ok {
            Ok(())
        } else {
            Err(Error::BadResponse(ErrorContext::default()))
        }
    }

    struct ScriptedTransport {
        rx: [u8; 2048],
        len: usize,
        pos: usize,
    }

    impl ScriptedTransport {
        const fn new() -> Self {
            Self {
                rx: [0; 2048],
                len: 0,
                pos: 0,
            }
        }

        fn push_byte(&mut self, byte: u8) -> Result<(), Error> {
            if self.len >= self.rx.len() {
                return Err(Error::InvalidArgument);
            }
            self.rx[self.len] = byte;
            self.len += 1;
            Ok(())
        }

        fn push_ignored(&mut self, count: usize) -> Result<(), Error> {
            for _ in 0..count {
                self.push_byte(0xFF)?;
            }
            Ok(())
        }

        fn push_command_response(&mut self, r1: u8, extra: &[u8]) -> Result<(), Error> {
            self.push_ignored(6)?;
            self.push_byte(r1)?;
            for &byte in extra {
                self.push_byte(byte)?;
            }
            Ok(())
        }
    }

    impl SpiTransport for ScriptedTransport {
        fn transfer_byte(&mut self, _byte: u8) -> Result<u8, Error> {
            if self.pos >= self.len {
                return Err(Error::Timeout(ErrorContext::default()));
            }
            let byte = self.rx[self.pos];
            self.pos += 1;
            Ok(byte)
        }
    }

    struct Runner;

    impl Write for Runner {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            for byte in s.bytes() {
                unsafe {
                    (UART0 as *mut u8).write_volatile(byte);
                }
            }
            Ok(())
        }
    }

    fn exit(code: u32) -> ! {
        let value = if code == 0 {
            0x5555
        } else {
            (code << 16) | 0x3333
        };
        unsafe {
            (TEST_FINISHER as *mut u32).write_volatile(value);
        }
        loop {
            unsafe {
                asm!("wfi");
            }
        }
    }

    #[panic_handler]
    fn panic(info: &PanicInfo<'_>) -> ! {
        let mut runner = Runner;
        let _ = writeln!(runner, "panic: {}", info);
        exit(1);
    }
}
