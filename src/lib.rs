//! poodpim: Thai speech-to-text with iapp/OpenThai2.0-ASR-Streaming.
//!
//! The core (features, ASR, audio helpers, model paths) builds everywhere. The
//! desktop app lives behind the `desktop` feature; the Lambda server behind `lambda`.

pub mod asr;
pub mod corrections;
pub mod audio;
pub mod features;
pub mod hotwords;
pub mod model;
pub mod numbers;
pub mod pipeline;
pub mod segment;

#[cfg(feature = "desktop")]
pub mod app;
#[cfg(feature = "desktop")]
pub mod config;
#[cfg(feature = "desktop")]
pub mod filejob;
#[cfg(feature = "desktop")]
pub mod inserter;
#[cfg(feature = "desktop")]
pub mod overlay;
#[cfg(feature = "desktop")]
pub mod platform;
#[cfg(feature = "desktop")]
pub mod remote;
#[cfg(feature = "desktop")]
pub mod settings;
#[cfg(feature = "desktop")]
pub mod update;
