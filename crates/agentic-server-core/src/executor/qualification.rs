//! Offline end-to-end execution of recorded pinned-provider responses.
//! This module and its loopback transport authority exist only under `cfg(test)`.
//! These tests are not live gateway or WebSocket transport qualification.

mod continuations;
mod failures;
mod support;
mod tools;
