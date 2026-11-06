//! Insert slots and deferred plugin teardown.

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::super::clap::{HostState, AUDIO_PORTS_CHANGED};
use super::load::{host_has_editor, host_parameters, load_plugin, PluginHandle};
use super::pref::{PluginParamView, PluginPref, PluginView};

pub(crate) struct Slot {
    pub(crate) pref: PluginPref,
    pub(crate) plugin: Option<PluginHandle>,
    pub(crate) error: Option<String>,
    pub(crate) parameters: Vec<PluginParamView>,
    /// Read once at load, so drawing the insert list never locks a plugin.
    has_editor: bool,
    /// A CLAP plugin's callback state, so its main-thread work can run
    /// without the chain lock or the instance lock.
    pub(crate) clap_host: Option<Arc<HostState>>,
    /// Whether `pref.state` may no longer describe the live plugin: it was
    /// missing or did not restore, or a control changed since.  The next
    /// preference save captures a fresh blob.
    pub(crate) state_stale: bool,
    /// Sample rate passed to the last load attempt.  `None` until then, so a
    /// restored slot stays distinguishable from one opened at a rate.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) built_rate: Option<u32>,
    /// Test slots counted by [`tracked_slots_alive`].  The counter lives on
    /// this field so `Slot` itself does not implement [`Drop`] (a `Drop`
    /// impl would reject the struct-update syntax the loaders use).
    #[cfg(test)]
    #[allow(dead_code)]
    track: SlotTrack,
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
            pref,
            plugin: None,
            error: None,
            parameters: Vec::new(),
            has_editor: false,
            clap_host: None,
            state_stale: false,
            built_rate: None,
            #[cfg(test)]
            track: SlotTrack::default(),
        }
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
                let clap_host = match &plugin {
                    PluginHandle::Clap(clap) => Some(
                        clap.lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .host(),
                    ),
                    PluginHandle::Vst3(_) => None,
                };
                Self {
                    pref,
                    plugin: Some(plugin),
                    error: None,
                    parameters,
                    has_editor,
                    clap_host,
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
        !self.pref.bypass && self.error.is_none() && self.plugin.is_some() && !self.ports_changed()
    }

    /// A saved slot that has never been opened and has not failed.
    pub(crate) fn needs_open(&self) -> bool {
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
            *self = Slot::loaded(self.pref.clone(), sample_rate);
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
            bypass: self.pref.bypass,
            mix: self.pref.mix,
            error: self.resolved_error(),
            parameters: self.parameters.clone(),
            has_editor: self.plugin.is_some() && self.has_editor,
            instance: self.plugin.as_ref().map(PluginHandle::instance_id),
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
