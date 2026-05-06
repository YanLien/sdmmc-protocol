# `sdhci-host` 使用指南

本文档讲清三件事：

1. 如何把 `sdhci-host` 接到一块真实的板子上
2. 如何接入平台时钟（CRU / clock controller）
3. 何时用 PIO，何时用 ADMA2 DMA，以及两者的接入差异

> 配套源码：`crates/sdhci-host/`，对应的协议层是 `sdmmc-protocol::sdio::SdioSdmmc`。

---

## 1. 总体分层

```text
 ┌──────────────────────────────────────────────────────┐
 │      Application                                     │
 ├──────────────────────────────────────────────────────┤
 │      sdmmc_protocol::sdio::SdioSdmmc<H, D>           │   ← 协议层
 │       (CMD0/2/3/8/41/55/13/16/17/18/24/25, ACMD6 …)  │
 ├──────────────────────────────────────────────────────┤
 │      H: SdioHost     ──── trait 边界 ────            │   ← 协议 ↔ 硬件
 ├──────────────────────────────────────────────────────┤
 │  Sdhci             /  SdhciAdma2<'buf, D: Dma>       │   ← Host 层
 │  (PIO 数据通路)       (ADMA2 数据通路)                │
 ├──────────────────────────────────────────────────────┤
 │  SDHCI v3.x MMIO 寄存器 (0xFE31_0000 / RK3568 等)    │
 └──────────────────────────────────────────────────────┘
```

`Sdhci` 是 PIO 实现，`SdhciAdma2` 在它的基础上重写数据通路走 ADMA2。两者都实现 `SdioHost`，都能直接喂给 `SdioSdmmc`。

---

## 2. 上手最短路径（PIO）

```rust
use embedded_hal::delay::DelayNs;
use sdhci_host::Sdhci;
use sdmmc_protocol::sdio::SdioSdmmc;

// 1. 平台先把 SDHCI 控制器的输入参考时钟拉起来（见 §3）。
//    例如 RK3568 上要让 CRU 把 CLK_EMMC_CORE 配到 ≥ 25 MHz。
//    此时控制器的 Capabilities.BaseClockFreq 字段就有值了。

// 2. 构造 Sdhci。SAFETY: base_addr 必须是合法 SDHCI v3.x 寄存器空间。
let mut host = unsafe { Sdhci::new(0xFE31_0000) };

// 3. 控制器自身复位 + 通电 + 状态开关
host.reset_all()?;
host.set_power(0x0E /* POWER_330 = 3.3V */);
host.enable_interrupts();         // 注意：只开 status，不开 signal IRQ

// 4. 用平台时钟把 SD 总线先调到 400 kHz 以做卡识别
let base_hz = host.base_clock_hz();
host.enable_clock(base_hz, 400_000)?;

// 5. 交给协议层。`SdioSdmmc::init` 会再把总线时钟拉到 25 / 50 MHz
let mut card = SdioSdmmc::new(host, my_delay());
let info = card.init()?;
```

读写：

```rust
let mut block = [0u8; 512];
card.read_single_block(0, &mut block)?;
card.write_single_block(0, &block)?;
```

---

## 3. 接入平台时钟（clk）

### 3.0 先理清两条时钟链

很多人困惑的根源是"clk"这个词指代了两条链路，但它们的归属完全不同：

```text
   ┌─────────┐    ┌──────┐    ┌─ controller ref clk ─┐    ┌──────────┐    ┌─ SD bus clk ─┐
   │  PLL    │ ─► │ Mux/ │ ─► │  (控制器输入引脚)     │ ─► │ SDHCI 内 │ ─► │ (SDCLK 引脚) │ ─► 卡
   │  晶振   │    │ 分频 │    │   也叫 base clock    │    │ 10-bit 分│    │              │
   └─────────┘    └──────┘    └──────────────────────┘    │  频器    │    └──────────────┘
        ▲             ▲                  ▲                 └──────────┘             ▲
        │             │                  │                                          │
   ┌────┴─────────────┴──────────────────┴────┐                            ┌────────┴────────┐
   │  ① 平台代码（CRU/CCU/CCM 寄存器）         │                            │  ② sdhci-host    │
   │     ── 不归 sdhci-host 管 ──             │                            │     enable_clock │
   └──────────────────────────────────────────┘                            │     set_clock    │
                                                                            └─────────────────┘
```

