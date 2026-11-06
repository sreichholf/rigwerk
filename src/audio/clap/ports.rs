//! CLAP audio-port layout and buffers.

use std::ptr;

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT,
    CLAP_AUDIO_PORTS_RESCAN_FLAGS, CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR,
    CLAP_AUDIO_PORTS_RESCAN_LIST, CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE,
    CLAP_AUDIO_PORT_IS_MAIN,
};
use clap_sys::plugin::clap_plugin;

/// What a slot shows once the plugin has declared audio ports that no longer
/// match the buffers it was loaded with.
pub(crate) const AUDIO_PORTS_CHANGED: &str =
    "The CLAP plugin changed its audio ports; remove it and add it again.";

/// Rescan flags that cannot invalidate the buffers: port names and metadata,
/// and the in-place pairing, which this host never uses because every port has
/// its own buffers.
pub(super) const AUDIO_PORTS_RESCAN_HARMLESS: u32 = CLAP_AUDIO_PORTS_RESCAN_NAMES
    | CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE
    | CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR;

/// Rescan flags whose change would leave the buffers wrongly sized or wired:
/// a different port list, channel count, or port flags needs another load.
pub(super) const AUDIO_PORTS_RESCAN_STOPS: u32 = CLAP_AUDIO_PORTS_RESCAN_LIST
    | CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT
    | CLAP_AUDIO_PORTS_RESCAN_FLAGS;

pub(super) const AUDIO_PORTS_RESCAN_ALL: u32 =
    AUDIO_PORTS_RESCAN_HARMLESS | AUDIO_PORTS_RESCAN_STOPS;

/// The most audio ports per direction, and channels per port, RigWerk sets up
/// buffers for.  Anything larger is a broken description, not an effect.
pub(super) const MAX_AUDIO_PORTS: usize = 32;
pub(super) const MAX_PORT_CHANNELS: usize = 64;

/// What [`port_layout`] needs to know about one declared audio port.  The
/// channel count is authoritative (the port type only names a common
/// layout), and every port gets its own buffers, so `in_place_pair` never
/// matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PortInfo {
    pub(super) channel_count: u32,
    pub(super) flags: u32,
}

/// The ports `process` hands the plugin: one entry per declared port, in the
/// plugin's index order, with its channel count.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PortLayout {
    pub(super) inputs: Vec<usize>,
    pub(super) outputs: Vec<usize>,
    /// The input that gets the stereo signal; every other input (a
    /// sidechain, say) gets silence.
    pub(super) main_input: usize,
    /// The output that is read back; the others are ignored.
    pub(super) main_output: usize,
}

/// Decides the buffers for the ports a plugin declares.  `None` means the
/// plugin has no audio-ports extension.  The CLAP spec says such a plugin
/// has no ports at all, but RigWerk has always given it one stereo input
/// and output, which is what simple effects without the extension expect.
///
/// The main port is the one flagged `CLAP_AUDIO_PORT_IS_MAIN` (the spec puts
/// it at index 0, but the flag wins wherever it is); with no flag, index 0
/// stands in.  A main port with one channel gets the mono sum and its output
/// goes to both sides; one with more than two gets left and right on its
/// first two channels and silence on the rest, and only its first two
/// output channels are read.
pub(super) fn port_layout(ports: Option<(&[PortInfo], &[PortInfo])>) -> Result<PortLayout, String> {
    let Some((inputs, outputs)) = ports else {
        return Ok(PortLayout {
            inputs: vec![2],
            outputs: vec![2],
            main_input: 0,
            main_output: 0,
        });
    };
    let (inputs, main_input) = direction_layout(inputs, "input")?;
    let (outputs, main_output) = direction_layout(outputs, "output")?;
    Ok(PortLayout {
        inputs,
        outputs,
        main_input,
        main_output,
    })
}

pub(super) fn direction_layout(
    ports: &[PortInfo],
    direction: &str,
) -> Result<(Vec<usize>, usize), String> {
    if ports.is_empty() {
        return Err(format!("This CLAP plugin has no audio {direction}."));
    }
    if ports.len() > MAX_AUDIO_PORTS {
        return Err(format!(
            "This CLAP plugin declares more than {MAX_AUDIO_PORTS} audio {direction}s."
        ));
    }
    let counts: Vec<usize> = ports
        .iter()
        .map(|port| port.channel_count as usize)
        .collect();
    if counts.iter().any(|count| *count > MAX_PORT_CHANNELS) {
        return Err(format!(
            "This CLAP plugin declares an audio {direction} with more than {MAX_PORT_CHANNELS} channels."
        ));
    }
    let main = ports
        .iter()
        .position(|port| port.flags & CLAP_AUDIO_PORT_IS_MAIN != 0)
        .unwrap_or(0);
    if counts[main] == 0 {
        return Err(format!(
            "This CLAP plugin's main audio {direction} has no channels."
        ));
    }
    Ok((counts, main))
}

