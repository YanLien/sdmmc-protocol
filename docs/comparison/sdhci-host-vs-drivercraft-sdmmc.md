# `sdhci-host` 与 `drivercraft/sdmmc` 初始化流程对比

本文档逐条对照本仓库 `crates/sdhci-host` 与上游
[`drivercraft/sdmmc`](https://github.com/drivercraft/sdmmc)
两份驱动在**初始化阶段**的行为差异。两者都基于 SDHCI v3.x
寄存器接口（前者通用，后者主线针对 Rockchip DWCMSHC 变种），
但在初始化的**职责划分**、**步骤完整度**和**容错策略**上都不一致。

> 关键事实：`drivercraft/sdmmc::EMmcHost::init()` 是「控制器
> bring-up + eMMC 卡识别 + EXT_CSD 解析 + 速度协商（含
> HS200/HS400/HS400ES tuning）」一次性做完；
> `sdhci-host` 仅暴露控制器层 bring-up（reset / power /
> interrupt / clock），卡识别与速度协商交由
> `sdmmc-protocol::SdioSdmmc::init()` 通过 `SdioHost` trait
> 调用。可对比的同层只有控制器 bring-up；卡识别这一段
> 在两份代码中位于不同抽象层。

---

## 1. 职责划分

| 阶段 | `drivercraft/sdmmc` | `sdhci-host` |
|------|---------------------|--------------|
| 控制器复位 / Power / Interrupt / Clock | ✅ 全部封装进 `EMmcHost::init()` | ✅ 拆成 `reset_all` / `set_power` / `enable_interrupts` / `enable_clock` 四个独立函数，由调用方按 README 的 "bring-up checklist" 顺序触发 |
| 选 voltage（根据 capabilities） | ✅ 从 caps 自动选 3.3 V / 3.0 V / 1.8 V | ❌ 调用方传入 `POWER_330` / `POWER_180` 字节 |
| host_caps 维护（HS / 4BIT / 8BIT 标志位） | ✅ 内部维护，影响后续模式选择 | ❌ 不维护，由 `sdmmc-protocol` 根据卡能力调度 |
| 卡识别（CMD0/1/2/3/9/7） | ✅ 在 `init_card()` 里做 | ❌ 由 `sdmmc-protocol::SdioSdmmc::init()` 做 |
| EXT_CSD 解析 + partition / boot / RPMB / GP | ✅ 完整解析 | ❌ 协议层目前未覆盖到这一深度 |
| HS200 / HS400 / HS400ES + tuning | ✅ 在 `mmc_change_freq` + `mmc_hs200_tuning` 里做 | ⚠ 控制器层 `execute_tuning(cmd_index)` 已就绪（CMD19/21），但协议层暂未编排成完整流程 |

> 说明：`drivercraft/sdmmc` 把这些事打包做了，所以对外只要
> `EMmcHost::new(base).init()` 一行；`sdhci-host` 必须先
> `Sdhci::new(base)` → `reset_all` → `set_power` →
> `enable_interrupts` → `enable_clock(base_hz, 400_000)`，
> 再交给 `SdioSdmmc::new(host, delay).init()` 才算"等价"。

---

## 2. 控制器 bring-up 步骤逐项对照

下表按 `drivercraft/sdmmc::EMmcHost::init()` 的实际代码顺序展开
（截止到 `init_card()` 之前），并对应到 `sdhci-host` 的同名能力。

| # | 步骤 | `drivercraft/sdmmc` | `sdhci-host` | 备注 |
|---|------|---------------------|--------------|------|
| 1 | 构造结构体 | `EMmcHost::new(base)` 同时读 `CAPABILITIES1`，初步推 `clock_base = (caps >> 8 & 0xFF) × 1e6` | `Sdhci::new(base)` **仅记录 `base_addr`**，不读任何寄存器 | `sdhci-host` 把读 caps 推迟到 `base_clock_hz()` 调用时 |
| 2 | 软复位 RESET_ALL | `reset(EMMC_RESET_ALL)`，**20 次循环 × `delay_us(1000)` = 固定 20 ms 超时** | `reset_all()`，**1000 次 `core::hint::spin_loop()`**（CPU 频率决定实际时长） | 时间预算策略不同：前者绝对时间，后者纯自旋 |
| 3 | 检测 PRESENT_STATE 是否插卡 | ✅ `is_card_present()` 读 `EMMC_PRESENT_STATE` | ❌ 不检测 | `sdhci-host` 假设调用方知道卡在；协议层后续命令失败再报错 |
| 4 | 读 HOST_CONTROL_VER | ✅ 缓存到 `self.version` | ❌ 不读 | |
| 5 | 读 CAPABILITIES1 / CAPABILITIES2 | ✅ 据 spec 版本判断是否取 `caps2` 的 `clock_mul`，重算 `clock_base = (caps & MASK) × 1e6 × clk_mul` | ⚠ `base_clock_hz()` 仅读 caps_low 的 `bits 15..8`，**不处理 `clk_mul`**（v2/v3 共用一段，QEMU sdhci-pci 兼容） | DWCMSHC 等高频控制器 `clk_mul ≠ 0`，`sdhci-host` 在这类硬件上算出的 base 偏小 |
| 6 | clock_base == 0 兜底 | 直接返回 `SdError::UnsupportedCard` | ⚠ `set_clock` 里读到 `base == 0` 才返回 `BadResponse(Phase::Init)`；构造期不检查 | |
| 7 | 设置 `host_caps`（HS \| HS_52MHZ \| 4BIT，按 caps 加 8BIT，再 `\|= 0x48`） | ✅ 内部维护，后续 `mmc_select_card_type` 用它筛选可用速度档 | ❌ 不维护 | 决定 HS200/HS400/DDR52 等是否可选的关键状态，`sdhci-host` 把这事交给协议层 |
| 8 | 选 voltage（3.3 V / 3.0 V / 1.8 V，全无返回 `UnsupportedCard`） | ✅ 根据 caps 自动选最高可用挡 | ❌ 调用方传入 power 字节（README 给的示例是 `POWER_330`） | |
| 9 | Power on（`sdhci_set_power(generic_fls(voltages) - 1)`） | ✅ 内含完整 power cycle 序列 | ⚠ `set_power(byte)` 仅一次性写 `REG_POWER_CONTROL = byte \| POWER_ON`，**不做 off → on 的 power cycle** | 多数 SDHCI 实现允许直接 set；少数控制器需要先关后开 |
| 10 | `NORMAL_INT_STAT_EN = INT_CMD_MASK \| INT_DATA_MASK` | ✅（仅 CMD/DATA 类） | ✅ `enable_interrupts()` 写 `NORMAL_INT_CLEAR_ALL` + `ERROR_INT_CLEAR_ALL`（更激进，**全部状态位都开**） | `sdhci-host` 把错误状态位也开了 — 出错时能拿到完整原因码 |
| 11 | `SIGNAL_ENABLE = 0`（不向 CPU 发 IRQ） | ✅ | ✅ `NORMAL_INT_SIGNAL_ENABLE = 0` + `ERROR_INT_SIGNAL_ENABLE = 0` | 两边都用轮询模型 |
| 12 | 设 1-bit 总线 | ✅ `mmc_set_bus_width(1)` → `sdhci_set_ios` | ⚠ 不显式设，依赖 HOST_CONTROL1 复位默认值 0（== 1-bit） | reset_all 已经把 HOST_CONTROL1 复位为 0，行为一致 |
| 13 | 设 400 kHz ID 速时钟 | ✅ `mmc_set_clock(400_000)` → `sdhci_set_ios` | ✅ `enable_clock(base_hz, 400_000)` | 见 §3 序列对照 |
| 14 | 设 timing = `MMC_TIMING_LEGACY` | ✅ `mmc_set_timing(LEGACY)` → `sdhci_set_ios` | ❌ 控制器层不维护 timing 状态 | timing 决定 HS / HS200 / HS400 等模式标记，`sdhci-host` 由协议层管 |

---

## 3. 时钟启用序列对照

两边都使用 SDHCI v3 的 10-bit divided clock 序列，差异在于
分频值的来源以及"外部时钟模式"的支持。

### 3.1 `drivercraft/sdmmc::sdhci_set_ios()` 的时钟段

```text
（在 mmc_set_clock(target) 之后由 sdhci_set_ios 触发）
1. 关 SD_CLOCK_ENABLE
2. 用 self.clock_base（从 caps 推出来，不可在运行时换）算分频
3. 写 CLOCK_CONTROL = (div<<8) | INTERNAL_CLOCK_EN
4. 轮询 INTERNAL_CLOCK_STABLE
5. 开 SD_CLOCK_ENABLE
6. 同时 sdhci_set_ios 还会刷 HOST_CONTROL1 的 HS bit、bus_width bit
```

特点：
- 分频 base 固定来源于 `EMmcHost::new(base)` 时读出来的 caps，
  之后不能换。
- 不存在"让平台 CRU 接管时钟"的旁路通道 — 高速档想要更高的
  bus 频率，必须 caps 报告了高 base clock，否则只能在
  `mmc_set_bus_speed` 里重算分频。
- Rockchip 平台特化在 `emmc/rockchip.rs`（约 370 行），
  通过 RK3568/RK3588 CRU 拉高 controller 输入时钟以支持
  HS200/HS400。

### 3.2 `sdhci-host::enable_clock()` 的时钟段

```text
fn enable_clock(&mut self, base_clock_hz: u32, target_hz: u32):
1. 写 CLOCK_CONTROL = 0          // 关 SD clock + INTERNAL clock
2. 若 target_hz == 0 → 立刻返回（合法的"停止时钟"）
3. 算 10-bit 分频 div：找最小 N 使 base/(2N) ≤ target，N ∈ [1, 0x3FF]
4. 编码 v3.0 的 10-bit 字段：(div&0xFF)<<8 | (div&0x300)>>2 | INTERNAL_ENABLE
5. 写 CLOCK_CONTROL
6. 1000 次自旋等 INTERNAL_STABLE，超时报 Error::Timeout(Phase::Init)
7. 写 CLOCK_CONTROL |= SD_ENABLE
```

特点：
- 分频 base 在每次调用时由参数传入 —— 调用方可以根据 caps、
  CRU 状态、动态频率切换重算 base。
- 提供了 **`enable_clock_external()` 旁路通道**：把控制器
  内置分频器固定到 1:1（div=0），由平台 CRU 控制实际频率。
  和 `set_external_clock(cb)` 注册的回调配套使用。
- `set_clock(speed)` 实现里会：
  - 翻转 `HOST_CONTROL1` 的 `HIGH_SPEED` bit；
  - 若装了 `ext_clock` 回调 → 走 CRU 路径；
  - 否则 `enable_clock(base_clock_hz(), target_hz)`。

### 3.3 时序与可移植性差异

| 场景 | `drivercraft/sdmmc` | `sdhci-host` |
|------|---------------------|--------------|
| caps 报的 base clock 不对 | 错的 base 会一路传到 HS200/HS400 阶段 | 调用方可以通过 `enable_clock(custom_base, target)` 或 `set_external_clock(cb)` 绕开 |
| 控制器内置分频器不可用（DWC MSHC 某些变种） | 受影响，需要在 Rockchip 平台代码里特化 | `enable_clock_external()` + `set_external_clock(cb)` 是一类模式 |
| 复位等待 | `delay_us(1000) × 20`，时间确定 | `spin_loop × 1000`，时间不确定 | 
| 时钟稳定等待 | `sdhci_set_ios` 内自旋 | `spin_loop × 1000` 后报 `Error::Timeout` | 

---

## 4. 卡识别 / 速度协商对比

> 注：`drivercraft/sdmmc` 把这一段紧跟在控制器 init 后面，
> 都包在同一个 `init()` 方法里；`sdhci-host` 把这一段委托给
> `sdmmc-protocol::SdioSdmmc::init()`。下表只为完整对照。

| 步骤 | `drivercraft/sdmmc` (`init_card` + `mmc_change_freq`) | `sdmmc-protocol` (经 `sdhci-host` 翻译) |
|------|------------------------------------------------------|-----------------------------------------|
| CMD0 GoIdleState | ✅ | ✅ |
| **CMD1 SendOpCond**（eMMC） | ✅ 循环 100 次 OCR 轮询 | ✅（协议层 eMMC 路径） |
| CMD8 SendIfCond（SD） | ❌（eMMC 路径不发） | ✅ SD 路径 |
| CMD2 AllSendCid → CID | ✅ | ✅ |
| CMD3 SendRelativeAddr | ✅ 主动写 RCA = 1（eMMC：host 设定 RCA） | ✅ SD：从 R6 取 RCA；MMC：host 设定 |
| CMD9 SendCsd → CSD V1/V2 解析 | ✅ 算 freq/mult/csize/cmult/dsr_imp | ✅ |
| CMD7 SelectCard | ✅ | ✅ |
| **CMD8 ReadExtCsd** + EXT_CSD 解析 | ✅ 容量 / partition / boot / RPMB / GP / enhanced area / wr_rel_set / driver_strength | ❌ 当前协议层未实现 |
| `mmc_select_hs` (CMD6 切 HS) → 升 52 MHz | ✅ | ✅ 通过 `set_clock(HighSpeed)` |
| `mmc_select_card_type` 决定 HS200/HS400/HS400ES | ✅ 综合 host_caps + EXT_CSD CARD_TYPE | ❌ 协议层未编排 |
| `mmc_select_bus_width`（自动协商 8 → 4 → 1） | ✅ 通过写 EXT_CSD + 回读校验 | ⚠ `set_bus_width(Bit8)` 在 `sdhci-host` 里返回 `UnsupportedCommand` |
| HS200 tuning（CMD21） | ✅ `mmc_hs200_tuning`：写 EXEC_TUNING，循环 ≤ 40 次检 EXEC_TUNING/TUNED_CLK | ✅ `execute_tuning(cmd_index=21)` 已就绪，但需协议层调度 |
| 1.8 V 切换（CMD11） | ❌ | ✅ `switch_voltage(SignalVoltage)` 已就绪 |

---

## 5. 总结

**控制器 bring-up 不一致**：

- `drivercraft/sdmmc` 把"读 caps + 选 voltage + 设 host_caps +
  power on + 中断 + 1-bit + 400 kHz + LEGACY timing"全部塞进
  `init()` 一次做完，行为更接近"开箱即用的 SDHCI 引导"。
- `sdhci-host` 把这些拆成 `reset_all` / `set_power` /
  `enable_interrupts` / `enable_clock` 四个独立函数，并把
  voltage / host_caps / timing 维护推给上层（README 的
  bring-up checklist 描述顺序）。
- `drivercraft/sdmmc` 在复位等待用 `delay_us(1000) × 20`
  绝对时间，`sdhci-host` 用 1000 次 `core::hint::spin_loop()`
  自旋（CPU 频率决定实际时长，超时分支统一返回 `Error::Timeout`）。
- `drivercraft/sdmmc` 处理 `CAPABILITIES2.clock_mul`，DWCMSHC
  这类高频控制器才能算对 `clock_base`；`sdhci-host::base_clock_hz()`
  目前不处理 `clk_mul`，要在 DWCMSHC 上跑必须改用
  `set_external_clock(cb)` 让 CRU 接管时钟。

**卡识别 / 速度协商不一致**：

- `drivercraft/sdmmc` 自带完整 eMMC 4.x/5.x 路径（CMD1 + EXT_CSD
  解析 + partition/boot/RPMB + HS200/HS400/HS400ES + tuning +
  自动 bus width 协商），且在 RK3568/RK3588 上验证过。
- `sdhci-host` 把这一段交给 `sdmmc-protocol`，目前协议层覆盖到
  SD 的 ID/HS 路径；eMMC 高速档与 EXT_CSD 解析的"协议状态机"
  尚未补齐，控制器层（`switch_voltage` / `execute_tuning`）已就绪。

> 如果要把两边行为对齐：
>
> - `sdhci-host` 想追上 eMMC 全速档，主要工作不在控制器层，
>   而在 `sdmmc-protocol` 内补 EXT_CSD 解析、HS200/HS400
>   协议状态机、partition 模型；控制器侧只需补 `clk_mul`
>   处理与可选的 power cycle。
> - `drivercraft/sdmmc` 想要变得可移植，需要把 `rockchip.rs`
>   抽成 trait、去掉 `aarch64-cpu` / `smccc` / `arm_pl011`
>   依赖、并把 SDHCI 寄存器层和 eMMC 协议层拆成两个 crate。

---

## 6. 参考代码位置

| 内容 | `drivercraft/sdmmc` | `sdhci-host` |
|------|---------------------|--------------|
| 控制器 bring-up 入口 | `src/emmc/mod.rs::EMmcHost::init` | `crates/sdhci-host/src/host.rs::Sdhci::{reset_all, set_power, enable_interrupts, enable_clock}` + `lib.rs::SdioHost impl` |
| 软复位 | `EMmcHost::reset(mask)` | `Sdhci::reset_with_mask(mask, phase)` |
| 时钟编程 | `sdhci_set_ios`（在 `emmc/` 子模块中） | `Sdhci::enable_clock` / `enable_clock_external` |
| 卡识别 | `EMmcHost::init_card` | `sdmmc-protocol::SdioSdmmc::init`（不在本 crate） |
| EXT_CSD 解析 + partition | `EMmcHost::init_card` 后半段 + `mmc_change_freq` | 不实现 |
| HS200 tuning | `EMmcHost::mmc_hs200_tuning` + `__emmc_execute_tuning` | `Sdhci::execute_tuning`（控制器层） |
| 1.8 V 切换 | 未实现 | `Sdhci::switch_voltage` |
| 平台时钟特化 | `src/emmc/rockchip.rs`（RK3568/RK3588 CRU） | 通过 `set_external_clock(cb)` 由调用方提供回调 |
