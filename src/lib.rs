//! mcl engine.
//!
//! Everything the launcher does lives here so that a front-end (the Trenchlight
//! window today, a native Rust UI later) can call it in-process instead of
//! shelling out to `mcl.exe`. The CLI in `main.rs` is one consumer of this
//! crate; `src/bin/trenchlight.rs` is another.

pub mod app;
pub mod auth;
pub mod download;
pub mod horizon;
pub mod install;
pub mod instance;
pub mod java;
pub mod launch;
pub mod manifest;
pub mod paths;
pub mod settings;
pub mod skin;
pub mod util;
pub mod version;
