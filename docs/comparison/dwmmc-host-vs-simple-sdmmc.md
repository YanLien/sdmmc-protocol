# `dwmmc-host` 与 `simple-sdmmc` 初始化流程对比

本文档逐条对照本仓库 `crates/dwmmc-host` 与上游
[`Starry-OS/simple-sdmmc`](https://github.com/Starry-OS/simple-sdmmc)
两份驱动在**初始化阶段**的行为差异。两者都驱动同一颗 IP（Synopsys
DesignWare Mobile Storage Host Controller，简称 dw_mshc / dw_mmc），
但在初始化的**职责划分**和**具体步骤**上都不一致。

> 关键事实：`simple-sdmmc::SdMmc::new` 是「控制器 init + SD 卡
> enumeration」一次性做完；`dwmmc-host::DwMmc::reset_and_init`
> 只做控制器 init，卡 enumeration 由 `sdmmc-protocol::SdioSdmmc::init()`
> 完成。可对比的只有控制器 init 这一段是同层；卡 enumeration
> 这一段属于不同抽象层的同名步骤。

---

## 1. 职责划分

| 阶段 | `simple-sdmmc` | `dwmmc-host` |
|------|----------------|--------------|
| 控制器复位 / 时钟编程 / 总线宽度 | ✅ 在 `SdMmc::new()` 里做 | ✅ 在 `DwMmc::reset_and_init()` 里做 |
| SD 卡识别（CMD0/8/ACMD41/2/3/9/7/ACMD51） | ✅ 同样在 `SdMmc::new()` 里做 | ❌ 不做，交给 `SdioSdmmc::init()`（位于 `sdmmc-protocol`） |
| MMC/eMMC 路径（CMD1） | ❌ 不支持 | ✅ 通过协议层支持 |
| 切到 4-bit / High Speed | ❌ 永远 1-bit + ID 速 | ✅ 由协议层根据卡能力切换，调用 host 的 `set_bus_width` / `set_clock` |

> 说明：因为 `simple-sdmmc` 把这两件事打包做了，所以它对外只有
> 一个 `SdMmc::new(base)`；而 `dwmmc-host` 必须先 `DwMmc::new` →
> `set_reference_clock` → `reset_and_init`，再交给 `SdioSdmmc::new(host, delay).init()`
> 才算"等价"。

---

## 2. 控制器 init 步骤逐项对照

下表按实际代码顺序展开。"❌"表示该步未做，"✅"表示做了，
"⚠"表示做了但行为有差异。

| # | 步骤 | `simple-sdmmc` | `dwmmc-host` | 备注 |
|---|------|----------------|--------------|------|
| 1 | 关 CCLK_ENABLE | ✅ `clkena.write(0)` | ✅ `clkena.write(0)` | |
| 2 | 关 `use_internal_dmac` | ⚠ 在第 9 步随 `dma_reset` 一起做 | ✅ 单独提前关掉 | |
| 3 | 关 `dma_enable` | ❌ | ✅ | `simple-sdmmc` 没显式清这一位 |
| 4 | 关 `int_enable`（全局中断使能） | ❌ | ✅ | |
| 5 | `controller_reset = 1` | ❌ **没做** | ✅ | 完整 CIU 复位，影响最大 |
| 6 | `fifo_reset = 1` | ❌ **没做** | ✅ | FIFO 指针不被复位，遗留数据可能影响首次传输 |
| 7 | `dma_reset = 1` | ✅ | ✅ | |
| 8 | `bmod.swr = 1`（IDMAC 软复位） | ✅ | ❌ | 因 PIO 模式下 IDMAC 不被使用，影响有限 |
| 9 | 等待复位位自清 | ❌（直接进入下一步） | ✅ 1M 次自旋后报 `Timeout(Phase::Init)` | dw_mshc 的复位位是异步自清的，不等可能导致后续写竞态 |
| 10 | `INTMASK = 0`（屏蔽所有中断） | ❌ | ✅ | 驱动用轮询 RINTSTS，不应让中断触发 |
| 11 | 主动清 RINTSTS | ⚠ 仅在 `init()` 末尾清一次 | ✅ 复位后就清 | |
| 12 | `CTYPE = 0`（1-bit 总线） | ✅ | ✅ | 两边都默认 1-bit，等待协议层切宽 |
| 13 | 编程 CLKDIV | ⚠ **硬写** `clk_divider0 = 4`，假设外部参考时钟 | ✅ 按 `ref_clock_hz / (2 × 400 kHz)` 计算，并对 8-bit 字段饱和到 `0xFF` | 见 §3 时钟更新序列 |
| 14 | 推送时钟改动到 CIU | ⚠ 2 次 update（关 cclk + ResetClock；写 div + 开 cclk + ResetClock） | ✅ 3 次 update（关 cclk → 推 → 写 div → 推 → 开 cclk → 推） | 见 §3 |
| 15 | 时钟操作带超时保护 | ❌ `wait_until` 无超时，硬件异常时会死循环 | ✅ 1M 次自旋后报 `Timeout(Phase::Init)` | |

---

## 3. 时钟更新序列对照

dw_mshc 文档建议**任何对 CLKDIV / CLKSRC / CLKENA 的修改都必须
在 CCLK_ENABLE = 0 时进行，并通过 `update_clock_registers_only`
+ `start_cmd` 把改动推送到 CIU**。两边在这一段的执行序列不同。

### 3.1 `simple-sdmmc::init()` 的时钟段

```text
clkena.write(0)                       // 关 cclk
send_cmd(ResetClock)                  // update #1：把"关 cclk"推到 CIU
clkdiv.write(divider=4)               // 写分频（此时未推送）
clkena.write(cclk_enable=1)           // 开 cclk
send_cmd(ResetClock)                  // update #2：把"写 div + 开 cclk"一起推到 CIU
```

特点：
- 总共 2 次 update。
- 「写 div」与「开 cclk」在同一次 update 里推送 —— 严格意义上违反
  「改 CLKDIV 时 cclk 必须为 0」的时序约束，但实际硬件通常容忍，
  因为 update 推送到 CIU 时寄存器值已经是「div 新值 + cclk 即将打开」。
- `ResetClock` 命令本身不带超时（基于 `wait_until` 自旋）。

### 3.2 `dwmmc-host::program_clock()` 的时钟段

```text
clkena.write(0)                       // 关 cclk
send_update_clock()                   // update #1：先把"关 cclk"落地
clkdiv.write(divider=ceil(ref_hz/(2*target_hz)))  // div 仅在 cclk=0 时写
send_update_clock()                   // update #2：把新 div 推到 CIU
clkena.write(cclk_enable=1)           // 开 cclk
send_update_clock()                   // update #3：把"开 cclk"推到 CIU
```

特点：
- 严格三段式 update：先确保 cclk 已经关停，再单独推送 div，最后再
  开 cclk。
- 每一次 `send_update_clock()` 都用 `start_cmd=1 + update_clock_registers_only=1
  + wait_prvdata_complete=1` 写 CMD，并轮询 `start_cmd` 自清，超过
  1M 次自旋报 `Timeout(Phase::Init)`。
- 分频值由 `set_reference_clock(ref_hz)` + 目标频率换算，初始化
  阶段目标 = 400 kHz；如果 `ref_hz == 0` 则退回 1:1 直通，假设
  平台 CRU 已经把频率调到位。

### 3.3 时序差异的实际影响

| 场景 | `simple-sdmmc` 行为 | `dwmmc-host` 行为 |
|------|--------------------|-------------------|
| 外部参考时钟 ≠ 假设值 | 实际 SCLK 偏离 400 kHz，可能导致 CMD0/CMD8 时序不达标 | 自动换算分频，仍能命中 ≈ 400 kHz |
| 复位后 div 残留 | 不显式 update 关 cclk，直接写 div 时 cclk 可能仍处于过渡态 | update #1 已经把 cclk 落地为 0 后才写 div |
| 控制器无响应 | `wait_until` 死循环 | 1M 次自旋后返回 `Error::Timeout` |

---

## 4. 卡 enumeration 段对比（仅 SD 卡场景）

> 注：本节严格说不属于"控制器 init"，但因 `simple-sdmmc` 把它放在
> 同一个 `init()` 函数里，所以列出来便于完整对照。

| 步骤 | `simple-sdmmc` (`init()` 后半段) | `sdmmc-protocol` (`SdioSdmmc::init()`) |
|------|----------------------------------|----------------------------------------|
| CMD0 GoIdleState | ✅ | ✅ |
| CMD8 SendIfCond(0x1AA) | ✅，校验失败直接 `assert!` panic | ✅，校验失败走 v1.x 卡分支 |
| 循环 CMD55 + ACMD41(0x41FF_8000) | ✅，无超时 | ✅，带重试与超时 |
| CMD2 AllSendCid → 解析 CID | ✅，`transmute` 到 bitfield 结构体 | ✅ |
| CMD3 SendRelativeAddr → 取 RCA | ✅ | ✅ |
| CMD9 SendCsd → 解析 CSD | ⚠ 强转 `CsdV2`，**SDSC（CSD V1）卡会被错误解码** | ✅ 区分 V1 / V2 |
| CMD7 SelectCard | ✅ | ✅ |
| CMD55 + ACMD51 SendScr | ✅，但读完 SCR 不据此切 4-bit | ✅，读完后据 SCR 决定是否 ACMD6 切 4-bit |
| CMD6 切 High Speed | ❌ | ✅，调 host 的 `set_clock(HighSpeed)` |
| ACMD6 切 4-bit 总线 | ❌ 永远 1-bit | ✅，调 host 的 `set_bus_width(Bit4)` |

---

## 5. 总结

**控制器 init 不一致**：

- `dwmmc-host` 的复位更彻底 —— 多了 `controller_reset` + `fifo_reset`
  + `INTMASK = 0` + RINTSTS 主动清理 + 全部 wait 带超时。
- `dwmmc-host` 的时钟更新走严格三段式（关 cclk → 推 → 写 div → 推
  → 开 cclk → 推），并按 `ref_clock_hz` 自动换算分频。
- `simple-sdmmc` 多做了 `bmod.swr = 1`（IDMAC 软复位），但因为
  两者都禁用 IDMAC，这步意义不大。
- `simple-sdmmc` 的 CLKDIV 硬写 4，依赖外部时钟"刚好"是某个特定
  频率才能得到 ≈ 400 kHz；`dwmmc-host` 通过 `set_reference_clock`
  解耦了平台时钟与目标频率。

**卡 enumeration 不一致**：

- `simple-sdmmc` 自带一份"够用版"：仅识别 SDHC、永远 1-bit、
  始终运行在 ID 速（≈ 400 kHz），出错就 panic。
- `dwmmc-host` 把这步交给 `sdmmc-protocol`，覆盖更全：SD/MMC
  双路径、4-bit 切换、HS 切换、CSD V1/V2 区分、错误返回标准
  `Error` 类型。

> 如果需要把两边行为对齐：要么在 `simple-sdmmc` 的 `init()` 里
> 补齐 `controller_reset` / `fifo_reset` / `INTMASK = 0` /
> 复位等待 / 时钟超时；要么直接采用 `dwmmc-host` +
> `sdmmc-protocol::SdioSdmmc` 这套分层方案。

---

## 6. 参考代码位置

| 内容 | `simple-sdmmc` | `dwmmc-host` |
|------|----------------|--------------|
| 控制器 init 入口 | `src/sdmmc.rs::SdMmc::init` | `crates/dwmmc-host/src/host.rs::DwMmc::reset_and_init` |
| 时钟编程 | `src/sdmmc.rs::SdMmc::init`（嵌在 init 中） | `crates/dwmmc-host/src/host.rs::DwMmc::program_clock` + `send_update_clock` |
| 卡 enumeration | `src/sdmmc.rs::SdMmc::init` 后半段 | `sdmmc-protocol` 的 `SdioSdmmc::init`（不在本 crate） |
| 命令构造 | `src/cmd.rs::Command::build` | `crates/dwmmc-host/src/command.rs` |