- **① 是 SoC 时钟控制器的活**：选 PLL、分频、给控制器供电的门控位。这部分 SDHCI 标准管不到，每个 SoC 写法都不一样。**这是你要在 BSP / board crate 自己写的代码。**
- **② 是 SDHCI 标准定义的活**：控制器内部还有一个 10-bit 分频器，把"controller ref clk"再分一次得到真正打到卡上的 `SDCLK`。这部分 `sdhci-host` 已经替你处理了（`enable_clock` / `set_clock`）。

`Sdhci::base_clock_hz()` 读的是 `Capabilities.BaseClockFreq`，**报告的是 ① 的输出 / ② 的输入**——也就是你必须先把 ① 配好，才能让 `base_clock_hz()` 返回非 0。

### 3.1 ① 平台侧：把 controller ref clock 拉起来（你写）

每个 SoC 都不一样，下面给三个具体例子，照搬就能跑。

#### 例 1：QEMU `virt` + `sdhci-pci`

什么都不用做。QEMU/EDK2 已经把 ref clock 配好了，`base_clock_hz()` 会直接返回 50 MHz 量级的值。直接跳到 §3.2。

#### 例 2：Rockchip RK3568（eMMC 控制器 @ `0xFE31_0000`）

要做两件事：选父 PLL + 设分频系数 + 打开门控。寄存器地址来自 RK3568 TRM。

```rust
// CRU 基址
const CRU_BASE: usize = 0xFD7C_0000;

// 你想给 eMMC 控制器多少 Hz 的输入时钟。
// 200 MHz 足够支撑 HS200；先验证用 50 MHz / HighSpeed 就够。
const EMMC_REF_HZ: u32 = 200_000_000;

unsafe fn rk3568_emmc_clk_init() {
    // 1) CLK_SEL_CON28: 选 GPLL (1188 MHz) 作为 EMMC 的父，
    //    bit[7:6] = 00 (sel = GPLL)，bit[4:0] = div - 1
    //    GPLL / 6 = 198 MHz，写 div=6 → 寄存器值 0b00_00101 = 5
    let sel_con28 = (CRU_BASE + 0x0370) as *mut u32;
    // CRU 用 16-bit 写掩码：高 16 bit 是 mask，低 16 bit 是 value
    sel_con28.write_volatile((0x00FF << 16) | 0x0005);

    // 2) CLK_GATE_CON5: 清 bit[14] (clk_emmc_gate) 让时钟通到控制器
    let gate_con5 = (CRU_BASE + 0x0354) as *mut u32;
    gate_con5.write_volatile((1 << 14) << 16); // mask=1, value=0 → ungate

    // 3) (可选) 软复位：SOFTRST_CON*
    //    省略，sdhci-host 自己会调 reset_all()

    let _ = EMMC_REF_HZ; // 这个值得跟 ① 实际配出的频率一致
}
```

#### 例 3：用 `embedded-hal`-like 的 SoC clock crate

如果你的 BSP 已经封装了一个 clock crate（如 `rk3568-clk`、`stm32-hal::rcc`），就长这样：

```rust
// 伪代码 — 看你 BSP 的 API
let mut cru = Rk3568Cru::take();
cru.emmc()
   .set_parent(Pll::Gpll)
   .set_divider(6)
   .enable();
let emmc_ref_hz = cru.emmc().rate();  // 198_000_000
```

不管哪条路，目标都一样：**让 `base_clock_hz()` 返回正确的值**。

### 3.2 ② 控制器侧：让 SDHCI 内部分频器干活（sdhci-host 替你做）

平台 ref clock 一上来，剩下的事交给 `sdhci-host`：

