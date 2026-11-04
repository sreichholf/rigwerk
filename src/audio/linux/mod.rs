//! Linux audio backends.  PipeWire is the default; direct ALSA hardware and
//! JACK remain available as the lower-level, exclusive paths.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

pub(crate) mod alsa;
pub(crate) mod pipewire;
