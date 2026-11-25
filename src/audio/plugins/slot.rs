//! Effect slots, the links the UI reaches them by, and deferred plugin
//! teardown.

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::super::delay::{Declick, DelayLine, DelayTap};
use super::handle::{LoadedPlugin, PluginHandle};
use super::load::{load_plugin, MAX_BLOCK};
use super::pref::{PluginPref, PluginView};
use super::MAX_ALIGN;
use crate::audio::LockStats;

/// An effect's mix and bypass as atomics: the UI sets them through the
/// effect's link and the audio thread reads them every chunk, without a
/// chain edit.  The slot's `pref.mix` / `pref.bypass` are only the values it
/// was made with; [`SlotLink::saved_pref`] and the views read these.
///
/// It also counts how often the audio thread tried to run the effect's
/// plugin, and how many of those chunks found it paused (a restart, a
/// failed start).  Measurement only: the audio thread writes these
/// relaxedly and Diagnostics reads them.
pub(crate) struct SlotControls {
    mix: AtomicU32,
    bypass: AtomicBool,
    attempts: AtomicU64,
    paused: AtomicU64,
    /// Rebuilds the plugin asked for in the current window, and when that
    /// window began (milliseconds on [`now_ms`], 0 for none): bounds a
    /// plugin that keeps asking.  UI thread.
    reloads: AtomicU32,
    reload_window_ms: AtomicU64,
}

/// Milliseconds since the first call, for [`SlotControls::reload_window_ms`].
fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        // Never 0 (no window), and far enough from it that a test can move
        // a window back by more than its length.
        + 1_000_000
}

/// How many rebuilds a plugin may ask for within [`RELOAD_WINDOW_MS`]:
/// the next request in that window fails the effect instead ("keeps
/// asking"), so a plugin that asks on every load, or after a fixed delay,
/// cannot reload forever.  Two in quick succession (a user changing the
/// buses twice) are fine.
pub(crate) const RELOAD_LIMIT: u32 = 2;
/// The window [`RELOAD_LIMIT`] counts in, from the first rebuild of a run;
/// a quiet window starts the count again.
pub(crate) const RELOAD_WINDOW_MS: u64 = 10_000;

