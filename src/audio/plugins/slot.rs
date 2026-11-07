//! Insert slots and deferred plugin teardown.

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::super::clap::{HostState, AUDIO_PORTS_CHANGED};
use super::super::delay::{Declick, DelayLine, DelayTap};
use super::load::{
    host_has_editor, host_parameters, load_plugin, PluginHandle, Vst3Service, MAX_BLOCK,
};
use super::pref::{PluginParamView, PluginPref, PluginView};
use super::MAX_ALIGN;

/// An insert's mix and bypass as atomics: the UI sets them without the
/// chain lock and the audio thread reads them every chunk, so dragging the
/// mix or toggling bypass never makes the audio thread miss the chain.
/// The slot's `pref.mix` / `pref.bypass` are only the values it was made
/// with; [`Slot::saved_pref`] and the views read these.
pub(crate) struct SlotControls {
    mix: AtomicU32,
    bypass: AtomicBool,
}

impl SlotControls {
    fn new(pref: &PluginPref) -> Self {
        Self {
            mix: AtomicU32::new(pref.mix.clamp(0.0, 1.0).to_bits()),
            bypass: AtomicBool::new(pref.bypass),
        }
    }

    pub(crate) fn mix(&self) -> f32 {
        f32::from_bits(self.mix.load(Ordering::Relaxed))
    }

