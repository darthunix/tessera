//! The boundary between Tessera's Rust implementation and its C callers.
//!
//! This crate produces a static library but does not expose C functions yet.
//! Future entry points must catch unwinding panics before returning to C.
//! Rust must not call PostgreSQL, directly or through callbacks.
//! Every unsafe block must explain its safety requirements in a SAFETY comment.
