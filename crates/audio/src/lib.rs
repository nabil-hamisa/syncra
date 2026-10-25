//! Audio streaming: host loopback capture, client playback, the jitter
//! buffer between them, and Opus framing helpers.
//!
//! Frames are 5 ms of 48 kHz stereo (see `proto::audio`), encoded with
//! Opus in restricted-low-delay (CELT) mode: 2.5 ms look-ahead.

pub mod jitter;
pub mod resample;

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod render;
#[cfg(windows)]
pub mod wasapi;

pub use opus_sys::{Decoder, Encoder};
