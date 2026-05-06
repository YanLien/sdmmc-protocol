# SPI 与 SDIO 模式异同对照

本文档阐述 SD/MMC 卡在 **SPI 模式** 与 **SDIO/SD 原生模式** 下的协议、电气、性能和能力差异，以及 `sdmmc-protocol` crate 如何在代码层面建模这两条路径。

> 关键事实：物理层面是 **同一张卡**，但卡上电后第一条命令的发送方式决定它进入哪一套状态机，此后两种模式不可混用。

---

## 1. 共同点

| 维度 | 说明 |
|------|------|
| 命令集 | 都是 SD 物理层规范定义的 CMDx / ACMDx：CMD0、CMD8、ACMD41、CMD17/24、CMD18/25、CMD6 等。命令编号、参数布局、CRC7 计算方式完全相同。 |
| 状态机 | `idle → ready → ident → standby → tran → data / programming` 这张状态图两边一致。 |
| 响应字段语义 | R1/R1b/R2/R3/R6/R7 各字段含义和编码相同（CSD、CID、OCR、RCA、状态位）。差异只在帧长度和封装。 |
| CRC 多项式 | 命令 CRC7 = `x⁷ + x³ + 1`；数据 CRC16 = `x¹⁶ + x¹² + x⁵ + 1`（CCITT）。 |
| 块大小 | 默认 512 字节；SDHC/SDXC 用块地址，SDSC 用字节地址。换算逻辑共享自 `src/common.rs::block_addr_of`。 |
| 初始化骨架 | `CMD0 → CMD8 → ACMD41 (轮询 idle) → CMD2/3 或 CMD58 → CMD9/10 → CMD7` 的高层流程一致。 |

---

## 2. 不同点

### 2.1 电气与引脚

|  | SPI 模式 | SDIO/原生模式 |
|---|---------|--------------|
| 引脚数 | 4：CS、SCK、MOSI、MISO | 6（1-bit）/ 9（4-bit）/ 13（8-bit eMMC） |
| 选片机制 | 软件拉低/拉高 CS | 没有 CS；通过 RCA 在总线上寻址 |
| 命令/数据线 | 共用 MOSI/MISO | CMD 线与 DAT 线物理分离 |
| 数据线宽度 | 永远 1-bit | 1-bit / 4-bit (SD) / 8-bit (eMMC) |

### 2.2 帧格式

**SPI 模式**

- 命令固定 6 字节：`{0,1, CMD[6], arg[32], CRC7, 1}`，靠 CS 起止帧。
- R1 是单字节。
- R2（CSD/CID）是 16 字节，内联在数据相中，用起始 token `0xFE` 引导。
- R3 / R7 = R1 + 32 位 payload。
- 数据相用 token 划分：`0xFE`（单块/多块读起始）、`0xFC`（多块写起始）、`0xFD`（停止传输）。
- 卡忙状态通过持续读到 `0x00` 判断（DO 拉低）。

**SDIO/原生模式**

- 命令在 CMD 线上以 48 位帧发送（含 CRC7），由 host controller 硬件成帧。
- 响应也是 48 位（R1/R3/R6/R7）或 136 位（R2，内嵌 CSD/CID）。
- 数据走 DAT0（1-bit）或 DAT0–3（4-bit），每条 DAT 线独立计算 CRC16。
- 卡忙状态通过 DAT0 是否被拉低判断（host controller 硬件可监听）。

### 2.3 数据 CRC

|  | SPI | SDIO |
|---|------|------|
| 接收方向 | 卡发送 CRC16，host **可选** 验证（spec 允许跳过） | host controller **必须** 验证；CRC 错误时卡会重发 |
| 发送方向 | host **必须** 发 CRC16（即便卡不强制校验） | host **必须** 发；卡校验失败会拒绝该数据块 |

> 对应到代码：`SpiSdmmc::set_verify_data_crc(false)` 在 SPI 路径里存在，SDIO 路径不需要也没有这个开关。

### 2.4 速度档与模式切换

|  | SPI | SDIO |
|---|------|------|
| 默认速率 | 400 kHz 起步 → 25 MHz | 400 kHz → 25 MHz (DS) |
| 高速档 | 25 MHz 是上限 | HS (50 MHz) → UHS-I SDR50 / SDR104 / DDR50 → UHS-II / III |
| 总线宽度切换 | 不支持 | ACMD6 切到 4-bit；MMC 用 CMD6 / SWITCH |
| 1.8 V 信号切换 | 不支持 | CMD11，UHS-I 前置 |
| Tuning | 不需要 | SDR50 / SDR104 / HS200 用 CMD19 / CMD21 |
| 实际带宽上限 | ≈ 12.5 MB/s | 4-bit DDR50 ≈ 50 MB/s；UHS-I SDR104 ≈ 104 MB/s；UHS-II ≈ 624 MB/s |

