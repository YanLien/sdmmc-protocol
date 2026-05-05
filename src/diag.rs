//! Diagnostic logging shim that routes to `defmt`, `log`, or to no-ops
//! depending on which feature is enabled.
//!
//! When both features are active `defmt` wins (it is the more useful one for
//! the embedded-target deployments this crate primarily serves). When neither
//! is active these macros expand to nothing — no allocation, no formatting,
//! no code-size cost.

#![allow(unused_macros, unused_imports)]

#[cfg(feature = "defmt")]
macro_rules! trace {
    ($($arg:tt)*) => { ::defmt::trace!($($arg)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! trace {
    ($($arg:tt)*) => { ::log::trace!($($arg)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! trace {
    ($($arg:tt)*) => {};
}

#[cfg(feature = "defmt")]
macro_rules! debug {
    ($($arg:tt)*) => { ::defmt::debug!($($arg)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! debug {
    ($($arg:tt)*) => { ::log::debug!($($arg)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! debug {
    ($($arg:tt)*) => {};
}

#[cfg(feature = "defmt")]
macro_rules! info {
    ($($arg:tt)*) => { ::defmt::info!($($arg)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! info {
    ($($arg:tt)*) => { ::log::info!($($arg)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! info {
    ($($arg:tt)*) => {};
}

#[cfg(feature = "defmt")]
macro_rules! warn_ {
    ($($arg:tt)*) => { ::defmt::warn!($($arg)*) };
}
#[cfg(all(feature = "log", not(feature = "defmt")))]
macro_rules! warn_ {
    ($($arg:tt)*) => { ::log::warn!($($arg)*) };
}
#[cfg(not(any(feature = "defmt", feature = "log")))]
macro_rules! warn_ {
    ($($arg:tt)*) => {};
}

pub(crate) use {debug, info, trace, warn_};