impl SlotControls {
    pub(crate) fn new(pref: &PluginPref) -> Self {
        Self {
            mix: AtomicU32::new(pref.mix.clamp(0.0, 1.0).to_bits()),
            bypass: AtomicBool::new(pref.bypass),
            attempts: AtomicU64::new(0),
            paused: AtomicU64::new(0),
            reloads: AtomicU32::new(0),
            reload_window_ms: AtomicU64::new(0),
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

    /// Counts one attempt to run the effect's plugin (audio thread, once
    /// per chunk the effect runs).
    pub(crate) fn note_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether the window of rebuilds on request has run out.
    fn reload_window_over(&self) -> bool {
        match self.reload_window_ms.load(Ordering::Relaxed) {
            0 => true,
            start => now_ms().saturating_sub(start) >= RELOAD_WINDOW_MS,
        }
    }

    /// Notes that the slot was just rebuilt on its plugin's request.
    pub(crate) fn note_rebuilt(&self) {
        if self.reload_window_over() {
            self.reload_window_ms.store(now_ms(), Ordering::Relaxed);
            self.reloads.store(1, Ordering::Relaxed);
        } else {
            self.reloads.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether another rebuild on request is allowed (see
    /// [`RELOAD_LIMIT`]).
    pub(crate) fn may_reload(&self) -> bool {
        self.reload_window_over() || self.reloads.load(Ordering::Relaxed) < RELOAD_LIMIT
    }

    /// Starts the count again: the user changed the slot (a rate change
    /// rebuilt it).
    pub(crate) fn reset_reloads(&self) {
        self.reloads.store(0, Ordering::Relaxed);
        self.reload_window_ms.store(0, Ordering::Relaxed);
    }

    /// Moves the current window `ms` into the past, for tests that would
    /// otherwise wait it out.
    #[cfg(test)]
    pub(crate) fn age_reload_window_for_test(&self, ms: u64) {
        let start = self.reload_window_ms.load(Ordering::Relaxed);
        if start != 0 {
            self.reload_window_ms
                .store(start - ms.min(start - 1), Ordering::Relaxed);
        }
    }

    /// Counts an attempt that found the plugin paused (audio thread).
    pub(crate) fn note_paused(&self) {
        self.paused.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn lock_stats(&self) -> LockStats {
        LockStats {
            attempts: self.attempts.load(Ordering::Relaxed),
            skips: self.paused.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn reset_lock_stats(&self) {
        self.attempts.store(0, Ordering::Relaxed);
        self.paused.store(0, Ordering::Relaxed);
    }
}

pub(crate) struct Slot {
    /// The effect's identity, and what an unloaded slot restores with.
    /// Once a plugin is loaded its saved values and state blob move to its
    /// handle (see [`SlotLink::saved_pref`]) and these are left empty.
    /// Shared with the published link, so a publish copies nothing.
    pub(crate) pref: Arc<PluginPref>,
    /// Mix and bypass as the UI sets them (see [`SlotControls`]).  Kept
    /// when the slot is reopened or rebuilt for a new rate.
    pub(crate) controls: Arc<SlotControls>,
    /// The loaded plugin.  A processing failure is recorded in its
    /// [`super::link::PluginShared`]; the effect then stays out of the
    /// chain like one with an `error`.
    pub(crate) plugin: Option<LoadedPlugin>,
    /// A load failure.
    pub(crate) error: Option<String>,
    /// Sample rate passed to the last load attempt.  `None` until then, so a
    /// restored slot stays distinguishable from one opened at a rate.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) built_rate: Option<u32>,
    /// The effect's input, read back delayed by its latency through
    /// `dry_tap`: the dry side of a partial mix, and the whole output of a
    /// block the effect skips, so the chain delays by
    /// [`Self::latency_samples`] whatever the effect does.  Sized for
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
    /// Shared with the slot's links, so a test can change it while an
    /// audio side holds the slot, as a plugin changes its own latency.
    pub(crate) latency: Arc<AtomicU32>,
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
            pref: Arc::new(pref),
            plugin: None,
            error: None,
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

    /// The dry delay line a loaded effect needs (allocates; UI thread).
    pub(crate) fn dry_line() -> DelayLine {
        DelayLine::new(MAX_ALIGN as usize + MAX_BLOCK)
    }

    /// An effect that delays its input by `latency` frames and reports that
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
            latency: Arc::new(AtomicU32::new(latency)),
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
            Ok((plugin, _)) => Self {
                plugin: Some(plugin),
                dry: Self::dry_line(),
                built_rate: Some(sample_rate),
                // The handle owns the saved values and state from here on.
                ..Self::empty(PluginPref {
                    params: Vec::new(),
                    state: None,
                    ..pref
                })
            },
            Err(error) => Self {
                error: Some(error),
                built_rate: Some(sample_rate),
                ..Self::empty(pref)
            },
        }
    }

    /// A slot holding `plugin`, the way a real load fills one in.
    #[cfg(test)]
    pub(crate) fn with_plugin_for_test(pref: PluginPref, plugin: LoadedPlugin, rate: u32) -> Self {
        Self {
            plugin: Some(plugin),
            dry: Self::dry_line(),
            built_rate: Some(rate),
            ..Self::empty(pref)
        }
    }

    pub(crate) fn active(&self) -> bool {
        #[cfg(test)]
        if self.test_delay.is_some() {
            return !self.controls.bypass();
        }
        !self.controls.bypass() && !self.failed() && self.plugin.is_some() && !self.reload_wanted()
    }

    /// A saved slot that has never been opened and has not failed.
    pub(crate) fn needs_open(&self) -> bool {
        #[cfg(test)]
        if self.test_delay.is_some() {
            return false;
        }
        self.plugin.is_none() && self.error.is_none()
    }

    /// Whether this slot's plugin asked to be rebuilt (see
    /// `PluginShared::want_reload`) and has not failed.  It stays out of
    /// the chain, still counting its latency, until the path's service
    /// replaces it.  Atomics only.
    pub(crate) fn reload_wanted(&self) -> bool {
        !self.failed()
            && self
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.shared().reload_wanted())
    }

    /// The latency in samples this effect adds to the chain, as the plugin
    /// last reported it.  Lock-free (atomics only), so the audio thread may
    /// call it.
    ///
    /// A bypassed effect still counts: bypass only skips the processing,
    /// while the alignment around the chain is sized for the plugin's
    /// latency, so toggling bypass never moves the alignment (and never
    /// makes the paths jump against each other).  So does one waiting for
    /// a reload.  An effect that is not loaded or failed counts 0: it stays
    /// out of the chain until it is loaded again.
    ///
    /// Whenever the chain skips the effect (bypass, mix at 0, a restart's
    /// pause) it plays the effect's input
    /// delayed by this many frames instead, and a partial mix delays its
    /// dry side by it, so the chain always delays by its reported total.
    /// Values are clamped to [`MAX_ALIGN`] where they are read.
    pub(crate) fn latency_samples(&self) -> u32 {
        #[cfg(test)]
        if let Some(test) = &self.test_delay {
            return test.latency.load(Ordering::Relaxed).min(MAX_ALIGN);
        }
        match &self.plugin {
            Some(plugin) if !self.failed() => plugin.latency(),
            _ => 0,
        }
    }

    /// Whether the slot failed to load or to process.  Lock and allocation
    /// free, so the audio thread may call it.
    pub(crate) fn failed(&self) -> bool {
        self.error.is_some()
            || self
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.shared().failure().is_some())
    }

    /// The first error the slot should show (see [`SlotLink::resolved_error`]).
    /// UI thread only.
    #[cfg(test)]
    pub(crate) fn resolved_error(&self) -> Option<String> {
        self.link().resolved_error()
    }

    #[cfg(test)]
    pub(crate) fn view(&self) -> PluginView {
        self.link().view()
    }

    /// What the UI needs to reach this effect while the runtime holds it (UI
    /// thread; allocates).
    pub(crate) fn link(&self) -> SlotLink {
        SlotLink {
            base: Arc::clone(&self.pref),
            controls: Arc::clone(&self.controls),
            plugin: self.plugin.as_ref().map(|plugin| plugin.handle().clone()),
            error: self.error.clone(),
            #[cfg(test)]
            test_latency: self
                .test_delay
                .as_ref()
                .map(|test| Arc::clone(&test.latency)),
            #[cfg(test)]
            built_rate: self.built_rate,
        }
    }
}

/// One effect as the UI reaches it while the runtime holds it, published with
/// the views (see `PathControls::slot_links`).  Everything it reads is the
/// handle's UI-side state or atomics, so the views, the saved prefs and the
/// latency total can be rebuilt from the links alone.
#[derive(Clone)]
pub(crate) struct SlotLink {
    /// The slot's pref (see [`Slot::pref`]): identity, plus the saved
    /// values and state of a slot that is not loaded.
    pub(crate) base: Arc<PluginPref>,
    pub(crate) controls: Arc<SlotControls>,
    pub(crate) plugin: Option<PluginHandle>,
    /// A load failure.
    pub(crate) error: Option<String>,
    /// A test delay's latency (see [`Slot::delay_for_test`]).
    #[cfg(test)]
    test_latency: Option<Arc<AtomicU32>>,
    /// As [`Slot::built_rate`].
    #[cfg(test)]
    pub(crate) built_rate: Option<u32>,
}

impl SlotLink {
    /// Moves a test delay's latency (see [`Slot::delay_for_test`]), as a
    /// plugin moves its own.
    #[cfg(test)]
    pub(crate) fn set_test_latency(&self, latency: u32) {
        if let Some(cell) = &self.test_latency {
            cell.store(latency, Ordering::Relaxed);
        }
    }

    /// As [`Slot::needs_open`].
    pub(crate) fn needs_open(&self) -> bool {
        #[cfg(test)]
        if self.test_latency.is_some() {
            return false;
        }
        self.plugin.is_none() && self.error.is_none()
    }

    /// What a rebuilt instance restores with: the pref with a loaded
    /// plugin's saved values and state blob, but the mix and bypass it was
    /// made with (the rebuild keeps the live [`SlotControls`]).
    pub(crate) fn restore_pref(&self) -> PluginPref {
        let mut pref = (*self.base).clone();
        if let Some(plugin) = &self.plugin {
            (pref.params, pref.state) = plugin.saved();
        }
        pref
    }

    /// As [`Slot::reload_wanted`].
    pub(crate) fn reload_wanted(&self) -> bool {
        !self.failed()
            && self
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.shared().reload_wanted())
    }

    fn failed(&self) -> bool {
        self.error.is_some()
            || self
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.failure().is_some())
    }

    /// Whether the effect is paused for a restart or a reactivation, and
    /// not failed (see [`PluginView::paused`]).  Atomics only.
    pub(crate) fn paused(&self) -> bool {
        !self.failed()
            && self
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.shared().pause.in_pause())
    }

    /// As [`Slot::latency_samples`].
    pub(crate) fn latency(&self) -> u32 {
        #[cfg(test)]
        if let Some(latency) = &self.test_latency {
            return latency.load(Ordering::Relaxed).min(MAX_ALIGN);
        }
        match &self.plugin {
            Some(plugin) if !self.failed() => plugin.latency(),
            _ => 0,
        }
    }

    /// As [`Slot::active`].
    pub(crate) fn active(&self) -> bool {
        #[cfg(test)]
        if self.test_latency.is_some() {
            return !self.controls.bypass();
        }
        !self.controls.bypass() && !self.failed() && self.plugin.is_some() && !self.reload_wanted()
    }

    /// The first error the effect should show: a load or processing
    /// failure.  Allocates: UI thread only, which is where a processing
    /// failure gets its text.
    pub(crate) fn resolved_error(&self) -> Option<String> {
        self.error.clone().or_else(|| {
            self.plugin
                .as_ref()
                .and_then(PluginHandle::failure)
                .map(|failure| format!("Processing failed: {}", failure.message()))
        })
    }

    pub(crate) fn view(&self) -> PluginView {
        let pref = &*self.base;
        PluginView {
            path: pref.path.clone(),
            format: pref.format,
            class_id: pref.class_id.clone(),
            name: pref.name.clone(),
            vendor: pref.vendor.clone(),
            category: pref.category.clone(),
            bypass: self.controls.bypass(),
            mix: self.controls.mix(),
            error: self.resolved_error(),
            // The handle hands back the list it last published while nothing
            // changed there, so a republish of an unchanged effect copies no
            // controls.
            parameters: self
                .plugin
                .as_ref()
                .map(PluginHandle::parameters)
                .unwrap_or_default(),
            has_editor: self.plugin.as_ref().is_some_and(PluginHandle::has_editor),
            instance: self.plugin.as_ref().map(PluginHandle::instance_id),
            latency: self.latency(),
            paused: self.paused(),
        }
    }

    /// As [`Slot::saved_pref`].
    pub(crate) fn saved_pref(&self) -> PluginPref {
        let mut pref = PluginPref {
            mix: self.controls.mix(),
            bypass: self.controls.bypass(),
            ..(*self.base).clone()
        };
        if let Some(plugin) = &self.plugin {
            (pref.params, pref.state) = plugin.saved();
        }
        pref
    }
}

#[cfg(test)]
static TRACKED_SLOTS: AtomicUsize = AtomicUsize::new(0);

/// Slots whose plugins must be destroyed on the UI thread.  Filled from
/// [`Drop`] when the dropping thread is not the recorded main thread, and
/// emptied by [`drain_deferred_plugins`].
static DEFERRED_PLUGIN_SLOTS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

pub(crate) fn defer_plugin_slots(slots: Vec<Slot>) {
    if slots.is_empty() {
        return;
    }
    match DEFERRED_PLUGIN_SLOTS.lock() {
        Ok(mut queue) => queue.extend(slots),
        Err(poison) => poison.into_inner().extend(slots),
    }
}

/// Drops plugins that a bridge thread deferred.  Only the recorded main
/// thread destroys them; any other caller leaves the queue alone.
pub(crate) fn drain_deferred_plugins() {
    if !super::is_main_thread() {
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
        Ok(queue) => queue.len(),
        Err(poison) => poison.into_inner().len(),
    }
}
