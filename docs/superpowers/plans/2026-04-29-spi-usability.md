# SPI Usability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the SPI initialization and read-token error paths protocol-correct enough to test without hardware.

**Architecture:** Keep the public API stable. Add focused response helpers for SPI R1 semantics, fix SPI initialization polling, and test via a scripted `SpiTransport`.

**Tech Stack:** Rust 2024, `no_std`, `embedded-hal` 1, Cargo tests.

---

### Task 1: SPI R1 Semantics

**Files:**
- Modify: `src/response.rs`
- Test: `src/response.rs`

- [ ] Add tests that prove SPI R1 idle and illegal-command bits are parsed from bits 0 and 2.
- [ ] Run the response tests and verify they fail against current parsing.
- [ ] Update `R1Response` helpers and `from_raw` so 8-bit SPI R1 values remain usable.
- [ ] Run the response tests and verify they pass.

### Task 2: SPI Initialization Flow

**Files:**
- Modify: `src/spi.rs`
- Test: `src/spi.rs`

- [ ] Add a scripted SPI transport test for CMD0, CMD8, ACMD41 polling, and CMD58.
- [ ] Run the SPI init test and verify it fails because ACMD41 is interpreted as R3.
- [ ] Change SPI ACMD41 polling to use R1 idle-bit completion and keep CMD58 as OCR read.
- [ ] Run the SPI init test and verify it passes.

### Task 3: Data Token Timeout

**Files:**
- Modify: `src/spi.rs`
- Test: `src/spi.rs`

- [ ] Add a scripted SPI transport test where CMD17 succeeds but no data token arrives.
- [ ] Run the test and verify it fails because the driver reads a block anyway.
- [ ] Return `Error::Timeout` when no start block token is observed.
- [ ] Run all tests and verify they pass for default, SDIO-only, and all-features builds.

