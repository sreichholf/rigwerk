//! PipeWire is the client path.  `hw:` and `plughw:` are exclusive ALSA.
//! JACK runs only when cpal provides a host.

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