```rust
let base_hz = host.base_clock_hz();   // 来自 ① 的配置，比如 198 MHz
assert!(base_hz > 0, "platform ref clock not configured");

// 卡识别阶段：必须 ≤ 400 kHz
host.enable_clock(base_hz, 400_000)?;

// init() 跑完后，协议层会自动调用：
//   set_clock(ClockSpeed::Default)    → 25 MHz
//   set_clock(ClockSpeed::HighSpeed)  → 50 MHz
// 不需要你手动调
```

`enable_clock` 内部做了三件事：关 SD clock → 找最小分频系数让 `base/(2N) ≤ target` → 等内部时钟稳定 → 打开 SD clock。完全是 SDHCI 标准动作，跟 SoC 无关。

### 3.3 一张表把两层对应起来

| 你想要的总线频率 | ① ref clock 至少 | ② SDHCI 分频 | 如何触发 |
|------------------|----------------|------------|---------|
| 400 kHz（识别）   | 任意（≥ 400 kHz） | 自动 | `enable_clock(base, 400_000)` |
| 25 MHz Default   | ≥ 25 MHz       | 自动 | 协议层 `init()` 内部 |
| 50 MHz HighSpeed | ≥ 50 MHz       | 自动 | 协议层 `init()` 内部 |
| 100 MHz HS200    | ≥ 100 MHz      | 自动（但还需 tuning，未实现） | 暂不支持 |

**核心约束**：SDHCI 的 10-bit 分频器只能往下分（除 2N，N=0..1023），不能升频。所以 ① 给的 ref clock **必须 ≥ 你想跑的最高总线频率**。

### 3.4 内部分频器不能用怎么办（外部调频模式）

不是所有控制器都能用内部分频器。常见情况：

- **`Capabilities.BaseClockFreq = 0`**：标准里这个值就是"我没法告诉你 ref clock，分频器无意义"
- **DWC MSHC / Synopsys mobile storage host**（Rockchip / Allwinner / Amlogic 部分 SoC 用这个 IP）：内部分频器不可用，所有频率切换都得通过 SoC CRU 改 ref clock
- **某些 vendor 的 quirk**：HS200/HS400 模式必须 1:1 直通，分频会破坏时序

这种情况下流程反过来——**内部分频器固定为 1:1 直通，外部 CRU 来调频**：

```text
   PLL ──► CRU/CCU 选父&分频 ──► [ref clk = SD bus clk] ──► SDHCI 分频=1:1 ──► [SDCLK]
          └────── ① 这里直接调到目标频率 ──────┘            └─── 仅做 gate ───┘
```

`sdhci-host` 给了两条 API 支持这种模式：

#### 方式 A：手动模式（最直接）

每次想换频率时自己来：

```rust
// 卡识别阶段：把 ref clock 调到 400 kHz
my_cru_set_emmc_rate(400_000);
host.enable_clock_external()?;          // 1:1 gate on，跳过分频器

// 后面要切到 50 MHz：
host.disable_sd_clock();                // 关 SD clock 防 glitch
my_cru_set_emmc_rate(50_000_000);       // CRU 重新调
host.enable_clock_external()?;          // 重新 gate on
```

#### 方式 B：注册回调（推荐，跟协议层无缝衔接）

把 CRU 调频函数注册给 `Sdhci`，之后协议层调 `set_clock` 时会自动走外部时钟：

```rust
fn rk3568_emmc_set_rate(target_hz: u32) -> Result<(), sdmmc_protocol::Error> {
    // 这里写你的 CRU 寄存器，把 EMMC 的输入时钟调到 target_hz
    // 比如根据 target_hz 选不同的 PLL + 分频系数
    unsafe { rk3568_cru_emmc_retune(target_hz) };
    Ok(())
}

let mut host = unsafe { Sdhci::new(0xFE31_0000) };
host.reset_all()?;
host.set_power(0x0E);
host.enable_interrupts();

// 注册外部时钟回调
host.set_external_clock(rk3568_emmc_set_rate);

// 卡识别阶段还是要自己拉一次起来
rk3568_emmc_set_rate(400_000)?;
host.enable_clock_external()?;

let mut card = SdioSdmmc::new(host, my_delay());
card.init()?;  // init() 内部会调 set_clock(Default) → set_clock(HighSpeed)
               //   两次都会自动转发到 rk3568_emmc_set_rate()
```

