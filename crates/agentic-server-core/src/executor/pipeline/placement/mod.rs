//! Experimental #245 correctness guards, not a production placement policy.
//! Both arms call the actual synchronous `RoundIngestion`, including translation.
//! No timing recommendation may be inferred from these contract tests.
#![cfg(test)]

mod arrival;
mod driver;
mod fault_probe;
mod fixtures;
mod native;
mod tests;
