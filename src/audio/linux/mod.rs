//! Linux audio backends. PipeWire is the primary path: RigWerk is a client
//! with its own capture and playback nodes. Direct ALSA hardware (`hw:` and
//! `plughw:`) is the exclusive path. A JACK host is used only when cpal
//! actually provides one.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

pub(crate) mod alsa;
#[cfg(target_os = "linux")]
pub(crate) mod duplex;
#[cfg(target_os = "linux")]
pub(crate) mod hotplug;
#[cfg(target_os = "linux")]
pub(crate) mod native;
pub(crate) mod pipewire;
#[cfg(target_os = "linux")]
pub(crate) mod sink_monitor;