注册回调后 `Sdhci` 的行为变化：

| 调用            | 内部分频器模式（默认） | 外部时钟模式（注册了 cb 后） |
|-----------------|-------------------|--------------------------|
| `set_clock(x)`  | 算分频系数，写 CLOCK_CONTROL | 关 SD clock → `cb(x)` → 1:1 重启 |
| `enable_clock`  | 算分频系数         | 不应再用，改用 `enable_clock_external` |
| `base_clock_hz` | 给分频用           | 信息性，只在 init 阶段判断"平台是否就位" |

#### 三个低层 API 速查

| API | 用途 |
|-----|------|
| `enable_clock(base, target)` | 内部分频器模式：用分频器把 base 分到 target |
| `enable_clock_external()`    | 外部时钟模式：分频器置 1:1，仅做 gate |
| `disable_sd_clock()`         | 关 SD clock，准备改 CRU（防止抖动到卡） |
| `set_external_clock(cb)`     | 注册 CRU 回调，让 set_clock 自动用外部模式 |

### 3.5 怎么判断走哪条路

按下面顺序判断：

1. **读 `Capabilities`**：`base_clock_hz()` 返回 0 → 必走外部模式
2. **看 SoC 文档/驱动**：用了 DWC MSHC 的 SoC → 走外部模式
3. **看时序**：如果你跑 HS200/HS400/SDR104，几乎一定是外部模式（vendor 时序对齐要求）
4. **其它情况**：用内部分频器（`enable_clock` + 不注册 cb），最简单

### 3.6 为什么不抽象 `embedded-hal` Clock trait

不是不想，而是抽不出来：

- 内部分频器（标准内）已经被 SDHCI 标准固定，不需要 trait
- 外部 CRU（标准外）每个 SoC 完全不同，强行抽象出来的 trait 既不通用也不能减少代码量
- 当前 `set_external_clock(fn(u32) -> Result<...>)` 已经是最薄的胶水：你只需写一个函数把频率调到位就行
- 推荐的边界是：**BSP 暴露 `pub fn init_emmc_clk() -> u32` 跟 `pub fn set_emmc_rate(hz: u32) -> Result<...>`**，前者负责开机一次性的供电/选父，后者注册给 `set_external_clock`

---

## 4. PIO vs ADMA2：怎么选

| 维度 | PIO (`Sdhci`) | ADMA2 (`SdhciAdma2`) |
|------|---------------|----------------------|
| 吞吐 | ~几 MB/s（CPU 搬 FIFO） | 受总线限制，可上 50+ MB/s |
| CPU 占用 | 高（忙等 + 一字字搬运） | 低（DMA 引擎跑，CPU 等中断/状态位） |
| 接入复杂度 | 直接 `Sdhci::new` 即可 | 需要实现 `Dma` trait + 提供描述符 buffer |
| 缓存维护 | 不涉及 | 平台自己负责 dcache flush/invalidate |
| 总线地址 | 不涉及 | 32-bit ADMA2，bus addr 必须 < 4 GiB |
| 缓冲区对齐 | 任意 | **4 字节对齐**，否则返回 `Misaligned` |

**经验法则：**

- 早期 bring-up、调试卡识别、单块读写做 sanity check → **先用 PIO**，确认硬件没问题
- 验证完时序后，切到 ADMA2 跑文件系统/吞吐基准
- 读 BootROM / 1 个 block 的小事务，PIO 也够用，没必要付 cache 维护成本

---

## 5. 接入 PIO 模式

### 5.1 完整骨架