    pub(crate) fn set_mix(&self, mix: f32) {
        let mix = if mix.is_nan() {
            1.0
        } else {
            mix.clamp(0.0, 1.0)
        };
        self.mix.store(mix.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn bypass(&self) -> bool {
        self.bypass.load(Ordering::Relaxed)
    }

    pub(crate) fn set_bypass(&self, bypass: bool) {
        self.bypass.store(bypass, Ordering::Relaxed);
    }
}

pub(crate) struct Slot {
    pub(crate) pref: PluginPref,
    /// Mix and bypass as the UI sets them (see [`SlotControls`]).  Kept
    /// when the slot is reopened or rebuilt for a new rate.
    pub(crate) controls: Arc<SlotControls>,
    pub(crate) plugin: Option<PluginHandle>,
    pub(crate) error: Option<String>,
    pub(crate) parameters: Vec<PluginParamView>,
    /// Read once at load, so drawing the insert list never locks a plugin.
    has_editor: bool,
    /// A CLAP plugin's callback state, so its main-thread work can run
    /// without the chain lock or the instance lock.
    pub(crate) clap_host: Option<Arc<HostState>>,
    /// A VST3 plugin's main-thread servicing and its latency, the
    /// counterpart of `clap_host`.
    pub(crate) vst3: Option<Arc<Vst3Service>>,
    /// Whether `pref.state` may no longer describe the live plugin: it was
    /// missing or did not restore, or a control changed since.  The next
    /// preference save captures a fresh blob.
    pub(crate) state_stale: bool,
    /// Sample rate passed to the last load attempt.  `None` until then, so a
    /// restored slot stays distinguishable from one opened at a rate.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) built_rate: Option<u32>,
    /// The insert's input, read back delayed by its latency through
    /// `dry_tap`: the dry side of a partial mix, and the whole output of a
    /// block the insert skips, so the chain delays by
    /// [`Self::latency_samples`] whatever the insert does.  Sized for
    /// [`MAX_ALIGN`] plus one chunk when a plugin loads (on the UI thread);
    /// empty (silent, never written) while nothing is loaded.
    pub(crate) dry: DelayLine,
    pub(crate) dry_tap: DelayTap,
    /// Whether `dry_tap` has adopted a delay (else the next block snaps).
    pub(crate) dry_started: bool,
    /// Whether the last chunk ran the plugin (else it was skipped): a
    /// switch between the two is smoothed by `declick` instead of stepping.
    pub(crate) was_processed: bool,
    pub(crate) declick: Declick,
    /// A pure delay standing in for a plugin, for tests.
    #[cfg(test)]
    pub(crate) test_delay: Option<TestDelay>,
    /// Test slots counted by [`tracked_slots_alive`].  The counter lives on
    /// this field so `Slot` itself does not implement [`Drop`] (a `Drop`
    /// impl would reject the struct-update syntax the loaders use).
    #[cfg(test)]
    #[allow(dead_code)]
    track: SlotTrack,
}

/// See [`Slot::delay_for_test`].
#[cfg(test)]
pub(crate) struct TestDelay {
    pub(crate) latency: u32,
    pub(crate) line: DelayLine,
}

#[cfg(test)]
#[derive(Default)]
struct SlotTrack {
    tracked: bool,
}

#[cfg(test)]
impl Drop for SlotTrack {
    fn drop(&mut self) {
        if self.tracked {
            TRACKED_SLOTS.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Slot {
    pub(crate) fn empty(pref: PluginPref) -> Self {
        Self {
            controls: Arc::new(SlotControls::new(&pref)),
            was_processed: false,
            declick: Declick::default(),
            pref,
            plugin: None,
            error: None,
            parameters: Vec::new(),
            has_editor: false,
            clap_host: None,
            vst3: None,
            state_stale: false,
            built_rate: None,
            dry: DelayLine::empty(),
            dry_tap: DelayTap::default(),
            dry_started: false,
            #[cfg(test)]
            test_delay: None,
            #[cfg(test)]
            track: SlotTrack::default(),
        }
    }

    /// The insert as it should be saved: its pref with the live mix and
    /// bypass.
    pub(crate) fn saved_pref(&self) -> PluginPref {
        PluginPref {
            mix: self.controls.mix(),
            bypass: self.controls.bypass(),
            ..self.pref.clone()
        }
    }

    /// The dry delay line a loaded insert needs (allocates; UI thread).
    pub(crate) fn dry_line() -> DelayLine {
        DelayLine::new(MAX_ALIGN as usize + MAX_BLOCK)
    }

    /// An insert that delays its input by `latency` frames and reports that
    /// latency, like a lookahead plugin with nothing to do: lets the tests
    /// check the alignment sample by sample.
    #[cfg(test)]
    pub(crate) fn delay_for_test(latency: u32) -> Self {
        let mut slot = Self::empty(PluginPref {
            name: "Delay".to_owned(),
            ..PluginPref::default()
        });
        slot.dry = Self::dry_line();
        slot.test_delay = Some(TestDelay {
            latency,
            line: DelayLine::new(MAX_ALIGN as usize),
        });
        slot
    }

    #[cfg(test)]
    pub(crate) fn tracked_for_test() -> Self {
        TRACKED_SLOTS.fetch_add(1, Ordering::SeqCst);
        let mut slot = Self::empty(PluginPref::default());
        slot.track.tracked = true;
        slot
    }

    pub(crate) fn loaded(pref: PluginPref, sample_rate: u32) -> Self {
        match load_plugin(&pref, sample_rate) {
            Ok((plugin, state_current)) => {
                let parameters = host_parameters(&plugin);
                let has_editor = host_has_editor(&plugin);
                let (clap_host, vst3) = match &plugin {
                    PluginHandle::Clap(clap) => (
                        Some(
                            clap.lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .host(),
                        ),
                        None,
                    ),
                    PluginHandle::Vst3(plugin) => {
                        (None, Some(Arc::new(Vst3Service::new(Arc::clone(plugin)))))
                    }
                };
                Self {
                    controls: Arc::new(SlotControls::new(&pref)),
                    was_processed: false,
                    declick: Declick::default(),
                    dry: Self::dry_line(),
                    dry_tap: DelayTap::default(),
                    dry_started: false,
                    #[cfg(test)]
                    test_delay: None,
                    pref,
                    plugin: Some(plugin),
                    error: None,
                    parameters,
                    has_editor,
                    clap_host,
                    vst3,
                    state_stale: !state_current,
                    built_rate: Some(sample_rate),
                    #[cfg(test)]
                    track: SlotTrack::default(),
                }
            }
            Err(error) => Self {
                error: Some(error),
                built_rate: Some(sample_rate),
                ..Self::empty(pref)
            },
        }
    }

    pub(crate) fn active(&self) -> bool {
        #[cfg(test)]
        if self.test_delay.is_some() {
            return !self.controls.bypass();
        }
        !self.controls.bypass()
            && self.error.is_none()
            && self.plugin.is_some()
            && !self.ports_changed()
    }

    /// A saved slot that has never been opened and has not failed.
    pub(crate) fn needs_open(&self) -> bool {
        #[cfg(test)]
        if self.test_delay.is_some() {
            return false;
        }
        self.plugin.is_none() && self.error.is_none()
    }

    /// Whether this slot's CLAP plugin invalidated its port buffers.  The
    /// insert is left out of the chain and the panel shows the error until it
    /// is removed and added again.
    pub(crate) fn ports_changed(&self) -> bool {
        self.clap_host
            .as_ref()
            .is_some_and(|host| host.ports_changed())
    }

    /// The latency in samples this insert adds to the chain, as the plugin
    /// last reported it.  Lock-free (atomics only), so the audio thread may
    /// call it.
    ///
    /// A bypassed insert still counts: bypass only skips the processing,
    /// while the alignment around the chain is sized for the plugin's
    /// latency, so toggling bypass never moves the alignment (and never
    /// makes the paths jump against each other).  An insert that is not
    /// loaded, failed, or lost its ports to a rescan counts 0: it stays out
    /// of the chain until it is loaded again.
    ///
    /// Whenever the chain skips the insert (bypass, mix at 0, a missed
    /// try-lock, a CLAP restart's passthrough) it plays the insert's input
    /// delayed by this many frames instead, and a partial mix delays its
    /// dry side by it, so the chain always delays by its reported total.
    /// Values are clamped to [`MAX_ALIGN`] where they are read.
    pub(crate) fn latency_samples(&self) -> u32 {
        #[cfg(test)]
        if let Some(test) = &self.test_delay {
            return test.latency.min(MAX_ALIGN);
        }
        if self.plugin.is_none() || self.error.is_some() || self.ports_changed() {
            return 0;
        }
        if let Some(host) = &self.clap_host {
            host.latency()
        } else if let Some(vst3) = &self.vst3 {
            vst3.latency()
        } else {
            0
        }
    }

    /// Tells a VST3 insert's service that the plugin was just edited, so it
    /// looks for restart requests for a while (see [`Vst3Service::nudge`]).
    pub(crate) fn nudge_vst3(&self) {
        if let Some(vst3) = &self.vst3 {
            vst3.nudge();
        }
    }

    /// The first error the slot should show: a load/processing failure, or a
    /// port change that has not yet reached `process`.
    pub(crate) fn resolved_error(&self) -> Option<String> {
        self.error
            .clone()
            .or_else(|| self.ports_changed().then(|| AUDIO_PORTS_CHANGED.to_owned()))
    }

    /// Opens a saved slot on demand (the configure window may be opened before
    /// the bridge has started) and refreshes its control list so the panel
    /// shows the values the plugin has right now.  A slot that failed stays
    /// failed.
    pub(crate) fn ensure_loaded(&mut self, sample_rate: u32) {
        if !self.load_if_needed(sample_rate) {
            if let Some(parameters) = self.plugin.as_ref().map(host_parameters) {
                self.parameters = parameters;
            }
        }
    }

    /// Opens a never-loaded slot.  Returns whether it loaded one, which also
    /// read the fresh control list.
    pub(crate) fn load_if_needed(&mut self, sample_rate: u32) -> bool {
        if self.plugin.is_none() && self.error.is_none() {
            let controls = Arc::clone(&self.controls);
            *self = Slot::loaded(self.saved_pref(), sample_rate);
            self.controls = controls;
            true
        } else {
            false
        }
    }

    pub(crate) fn view(&self) -> PluginView {
        PluginView {
            path: self.pref.path.clone(),
            format: self.pref.format,
            class_id: self.pref.class_id.clone(),
            name: self.pref.name.clone(),
            vendor: self.pref.vendor.clone(),
            category: self.pref.category.clone(),
            bypass: self.controls.bypass(),
            mix: self.controls.mix(),
            error: self.resolved_error(),
            parameters: self.parameters.clone(),
            has_editor: self.plugin.is_some() && self.has_editor,
            instance: self.plugin.as_ref().map(PluginHandle::instance_id),
            latency: self.latency_samples(),
        }
    }
}

#[cfg(test)]
static TRACKED_SLOTS: AtomicUsize = AtomicUsize::new(0);

/// Slots whose plugins must be destroyed on the UI thread.  Filled from
/// [`Drop`] when the dropping thread is not the recorded main thread, and
/// emptied by [`drain_deferred_plugins`].
static DEFERRED_PLUGIN_SLOTS: Mutex<Vec<Vec<Slot>>> = Mutex::new(Vec::new());

pub(crate) fn defer_plugin_slots(slots: Vec<Slot>) {
    if slots.is_empty() {
        return;
    }
    match DEFERRED_PLUGIN_SLOTS.lock() {
        Ok(mut queue) => queue.push(slots),
        Err(poison) => poison.into_inner().push(slots),
    }
}

/// Drops plugins that a bridge thread deferred.  Only the recorded main
/// thread destroys them; any other caller leaves the queue alone.
pub(crate) fn drain_deferred_plugins() {
    if !super::super::clap::is_main_thread() {
        return;
    }
    let pending = match DEFERRED_PLUGIN_SLOTS.lock() {
        Ok(mut queue) => std::mem::take(&mut *queue),
        Err(poison) => std::mem::take(&mut *poison.into_inner()),
    };
    drop(pending);
}

#[cfg(test)]
pub(crate) fn tracked_slots_alive() -> usize {
    TRACKED_SLOTS.load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn deferred_slot_count() -> usize {
    match DEFERRED_PLUGIN_SLOTS.lock() {
        Ok(queue) => queue.iter().map(Vec::len).sum(),
        Err(poison) => poison.into_inner().iter().map(Vec::len).sum(),
    }
}