### 2.5 能力差异（SPI 做不到的）

- ACMD6 切换总线宽度——SPI 永远 1-bit。
- 电压切换 / UHS 速率档——SPI 仅支持默认 3.3 V、≤ 25 MHz。
- SDIO IO 模式（CMD52 / CMD53 直接访问 SDIO 卡寄存器，例如 WiFi / BT 模组）。
- 中断 / DAT0 忙线感知——SPI 只能软件轮询 `0xFF`。
- CMD2（ALL_SEND_CID）SPI 模式不发 R2 形式响应——CID 要靠 CMD10 单独读。
- 没有 RCA 概念——SPI 用 CS 选片，因此不发 CMD3。

反过来，**SPI 接口几乎所有 MCU 都有**，而 SDIO host controller 是较少集成的外设。

### 2.6 错误反馈粒度

- **SPI**：R1 字节里 7 个 bit 描述错误（idle、erase reset、illegal command、CRC error、erase sequence、address error、parameter error）。
- **SDIO**：CMD13（SEND_STATUS）返回 32 位 Card Status，字段更丰富（`CURRENT_STATE`、`READY_FOR_DATA`、`AKE_SEQ_ERROR`、`CARD_ECC_FAILED` 等）。

---

## 3. 在本 crate 中的代码抽象

两条路径用不同的 trait 反映**硬件分工差异**：SPI 模式下大部分协议工作落在本 crate 上，SDIO 模式下大部分协议工作落在 host controller 上。

### 3.1 SPI: 字节级 trait

```rust
pub trait SpiTransport {
    fn transfer_byte(&mut self, byte: u8) -> Result<u8, Error>;
    fn select(&mut self) -> Result<(), Error> { Ok(()) }
    fn deselect(&mut self) -> Result<(), Error> { Ok(()) }
    fn send_byte(&mut self, byte: u8) -> Result<(), Error> { ... }
    fn clock(&mut self) -> Result<(), Error> { ... }
}

pub struct SpiSdmmc<T: SpiTransport, D: DelayNs> { ... }
```

平台只需实现「发一个字节并读回一个字节」。命令封装、CRC7 计算、token 解析、CRC16 校验、数据相组帧、忙线轮询全都由本 crate 完成。

### 3.2 SDIO: 命令级 trait

```rust
pub trait SdioHost {
    fn send_command(&mut self, cmd: &Command) -> Result<Response, Error>;
    fn set_bus_width(&mut self, width: BusWidth) -> Result<(), Error>;
    fn set_clock(&mut self, speed: ClockSpeed) -> Result<(), Error>;
    fn prepare_data_transfer(&mut self, ...) -> Result<(), Error>;
}

pub struct SdioSdmmc<H: SdioHost, D: DelayNs> { ... }
```

`SdioHost` 比 `SpiTransport` 厚得多——它要暴露调时钟、调位宽、准备 DMA 这些 host controller 级别的操作。本 crate 只负责命令编排和高层流程。

### 3.3 共享部分

`src/common.rs` 与 `src/diag.rs` 在两条路径间复用：

- `block_addr_of(addr, high_capacity)`：SDHC/SDSC 块地址换算。
- `crc16_ccitt(bytes)`：CCITT CRC-16，仅 SPI 路径用（SDIO 由硬件做 CRC16）。
- `diag::{trace, debug, info, warn_}`：统一的日志 shim，按需路由到 `defmt` / `log` / no-op。

---

## 4. 选型建议

| 场景 | 推荐 |
|------|------|
| 资源紧张的 MCU，只要能读写 SD 卡 | **SPI**——简单、引脚少、几乎所有 SoC 都有 |
| 嵌入式 Linux / RTOS，需要 ≥ 50 MB/s | **SDIO**（4-bit HS 或 UHS-I） |
| WiFi / BT 模组、SDIO interrupt 设备 | 必须 **SDIO**——SPI 做不了 |
| 板载 eMMC 芯片 | 必须 **eMMC** 模式（SDIO 协议族扩展） |
| 工业 / 可靠性场景，带宽不重要、希望 CRC 控制权在 host | **SPI**——容易调试 |

经验法则：**追性能选 SDIO；追可移植性 + 调试便利选 SPI**。

---

## 5. 参考

- SD Physical Layer Simplified Specification（SDA 官网公开版）
- SDIO Simplified Specification
- JEDEC Standard No. 84-B51（eMMC）
- 本 crate 源码：
  - SPI 路径：`src/spi.rs`
  - SDIO 路径：`src/sdio.rs`
  - 共享辅助：`src/common.rs`、`src/diag.rs`
  - 命令 / 响应解码：`src/cmd.rs`、`src/response.rs`
