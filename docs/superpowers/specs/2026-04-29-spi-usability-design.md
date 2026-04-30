# SPI Usability Design

## Goal

Improve the SPI path of this `no_std` SD/MMC protocol crate so initialization and block reads have protocol-level tests and avoid known incorrect behavior.

## Scope

This phase focuses on `src/spi.rs`, `src/response.rs`, and tests. SDIO remains structurally unchanged except for compile compatibility.

## Design

SPI command responses use the SPI R1 byte semantics during SPI transactions. Initialization sends clock preamble, enters idle with CMD0, probes SD v2 with CMD8, waits for ACMD41 by polling the R1 idle bit until it clears, and reads OCR with CMD58 to determine high capacity.

Data block reads wait for a start block token. If the token is not observed before the retry limit, the operation returns an error instead of continuing to read a fake block.

Tests use an in-memory scripted `SpiTransport` to verify command bytes and card responses without hardware.

