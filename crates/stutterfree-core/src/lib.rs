//! Shaderless core library (Rust port of SCSKiller.Linux.Core plus the parts of SCSKiller.Core it uses).
//!
//! Phase 1 of the port: everything the Linux app does except the static engine readers and the planner
//! (Unreal / Unity / FromSoftware / RE Engine / REDengine / Dagor / Northlight readers). Compiling therefore works from a
//! Record-mode recording; [`engines`] is the hook where the static readers plug in.

pub mod coordinator;
pub mod drivers;
pub mod dxbc;
pub mod engine_detector;
pub mod engines;
pub mod game_manager;
pub mod gamefiles;
pub mod models;
pub mod monitor;
pub mod nvcache;
pub mod paths;
pub mod proton;
pub mod psodb;
pub mod recorder;
pub mod recordings;
pub mod settings;
pub mod sources;
pub mod util;

pub use models::*;