/// Input and output ports as a plugin declares them.
pub(super) type DeclaredPorts = (Vec<PortInfo>, Vec<PortInfo>);

/// Reads every audio port the plugin declares, for [`port_layout`].  A
/// main-thread call, made while the plugin is inactive.
pub(super) fn read_audio_ports(
    plugin: *const clap_plugin,
    ext: *const clap_plugin_audio_ports,
) -> Result<Option<DeclaredPorts>, String> {
    let Some(ext) = (unsafe { ext.as_ref() }) else {
        return Ok(None);
    };
    let (Some(count), Some(get)) = (ext.count, ext.get) else {
        return Err("This CLAP plugin's audio ports cannot be read.".to_owned());
    };
    let read = |is_input: bool| -> Result<Vec<PortInfo>, String> {
        let total = unsafe { count(plugin, is_input) } as usize;
        // Checked before asking for each port, so a nonsense count cannot
        // make the host allocate or loop for long.
        if total > MAX_AUDIO_PORTS {
            return Err(format!(
                "This CLAP plugin declares more than {MAX_AUDIO_PORTS} audio ports."
            ));
        }
        let mut ports = Vec::with_capacity(total);
        for index in 0..total as u32 {
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            // A port that cannot be described still exists at its index, so
            // skipping it would shift every later buffer onto the wrong port.
            if !unsafe { get(plugin, index, is_input, &mut info) } {
                return Err(
                    "This CLAP plugin could not describe one of its audio ports.".to_owned(),
                );
            }
            ports.push(PortInfo {
                channel_count: info.channel_count,
                flags: info.flags,
            });
        }
        Ok(ports)
    };
    Ok(Some((read(true)?, read(false)?)))
}

/// The preallocated buffers for one direction: one `clap_audio_buffer` per
/// declared port, each pointing at that port's channels.  Nothing here
/// allocates after [`PortBuffers::new`].
pub(super) struct PortBuffers {
    /// Every channel of every port, in port order, each `max_block` long.
    pub(super) channels: Vec<Vec<f32>>,
    /// The channel pointers the `clap_audio_buffer`s point into.
    pub(super) pointers: Vec<*mut f32>,
    pub(super) ports: Vec<clap_audio_buffer>,
    /// Index into `channels` of each port's first channel.
    pub(super) first: Vec<usize>,
    /// Each port's channel count.
    pub(super) counts: Vec<usize>,
    /// The main port's index.
    pub(super) main: usize,
}

impl Default for PortBuffers {
    fn default() -> Self {
        Self::new(&[], 0, 0)
    }
}

impl PortBuffers {
    pub(super) fn new(counts: &[usize], main: usize, block: usize) -> Self {
        let total = counts.iter().sum();
        let mut first = Vec::with_capacity(counts.len());
        let mut next = 0;
        for count in counts {
            first.push(next);
            next += count;
        }
        let mut buffers = Self {
            channels: vec![vec![0.0; block]; total],
            pointers: vec![ptr::null_mut(); total],
            ports: counts
                .iter()
                .map(|count| clap_audio_buffer {
                    data32: ptr::null_mut(),
                    data64: ptr::null_mut(),
                    channel_count: *count as u32,
                    latency: 0,
                    constant_mask: 0,
                })
                .collect(),
            first,
            counts: counts.to_vec(),
            main,
        };
        buffers.refresh();
        buffers
    }

    /// The channels of the main port.  Empty when the buffers have not been
    /// set up (only [`PortBuffers::default`]), so a stray call cannot panic.
    pub(super) fn main_channels(&mut self) -> &mut [Vec<f32>] {
        let (Some(first), Some(count)) = (self.first.get(self.main), self.counts.get(self.main))
        else {
            return &mut [];
        };
        &mut self.channels[*first..*first + *count]
    }

    /// Clears the first `frames` samples of every channel.
    pub(super) fn silence(&mut self, frames: usize) {
        for channel in &mut self.channels {
            channel[..frames].fill(0.0);
        }
    }

    /// Points every port at its channels again.  The plugin gets mutable
    /// pointers and could have changed any of them (or set a constant
    /// mask), so this runs before each block.
    pub(super) fn refresh(&mut self) {
        for (pointer, channel) in self.pointers.iter_mut().zip(&mut self.channels) {
            *pointer = channel.as_mut_ptr();
        }
        let base = self.pointers.as_mut_ptr();
        for (index, port) in self.ports.iter_mut().enumerate() {
            let count = self.counts[index];
            // A zero-channel port gets a null pointer rather than one past the
            // end of `pointers`; a plugin that checks the pointer instead of
            // the count then sees the empty port.
            port.data32 = if count == 0 {
                ptr::null_mut()
            } else {
                // In bounds: `first + count <= pointers.len()`.
                unsafe { base.add(self.first[index]) }
            };
            port.data64 = ptr::null_mut();
            port.channel_count = count as u32;
            port.latency = 0;
            port.constant_mask = 0;
        }
    }
}