```rust
use embedded_hal::delay::DelayNs;
use sdhci_host::Sdhci;
use sdmmc_protocol::sdio::SdioSdmmc;

fn bringup_pio<D: DelayNs>(mmio_base: usize, delay: D) -> Result<SdioSdmmc<Sdhci, D>, sdmmc_protocol::Error> {
    // 平台 ref clock 假定已经拉起（见 §3.1）
    let mut host = unsafe { Sdhci::new(mmio_base) };
    host.reset_all()?;
    host.set_power(0x0E);
    host.enable_interrupts();
    let base = host.base_clock_hz();
    host.enable_clock(base, 400_000)?;

    let mut card = SdioSdmmc::new(host, delay);
    card.init()?;
    Ok(card)
}
```

### 5.2 边界条件

- buffer 长度必须是 `block_size` 的整数倍（协议层会保证），否则返回 `Misaligned`
- PIO 是 32-bit FIFO 端口，但 `pio_read/write` 已经处理了非 4 字节对齐的尾巴
- 多块传输由协议层 + AutoCMD12 完成，host 不需要手动发 CMD12

---

## 6. 接入 ADMA2 DMA 模式

ADMA2 模式要求你提供两件东西：

1. **`Dma` trait 的实现** —— 做地址翻译和缓存维护
2. **`Adma2Buffer`** —— 描述符表的 backing store（零分配，调用方拥有）

### 6.1 一句话：Dma trait 在干什么

```rust
pub trait Dma {
    fn map(&self, buf: *const u8, len: usize, dir: DmaDir) -> u64;
    fn before_dma(&self, buf: *const u8, len: usize, dir: DmaDir);
    fn after_dma(&self, buf: *const u8, len: usize, dir: DmaDir);
}
```

- `map`：CPU 指针 → 设备看到的总线地址。三种典型实现：
  - **identity 映射** + 无 IOMMU：直接 `ptr as u64`
  - **有 DMA offset**（很多 ARM SoC 有 PCIe/sysbus offset）：`ptr as u64 + DMA_OFFSET`
  - **IOMMU**：调你的 IOMMU 驱动 `iommu_map()` 拿一段 IOVA
- `before_dma`：传输开始前
  - **写方向（ToDevice）**：dcache **clean**（保证 CPU 写过的数据已经回到内存）
  - **读方向（FromDevice）**：dcache **invalidate**（避免读到陈旧 cache 行）
- `after_dma`：传输结束后
  - **读方向（FromDevice）**：再 invalidate 一次（防止预取过的脏行覆盖 device 写入）
  - **写方向（ToDevice）**：通常 no-op

> 在 cache-coherent 的系统（如 x86、QEMU virt 默认）三个 hook 都可以是空函数，`map` 直接返回 `ptr as u64`。

### 6.2 三种典型场景的 Dma 实现

#### 场景 A：QEMU `virt` / 简单 cache-coherent 板（最简）

```rust
use sdhci_host::{Dma, DmaDir};

pub struct CoherentDma;
impl Dma for CoherentDma {
    fn map(&self, p: *const u8, _: usize, _: DmaDir) -> u64 { p as u64 }
    fn before_dma(&self, _: *const u8, _: usize, _: DmaDir) {}
    fn after_dma(&self, _: *const u8, _: usize, _: DmaDir) {}
}
```

#### 场景 B：AArch64 裸机，identity 映射，cache 非一致

```rust
use core::arch::asm;
use sdhci_host::{Dma, DmaDir};

pub struct A64Dma;

fn dcache_clean_range(start: usize, end: usize) {
    // CTR_EL0.DminLine 给出 dcache line size，省略；这里假设 64B
    const LINE: usize = 64;
    let mut p = start & !(LINE - 1);
    while p < end {
        unsafe { asm!("dc cvac, {0}", in(reg) p) }
        p += LINE;
    }
    unsafe { asm!("dsb sy") }
}

fn dcache_invalidate_range(start: usize, end: usize) {
    const LINE: usize = 64;
    let mut p = start & !(LINE - 1);
    while p < end {
        unsafe { asm!("dc ivac, {0}", in(reg) p) }
        p += LINE;
    }
    unsafe { asm!("dsb sy") }
}

impl Dma for A64Dma {
    fn map(&self, p: *const u8, _len: usize, _dir: DmaDir) -> u64 { p as u64 }

    fn before_dma(&self, p: *const u8, len: usize, dir: DmaDir) {
        let s = p as usize;
        match dir {
            DmaDir::ToDevice   => dcache_clean_range(s, s + len),
            DmaDir::FromDevice => dcache_invalidate_range(s, s + len),
        }
    }

    fn after_dma(&self, p: *const u8, len: usize, dir: DmaDir) {
        if matches!(dir, DmaDir::FromDevice) {
            let s = p as usize;
            dcache_invalidate_range(s, s + len);
        }
    }
}
```

