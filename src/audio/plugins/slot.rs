//! A slot the audio thread may still be inside is leaked, not destroyed.
//! Teardown off the recorded main thread goes through [`SlotPark`].

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

/// Mix and bypass as atomics: the UI sets them and the audio thread reads
/// them every chunk, with no chain edit.  `pref.mix` / `pref.bypass` are
/// only what the slot was made with; [`SlotLink::saved_pref`] reads these.
///
/// Attempt and paused counts are measurement only.  The audio thread
/// writes them relaxed; Diagnostics reads them.
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

    /// Audio thread, once per chunk the effect runs.
    pub(crate) fn note_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    fn reload_window_over(&self) -> bool {
        match self.reload_window_ms.load(Ordering::Relaxed) {
            0 => true,
            start => now_ms().saturating_sub(start) >= RELOAD_WINDOW_MS,
        }
    }

    pub(crate) fn note_rebuilt(&self) {
        if self.reload_window_over() {
            self.reload_window_ms.store(now_ms(), Ordering::Relaxed);
            self.reloads.store(1, Ordering::Relaxed);
        } else {
            self.reloads.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn may_reload(&self) -> bool {
        self.reload_window_over() || self.reloads.load(Ordering::Relaxed) < RELOAD_LIMIT
    }

    /// The user changed the slot (a rate change rebuilt it), so the
    /// count starts again.
    pub(crate) fn reset_reloads(&self) {
        self.reloads.store(0, Ordering::Relaxed);
        self.reload_window_ms.store(0, Ordering::Relaxed);
    }

    /// Moves the window into the past so a test does not wait it out.
    #[cfg(test)]
    pub(crate) fn age_reload_window_for_test(&self, ms: u64) {
        let start = self.reload_window_ms.load(Ordering::Relaxed);
        if start != 0 {
            self.reload_window_ms
                .store(start - ms.min(start - 1), Ordering::Relaxed);
        }
    }

    /// Audio thread: the chunk found the plugin paused.
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
    /// Kept when the slot is reopened or rebuilt for a new rate.
    pub(crate) controls: Arc<SlotControls>,
    /// A processing failure lives in its [`super::link::PluginShared`].
    /// The effect then stays out of the chain, as with `error`.
    pub(crate) plugin: Option<LoadedPlugin>,
    pub(crate) error: Option<String>,
    /// Sample rate passed to the last load attempt.  `None` until then, so a
    /// restored slot stays distinguishable from one opened at a rate.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) built_rate: Option<u32>,
    /// Dry side of a partial mix, and the output of a skipped block.
    /// Sized for [`MAX_ALIGN`] plus one chunk on load (UI thread); empty,
    /// and never written, while nothing is loaded.
    pub(crate) dry: DelayLine,
    pub(crate) dry_tap: DelayTap,
    /// Else the next block snaps `dry_tap` instead of fading to the delay.
    pub(crate) dry_started: bool,
    /// The last chunk ran the plugin.  A switch is declicked.
    pub(crate) was_processed: bool,
    pub(crate) declick: Declick,
    #[cfg(test)]
    pub(crate) test_delay: Option<TestDelay>,
    /// Whether this slot's plugin is stranded, for the tests that prove the
    /// leak: a real slot reads its plugin's pause handshake.
    #[cfg(test)]
    test_stranded: bool,
    /// Test slots counted by [`tracked_slots_alive`].  The counter lives on
    /// this field so `Slot` itself does not implement [`Drop`] (a `Drop`
    /// impl would reject the struct-update syntax the loaders use).
    #[cfg(test)]
    #[allow(dead_code)]
    track: SlotTrack,
}

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
            test_stranded: false,
            #[cfg(test)]
            track: SlotTrack::default(),
        }
    }

    /// Allocates.  UI thread.
    pub(crate) fn dry_line() -> DelayLine {
        DelayLine::new(MAX_ALIGN as usize + MAX_BLOCK)
    }

    /// Reports `latency`, so alignment tests can check it sample by sample.
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

    /// Stranded, so the leak tests can prove it is not destroyed.
    #[cfg(test)]
    pub(crate) fn stranded_for_test() -> Self {
        let mut slot = Self::empty(PluginPref::default());
        slot.test_stranded = true;
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

    /// Filled the way a real load fills one.
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

    pub(crate) fn needs_open(&self) -> bool {
        #[cfg(test)]
        if self.test_delay.is_some() {
            return false;
        }
        self.plugin.is_none() && self.error.is_none()
    }

    /// Asked to be rebuilt, and has not failed.  Stays out of the chain,
    /// still counting its latency, until service replaces it.  Atomics only.
    pub(crate) fn reload_wanted(&self) -> bool {
        !self.failed()
            && self
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.shared().reload_wanted())
    }

    /// Atomics only, so the audio thread may call it.
    ///
    /// A bypassed effect still counts, and so does one waiting for a
    /// reload: toggling bypass must not move the alignment.  An unloaded
    /// or failed effect counts 0.  A skip and a partial mix delay by this,
    /// so the chain always delays by its reported total.  Clamped to
    /// [`MAX_ALIGN`] where the value is read.
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

    /// Lock and allocation free, so the audio thread may call it.
    pub(crate) fn failed(&self) -> bool {
        self.error.is_some()
            || self
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.shared().failure().is_some())
    }

    /// A block never came out of `process`.  Lock and allocation free.
    /// A stranded slot is never destroyed.
    pub(crate) fn stranded(&self) -> bool {
        #[cfg(test)]
        if self.test_stranded {
            return true;
        }
        self.plugin
            .as_ref()
            .is_some_and(|plugin| plugin.shared().pause.stranded())
    }

    /// Leaks the slot when its plugin is stranded.  It is still inside
    /// `process`, so nothing may stop, deactivate, or destroy it.
    pub(crate) fn destroy(self) {
        if self.stranded() {
            std::mem::forget(self);
        } else {
            drop(self);
        }
    }

    /// UI thread only.
    #[cfg(test)]
    pub(crate) fn resolved_error(&self) -> Option<String> {
        self.link().resolved_error()
    }

    #[cfg(test)]
    pub(crate) fn view(&self) -> PluginView {
        self.link().view()
    }

    /// UI thread; allocates.  What the UI needs while the runtime holds it.
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

