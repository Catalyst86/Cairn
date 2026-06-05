#![no_std]
#![forbid(unsafe_code)]

pub mod capability;
pub mod table;

#[cfg(feature = "kani")]
pub mod verification;

#[cfg(test)]
mod tests;