#### 场景 C：上层有现成的 dma-api crate

把它们的 `map_single` / `unmap_single` / `dma_sync_*` 直接转发到 `map` / `before_dma` / `after_dma` 即可。

### 6.3 完整 ADMA2 接入骨架

```rust
use embedded_hal::delay::DelayNs;
use sdhci_host::{Adma2Buffer, Sdhci, SdhciAdma2};
use sdmmc_protocol::sdio::SdioSdmmc;

// 描述符表 — 一个控制器一份，整个生命周期都借给 SdhciAdma2。
// 也可以放在 .bss / .data 段里作为 static（自己保证 Sync）。
let table = Adma2Buffer::new();

let mut inner = unsafe { Sdhci::new(0xFE31_0000) };
inner.reset_all()?;
inner.set_power(0x0E);
inner.enable_interrupts();
let base = inner.base_clock_hz();
inner.enable_clock(base, 400_000)?;

// 可选：检查能力位，避免在不支持 ADMA2 的老控制器上跑
assert!(inner.supports_adma2(), "controller doesn't advertise ADMA2");

let host = SdhciAdma2::new(inner, A64Dma, &table);
let mut card = SdioSdmmc::new(host, my_delay());
card.init()?;
```

### 6.4 缓冲区约束

- 必须 **4 字节对齐**（`buf.as_ptr() as usize % 4 == 0`）
- 长度必须是 `block_size` 的整数倍
- 单次传输上限 = `ADMA2_DESC_COUNT * 64 KiB`（默认 16 个描述符 ≈ 1 MiB），协议层最大也只会一次发到 65535 个 block，所以正常使用不会超
- 32-bit ADMA2：bus 地址必须 < 4 GiB，否则 `build_descriptors` 返回 `BadResponse`

### 6.5 错误识别

`SdhciAdma2` 在 ADMA 引擎报错时会返回 `Error::Misaligned`（描述符 / 地址越界），其它错误（CRC、timeout、data-line error）跟 PIO 路径一致。

---

## 7. 常见坑

| 现象 | 可能原因 |
|------|----------|
| `enable_clock` 卡在 `CLOCK_INTERNAL_STABLE` 等不到 | 平台 ref clock 没拉起，`base_clock_hz()` 是 0 或太低 |
| `init()` 在 CMD8 / ACMD41 timeout | 卡没插好 / Power 字节给错 / SDR12 时钟太低 |
| ADMA2 `Misaligned` | buffer 不是 4 字节对齐，或 bus 地址 ≥ 4 GiB |
| ADMA2 数据全是 0xFF / 旧值 | `before_dma` / `after_dma` 没做 cache 维护 |
| 多块写偶发 CRC | 时钟太快或没切 HighSpeed bit；先 fallback 到 25 MHz 验证 |
| `set_bus_width(Bit8)` 报 `UnsupportedCommand` | 当前 host 实现 4-bit 上限，eMMC 8-bit 还没做 |

---

## 8. 下一步可以做什么

- 64-bit ADMA2（SDHCI v4 描述符）
- 中断驱动 + async（替换 `wait_*` 的轮询）
- HS200 / SDR104 + tuning（CMD19 / CMD21）
- 1.8 V 切换（CMD11 + `HOST_CONTROL2.SignalVoltage`）
- eMMC 8-bit 总线 + EXT_CSD 路径