/// Everything it reads is the handle's UI-side state or atomics, so the
/// views, the saved prefs and the latency can be rebuilt from the links
/// alone, with the runtime held.
#[derive(Clone)]
pub(crate) struct SlotLink {
    /// The slot's pref (see [`Slot::pref`]): identity, plus the saved
    /// values and state of a slot that is not loaded.
    pub(crate) base: Arc<PluginPref>,
    pub(crate) controls: Arc<SlotControls>,
    pub(crate) plugin: Option<PluginHandle>,
    pub(crate) error: Option<String>,
    #[cfg(test)]
    test_latency: Option<Arc<AtomicU32>>,
    #[cfg(test)]
    pub(crate) built_rate: Option<u32>,
}

impl SlotLink {
    #[cfg(test)]
    pub(crate) fn with_handle_for_test(handle: PluginHandle) -> Self {
        let pref = PluginPref::default();
        Self {
            base: Arc::new(pref.clone()),
            controls: Arc::new(SlotControls::new(&pref)),
            plugin: Some(handle),
            error: None,
            test_latency: None,
            built_rate: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_latency(&self, latency: u32) {
        if let Some(cell) = &self.test_latency {
            cell.store(latency, Ordering::Relaxed);
        }
    }

    pub(crate) fn needs_open(&self) -> bool {
        #[cfg(test)]
        if self.test_latency.is_some() {
            return false;
        }
        self.plugin.is_none() && self.error.is_none()
    }

    /// Saved values and state from a loaded plugin.  Mix and bypass stay
    /// the ones the slot was made with; the rebuild keeps the live
    /// [`SlotControls`].
    pub(crate) fn restore_pref(&self) -> PluginPref {
        let mut pref = (*self.base).clone();
        if let Some(plugin) = &self.plugin {
            (pref.params, pref.state) = plugin.saved();
        }
        pref
    }

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

    /// Atomics only.  A failed effect is not paused.
    pub(crate) fn paused(&self) -> bool {
        !self.failed()
            && self
                .plugin
                .as_ref()
                .is_some_and(|plugin| plugin.shared().pause.in_pause())
    }

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

    pub(crate) fn active(&self) -> bool {
        #[cfg(test)]
        if self.test_latency.is_some() {
            return !self.controls.bypass();
        }
        !self.controls.bypass() && !self.failed() && self.plugin.is_some() && !self.reload_wanted()
    }

    /// Allocates.  UI thread only: that is where a processing failure
    /// gets its text.
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

/// Slots a bridge thread must not destroy.  One park per engine, shared
/// by every run, so a bridge that defers after the engine swapped its run
/// still has an owner.  The recorded main thread drains it.
pub(crate) struct SlotPark {
    slots: Mutex<Vec<Slot>>,
}

impl SlotPark {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::new(Vec::new()),
        })
    }

    /// Only a `Drop` comes here (`chain::release_slot`).  The owners are
    /// the UI thread and the bridge, not an audio callback: that lock is
    /// asserted before it is taken.
    pub(crate) fn defer(&self, slots: Vec<Slot>) {
        if slots.is_empty() {
            return;
        }
        match self.slots.lock() {
            Ok(mut queue) => queue.extend(slots),
            Err(poison) => poison.into_inner().extend(slots),
        }
    }

    /// Only the recorded main thread destroys them; any other caller
    /// leaves the queue.  A stranded slot is leaked: it was released
    /// while a block was still inside `process`.
    pub(crate) fn drain(&self) {
        if !super::is_main_thread() {
            return;
        }
        let pending = match self.slots.lock() {
            Ok(mut queue) => std::mem::take(&mut *queue),
            Err(poison) => std::mem::take(&mut *poison.into_inner()),
        };
        for slot in pending {
            slot.destroy();
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        match self.slots.lock() {
            Ok(queue) => queue.len(),
            Err(poison) => poison.into_inner().len(),
        }
    }
}

impl Drop for SlotPark {
    /// A thread that may destroy plugins does, and a stranded slot is
    /// leaked.  Any other thread forgets the slots: there is no later
    /// owner to defer to, and this thread may not destroy a plugin.
    fn drop(&mut self) {
        let pending = match self.slots.get_mut() {
            Ok(queue) => std::mem::take(queue),
            Err(poison) => std::mem::take(poison.into_inner()),
        };
        if super::may_destroy_plugins_here() {
            for slot in pending {
                slot.destroy();
            }
        } else {
            for slot in pending {
                std::mem::forget(slot);
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn tracked_slots_alive() -> usize {
    TRACKED_SLOTS.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests;
