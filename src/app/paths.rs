//! Signal paths as the app holds them, and the actions the UI calls on them.
//!
//! The app keeps one [`SignalPath`] per live path slot in
//! [`RigWerkApp::paths`] (indexed by the engine's slot, which never moves
//! while the path lives) and pushes each one's [`PathMix`] to the engine
//! every frame, like the other mix settings.  The plugin chain of each
//! path lives in the engine.  How a path reaches each output device lives
//! with the device: `OutputDeviceSettings::path_map[slot]`.
//!
//! The Mixer and Plugins pages edit [`RigWerkApp::selected_path`]
//! ([`RigWerkApp::path`] / [`RigWerkApp::path_mut`]).
//!
//! Slots are engine-internal.  The UI lists paths in
//! [`RigWerkApp::path_order`] (the display order: paths are appended as
//! they are added), and that is also the order they are saved in: each
//! device's `path_map` is compacted to it.  Loading reuses the live slots
//! first, in order, then free ones.
//!
//! A removed path keeps its device entries while it fades out, so it fades
//! where it played; the next path in that slot overwrites them.
//!
//! A return ([`PathFeed::Sends`]) runs on the other paths' sends.  Sends
//! are indexed by slot like the device maps, and saved in the display
//! order the same way.  Only a path fed by the input sends, and only to a
//! return: a path that stops being a return, or goes away, takes every
//! send to it along.  A return is solo-safe and has no solo of its own.

use crate::app::cab::CabInstall;
use crate::app::chain::ChainSelection;
use crate::app::nam::NamInstall;
use crate::app::settings::{default_path_name, ExtraOutputPref, PathPref};
use crate::app::LogTone;
use crate::app::RigWerkApp;
use crate::audio::nam::NamLoad;
use crate::audio::{
    low_bits, path_level_gain, sanitize_path_level, sanitize_send, EngineState, EqSettings,
    GateSettings, MapPreset, NamBlockControls, NamBlockSettings, NamPosition, NamStack,
    OutputDeviceSettings, PathFeed, PathMapSettings, PathMix, PathState, UnusedChannels,
    MAX_NAM_BLOCKS, MAX_OUTPUT_CHANNELS, MAX_PATHS, SEND_OFF_DB,
};

/// One NAM block of a path, as the UI holds it: the controls the engine
/// gets, the capture it runs, and what loading that capture produced.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NamBlock {
    /// The controls the engine reads.  `id` is this block's handle.
    pub(crate) controls: NamBlockControls,
    /// The `.nam` capture.  Empty means the block has no model and passes
    /// its input through.
    pub(crate) model: String,
    /// Display name of the loaded capture, or a stem when the load failed.
    pub(crate) name: String,
    /// Why the last load failed.  The path string is kept.
    pub(crate) error: Option<String>,
    /// Width thresholds of a slimmable capture, empty otherwise.  The width
    /// dial shows when there are some.
    pub(crate) slim_steps: Vec<f32>,
    /// The file parsed and its sample rate is not the path's rate.
    pub(crate) mismatch: bool,
    /// The file's sample rate, when one was declared.  0 if unknown.
    pub(crate) file_rate: u32,
    /// The last install attempt.  A matching stamp is not rebuilt.
    pub(crate) install: NamInstall,
    /// Gear type of the loaded capture, trimmed.  UI only: the audio
    /// thread never sees it.  Cleared when the model is cleared or fails.
    pub(crate) gear: Option<String>,
    /// The file's architecture, for the badge and the estimate.
    pub(crate) arch: String,
    /// Every submodel of the loaded capture is a WaveNet whose dilations
    /// scale with a doubled rate.  The UI counts these per group to decide
    /// whether a block runs at 1x or 2x.
    pub(crate) oversample: bool,
    /// What the capture measured as at the running rate and block size.
    /// `None` until it is measured.
    pub(crate) load: Option<NamLoad>,
}

impl NamBlock {
    pub(crate) fn new(id: u32) -> Self {
        Self {
            controls: NamBlockControls {
                id,
                ..NamBlockControls::default()
            },
            model: String::new(),
            name: String::new(),
            error: None,
            slim_steps: Vec::new(),
            mismatch: false,
            file_rate: 0,
            install: NamInstall::default(),
            gear: None,
            arch: String::new(),
            oversample: false,
            load: None,
        }
    }

    pub(crate) fn from_settings(settings: &NamBlockSettings, id: u32) -> Self {
        Self {
            controls: NamBlockControls {
                id,
                ..settings.controls.sanitized()
            },
            model: settings.model.trim().to_owned(),
            ..Self::new(id)
        }
    }

    /// Forgets what the last load produced, for a block whose capture is
    /// gone or whose file failed.
    pub(crate) fn cleared(&mut self) {
        self.name.clear();
        self.error = None;
        self.slim_steps.clear();
        self.mismatch = false;
        self.file_rate = 0;
        self.gear = None;
        self.arch.clear();
        self.oversample = false;
        self.load = None;
        self.controls.level_in_dbu = None;
        self.controls.level_out_dbu = None;
        self.controls.loudness_db = None;
    }

    pub(crate) fn settings(&self) -> NamBlockSettings {
        NamBlockSettings {
            model: self.model.clone(),
            controls: self.controls.sanitized(),
        }
    }

    /// Whether the block's capture is installed, whether or not the block is
    /// bypassed.  A bypassed block still counts in its group.
    pub(crate) fn installed(&self) -> bool {
        !self.model.trim().is_empty() && !self.mismatch && self.error.is_none()
    }

    /// Whether the block runs: on, and with a capture that loaded (or is
    /// still loading).  A failed load or a rate mismatch runs nothing.
    pub(crate) fn active(&self) -> bool {
        self.controls.enabled && self.installed()
    }

    /// Whether the installed capture may run at twice the output rate: it
    /// has a model, and every submodel of it can scale.
    pub(crate) fn can_oversample(&self) -> bool {
        self.installed() && self.oversample
    }

    /// The share of one block the block is expected to take, measured when
    /// there is a measurement, estimated from its architecture otherwise.
    pub(crate) fn load_share(&self, rate: u32) -> f32 {
        self.load.map(NamLoad::share).unwrap_or_else(|| {
            crate::audio::nam::estimate_load(&self.arch, f64::from(rate)).share()
        })
    }
}

/// One live path's UI data (name, colour index) and the settings pushed to
/// the engine.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SignalPath {
    pub(crate) name: String,
    /// Index into the path palette.  The colour is chosen when a row is drawn.
    pub(crate) colour: u8,
    pub(crate) mix: PathMix,
    /// The NAM blocks in signal order: the `Pre` group first, then the one
    /// after the effects.  The engine gets their controls as one
    /// [`NamStack`].
    pub(crate) nam: Vec<NamBlock>,
    /// The id the next added block gets.  Never reused within a path.
    pub(crate) next_nam_id: u32,
    /// The impulse response.  Empty means the cabinet has no file.
    pub(crate) cab_file: String,
    /// Display name of the loaded impulse, or a stem when the load failed.
    pub(crate) cab_name: String,
    /// Why the last load failed.  The path string is kept.
    pub(crate) cab_error: Option<String>,
    /// The last install attempt.  A matching stamp is not rebuilt.
    pub(crate) cab_install: CabInstall,
    /// Channels already decoded and resampled to this rate, so a cut or a
    /// polarity change does not read the file again.
    pub(crate) cab_raw: Option<(u32, Vec<f32>, Vec<f32>)>,
}

impl SignalPath {
    /// The path `pref` describes, its blocks numbered from `first_id`.  A
    /// slot the engine may still hold block state for passes the first id
    /// it has not used, so a new block never takes an old one's model.
    /// The blocks are put in stack order (the `Pre` group, then `Post`):
    /// a row or a sheet names a block by its index in that order.
    pub(crate) fn from_pref(pref: &PathPref, first_id: u32) -> Self {
        let nam: Vec<NamBlock> = pref
            .nam_blocks
            .iter()
            .take(MAX_NAM_BLOCKS)
            .enumerate()
            .map(|(at, settings)| {
                NamBlock::from_settings(settings, first_id.wrapping_add(at as u32))
            })
            .collect();
        let mut path = Self {
            name: pref.name.clone(),
            colour: pref.colour % MAX_PATHS as u8,
            mix: pref.mix(),
            next_nam_id: first_id.wrapping_add(nam.len() as u32),
            nam,
            cab_file: pref.cab.file.clone(),
            cab_name: String::new(),
            cab_error: None,
            cab_install: CabInstall::default(),
            cab_raw: None,
        };
        let stack = path.nam_stack();
        path.order_nam_like(&stack);
        path
    }

    /// The path as saved, its sends in the saved order `order`.
    fn to_pref(&self, plugins: Vec<crate::audio::PluginPref>, order: &[usize]) -> PathPref {
        PathPref {
            sends: PathPref::sends_in_order(&self.mix.sends, order),
            ..PathPref::from_mix(
                self.name.clone(),
                self.colour,
                &self.mix,
                self.nam_settings(),
                &self.cab_file,
                plugins,
            )
        }
    }

    /// The engine's view of the blocks: their controls, in signal order.
    pub(crate) fn nam_stack(&self) -> NamStack {
        let mut stack = NamStack::default();
        for block in &self.nam {
            if !stack.push(block.controls.sanitized()) {
                break;
            }
        }
        stack
    }

    /// Reorders the blocks to `stack`'s order, dropping the ones it left
    /// out and keeping each block's runtime state with its id.
    pub(crate) fn order_nam_like(&mut self, stack: &NamStack) {
        let mut blocks = std::mem::take(&mut self.nam);
        let mut ordered = Vec::with_capacity(stack.count());
        for controls in stack.blocks() {
            if let Some(at) = blocks
                .iter()
                .position(|block| block.controls.id == controls.id)
            {
                let mut block = blocks.remove(at);
                block.controls = controls.sanitized();
                ordered.push(block);
            }
        }
        self.nam = ordered;
    }

    /// The blocks' ids, in signal order.
    pub(crate) fn nam_ids(&self) -> Vec<u32> {
        self.nam.iter().map(|block| block.controls.id).collect()
    }

    /// The blocks as saved: their captures and controls, in signal order.
    pub(crate) fn nam_settings(&self) -> Vec<NamBlockSettings> {
        self.nam.iter().map(NamBlock::settings).collect()
    }

    /// Whether any block runs.
    pub(crate) fn nam_active(&self) -> bool {
        self.nam.iter().any(NamBlock::active)
    }

    /// The dilation scale each block should run at, in order: 2 for every
    /// installed capture that can be oversampled once its series holds two
    /// or more of them, 1 otherwise.
    ///
    /// The `Pre` and `Post` groups are separate series.  With no effects
    /// between them (`effects_empty`) they are one series, so a pedal before
    /// the effects and an amp after them oversample together; one plugin
    /// between them splits the series and each side is judged alone.  A
    /// bypassed block still counts while its model is installed.
    pub(crate) fn nam_factors(&self, effects_empty: bool) -> Vec<u8> {
        let eligible: Vec<bool> = self.nam.iter().map(NamBlock::can_oversample).collect();
        let count = |position: NamPosition| {
            self.nam
                .iter()
                .zip(&eligible)
                .filter(|(block, eligible)| **eligible && block.controls.position == position)
                .count()
        };
        let pre = count(NamPosition::Pre);
        let post = count(NamPosition::Post);
        self.nam
            .iter()
            .zip(eligible)
            .map(|(block, eligible)| {
                if !eligible {
                    return 1;
                }
                let series = if effects_empty {
                    pre + post
                } else if block.controls.position == NamPosition::Pre {
                    pre
                } else {
                    post
                };
                if series >= 2 {
                    2
                } else {
                    1
                }
            })
            .collect()
    }

    /// The gear type of the last active capture before the cabinet: a
    /// capture that already includes a speaker box makes an impulse
    /// response after it a second one.
    pub(crate) fn last_nam_gear(&self) -> Option<String> {
        self.nam
            .iter()
            .rev()
            .find(|block| block.active())
            .and_then(|block| block.gear.clone())
    }
}

impl Default for SignalPath {
    fn default() -> Self {
        Self {
            name: default_path_name(0),
            colour: 0,
            mix: PathMix::default(),
            nam: Vec::new(),
            next_nam_id: 0,
            cab_file: String::new(),
            cab_name: String::new(),
            cab_error: None,
            cab_install: CabInstall::default(),
            cab_raw: None,
        }
    }
}

/// One plugin effect of one path: what plugin editors and GUI windows are
/// keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct EffectKey {
    pub(crate) path: usize,
    pub(crate) effect: usize,
}

/// What the UI shows for one live path (sidebar entry, path chips, cards).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PathInfo {
    pub(crate) slot: usize,
    pub(crate) name: String,
    /// Index into the path palette.
    pub(crate) colour_index: u8,
    /// Built-in stages that are on (pre EQ, gate, post EQ) plus effects.
    pub(crate) stages: usize,
    /// Plugin effects on the path.
    pub(crate) effects: usize,
    /// Plugin latency of the chain, samples and ms at the running rate.
    pub(crate) latency_samples: u32,
    pub(crate) latency_ms: f32,
    pub(crate) level_db: f32,
    pub(crate) mute: bool,
    pub(crate) solo: bool,
    /// The path meters (last block's peaks after the chain, before the
    /// path level and the master).
    pub(crate) meters: (f32, f32),
    /// Whether the path's own map reaches some channel of some device.
    pub(crate) routed: bool,
    /// Whether the path sounds: not muted, and soloed if any path is.
    pub(crate) audible: bool,
    pub(crate) selected: bool,
}

/// The slot with the most DSP time of `(slot, average ns)`; the first of
/// a tie.  `None` when nothing was measured.
pub(crate) fn heaviest_path(times: &[(usize, u64)]) -> Option<usize> {
    let mut heaviest: Option<(usize, u64)> = None;
    for &(slot, time) in times {
        if heaviest.is_none_or(|(_, most)| time > most) {
            heaviest = Some((slot, time));
        }
    }
    heaviest.filter(|(_, most)| *most > 0).map(|(slot, _)| slot)
}

impl RigWerkApp {
    /// The live path slots in display order.  Never empty.
    pub(crate) fn path_slots(&self) -> Vec<usize> {
        let mut slots: Vec<usize> = self
            .path_order
            .iter()
            .copied()
            .filter(|&slot| slot < MAX_PATHS && self.paths[slot].is_some())
            .collect();
        // Robust against an order that missed a slot.
        for slot in 0..MAX_PATHS {
            if self.paths[slot].is_some() && !slots.contains(&slot) {
                slots.push(slot);
            }
        }
        slots
    }

    pub(crate) fn path_count(&self) -> usize {
        self.paths.iter().filter(|path| path.is_some()).count()
    }

    /// One flag per slot: whether a path lives there.
    pub(crate) fn live_path_mask(&self) -> [bool; MAX_PATHS] {
        std::array::from_fn(|slot| self.paths[slot].is_some())
    }

    /// Whether some path is soloed.  A return's solo does not count: a
    /// return is solo-safe and has none.
    pub(crate) fn any_solo(&self) -> bool {
        self.paths
            .iter()
            .flatten()
            .any(|path| path.mix.solo && !path.mix.is_return())
    }

    /// Whether `mix` sounds under the mute and the solos (`any_solo`).
    pub(crate) fn mix_audible(mix: &PathMix, any_solo: bool) -> bool {
        !mix.mute && (!any_solo || mix.solo || mix.is_return())
    }

    /// Each slot's level, mute and solo as one gain (0 for no path), as the
    /// mixer applies it.
    pub(crate) fn path_gains(&self) -> [f32; MAX_PATHS] {
        let any_solo = self.any_solo();
        std::array::from_fn(|slot| match &self.paths[slot] {
            Some(path) if Self::mix_audible(&path.mix, any_solo) => {
                path_level_gain(path.mix.level_db)
            }
            _ => 0.0,
        })
    }

    /// The live returns, in display order.
    pub(crate) fn return_slots(&self) -> Vec<usize> {
        self.path_slots()
            .into_iter()
            .filter(|&slot| self.paths[slot].as_ref().is_some_and(|p| p.mix.is_return()))
            .collect()
    }

    /// The live paths fed by the input, in display order: the ones that
    /// can send.
    pub(crate) fn sender_slots(&self) -> Vec<usize> {
        self.path_slots()
            .into_iter()
            .filter(|&slot| {
                self.paths[slot]
                    .as_ref()
                    .is_some_and(|p| !p.mix.is_return())
            })
            .collect()
    }

    /// Feeds path `slot` from the input or from the other paths' sends.  A
    /// new return keeps no sends or solo of its own, and its routing goes
    /// back to the default (it has no input to route).  A path that stops
    /// being a return takes every send to it along.  The last path fed by
    /// the input stays on it: a return would have nothing to hear.
    pub(crate) fn set_path_feed(&mut self, slot: usize, feed: PathFeed) {
        if feed == PathFeed::Sends && self.sender_slots() == [slot] {
            self.push_log(
                "A return needs another path that plays the input.",
                LogTone::Amber,
            );
            return;
        }
        let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        if path.mix.feed == feed {
            return;
        }
        path.mix.feed = feed;
        let name = path.name.clone();
        if feed == PathFeed::Sends {
            let defaults = PathMix::default();
            path.mix.sends = defaults.sends;
            path.mix.solo = false;
            path.mix.routes = defaults.routes;
            path.mix.mono = defaults.mono;
            self.push_log(
                format!("{name} now plays the other paths' sends."),
                LogTone::Teal,
            );
        } else {
            self.clear_sends_to(slot);
            self.push_log(format!("{name} now plays the input."), LogTone::Teal);
        }
    }

    /// Path `sender`'s send to return `ret`, dB ([`SEND_OFF_DB`] is off).
    /// Ignored unless `sender` is fed by the input and `ret` is another
    /// path's return.
    pub(crate) fn set_path_send(&mut self, sender: usize, ret: usize, level_db: f32) {
        if sender == ret
            || ret >= MAX_PATHS
            || !self
                .paths
                .get(ret)
                .and_then(Option::as_ref)
                .is_some_and(|p| p.mix.is_return())
        {
            return;
        }
        if let Some(path) = self.paths.get_mut(sender).and_then(Option::as_mut) {
            if !path.mix.is_return() {
                path.mix.sends[ret] = sanitize_send(level_db);
            }
        }
    }

    /// Path `sender`'s send to `ret`, dB.
    pub(crate) fn path_send(&self, sender: usize, ret: usize) -> f32 {
        self.paths
            .get(sender)
            .and_then(Option::as_ref)
            .and_then(|path| path.mix.sends.get(ret).copied())
            .unwrap_or(SEND_OFF_DB)
    }

    /// Turns off every send to slot `slot`.
    fn clear_sends_to(&mut self, slot: usize) {
        for path in self.paths.iter_mut().flatten() {
            if let Some(level) = path.mix.sends.get_mut(slot) {
                *level = SEND_OFF_DB;
            }
        }
    }

    /// The selected slot, or the first live one if the selection is stale.
    fn selected_slot(&self) -> usize {
        if self
            .paths
            .get(self.selected_path)
            .is_some_and(Option::is_some)
        {
            self.selected_path
        } else {
            self.path_slots().first().copied().unwrap_or(0)
        }
    }

    /// The path the Mixer and Plugins pages edit.
    pub(crate) fn path(&self) -> &SignalPath {
        static FALLBACK: std::sync::OnceLock<SignalPath> = std::sync::OnceLock::new();
        self.paths[self.selected_slot()]
            .as_ref()
            .unwrap_or_else(|| FALLBACK.get_or_init(SignalPath::default))
    }

    /// The path the Mixer and Plugins pages edit, for changing it.
    pub(crate) fn path_mut(&mut self) -> &mut SignalPath {
        let slot = self.selected_slot();
        self.selected_path = slot;
        self.paths[slot].get_or_insert_with(SignalPath::default)
    }

    /// Makes `slot` the path the Mixer and Plugins pages edit.  The Mixer's
    /// effect selection and the next effect position belong to a path, so
    /// they are reset when the path changes.
    pub(crate) fn select_path(&mut self, slot: usize) -> bool {
        if !self.paths.get(slot).is_some_and(Option::is_some) {
            return false;
        }
        if slot != self.selected_path {
            self.plugin_effect_at = None;
            if let ChainSelection::Plugin(index) = self.chain_selected {
                if index >= self.engine.plugin_views(slot).len() {
                    self.chain_selected = ChainSelection::Plugins;
                }
            }
        }
        self.selected_path = slot;
        true
    }

    /// What the UI shows for every live path, in display order.
    pub(crate) fn path_infos(&self) -> Vec<PathInfo> {
        let any_solo = self.any_solo();
        let rate = match self.snapshot.output_rate {
            0 => self.sample_rate.max(1),
            rate => rate,
        };
        let selected = self.selected_slot();
        let devices = self.device_settings().len();
        self.path_slots()
            .into_iter()
            .filter_map(|slot| {
                let path = self.paths[slot].as_ref()?;
                let effects = self.engine.plugin_views(slot).len();
                let mix = &path.mix;
                let builtins = [
                    mix.eqs[0].is_active(),
                    mix.gate.enabled,
                    path.nam_active(),
                    mix.eqs[1].is_active(),
                ]
                .into_iter()
                .filter(|on| *on)
                .count();
                let latency_samples = self.engine.path_latency_samples(slot);
                Some(PathInfo {
                    slot,
                    name: path.name.clone(),
                    colour_index: path.colour,
                    stages: builtins + effects,
                    effects,
                    latency_samples,
                    latency_ms: latency_samples as f32 * 1000.0 / rate as f32,
                    level_db: mix.level_db,
                    mute: mix.mute,
                    solo: mix.solo,
                    meters: self.engine.path_meters(slot),
                    routed: (0..devices).any(|device| self.path_reaches_device(slot, device)),
                    audible: Self::mix_audible(mix, any_solo),
                    selected: slot == selected,
                })
            })
            .collect()
    }

    /// Path `slot`'s gate gain reduction in dB from the live snapshot: the
    /// deepest of the last block, 0 while the gate is open or off, and
    /// while no bridge runs.
    #[cfg_attr(not(test), allow(dead_code))] // For the gate sheet's meter.
    pub(crate) fn gate_reduction_db(&self, slot: usize) -> f32 {
        if self.engine.state != EngineState::Running {
            return 0.0;
        }
        self.snapshot
            .gate_reduction_db
            .get(slot)
            .copied()
            .unwrap_or(0.0)
    }

    /// Whether another path fits (a slot is free in the app and the
    /// engine has finished any fade-out there).
    pub(crate) fn can_add_path(&self) -> bool {
        (0..MAX_PATHS).any(|slot| {
            self.paths[slot].is_none() && self.engine.path_state(slot) == PathState::Inactive
        })
    }

    /// Why another path does not fit, or `None` while one does.
    pub(crate) fn add_path_blocker(&self) -> Option<&'static str> {
        if self.can_add_path() {
            None
        } else if self.path_count() >= MAX_PATHS {
            Some("Four paths is the most RigWerk runs.")
        } else {
            Some("A removed path is still fading out. Try again in a moment.")
        }
    }

    /// The first palette colour no live path uses.
    fn free_colour(&self) -> u8 {
        (0..MAX_PATHS as u8)
            .find(|colour| {
                !self
                    .paths
                    .iter()
                    .flatten()
                    .any(|path| path.colour == *colour)
            })
            .unwrap_or(0)
    }

    fn name_taken(&self, name: &str) -> bool {
        self.paths
            .iter()
            .flatten()
            .any(|path| path.name.eq_ignore_ascii_case(name))
    }

    /// "Path N" for an added path, with N the first number from the new
    /// path count up that no live path uses.
    fn new_path_name(&self) -> String {
        let mut number = self.path_count() + 1;
        loop {
            let name = format!("Path {number}");
            if !self.name_taken(&name) {
                return name;
            }
            number += 1;
        }
    }

    /// The name for a copy of `name`: "Main" → "Main 2", "Main 2" →
    /// "Main 3", skipping names live paths use.
    fn copy_name(&self, name: &str) -> String {
        let (base, mut number) = match name.rsplit_once(' ') {
            Some((base, digits))
                if !base.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) =>
            {
                (base, digits.parse::<usize>().map_or(2, |n| n + 1))
            }
            _ => (name, 2),
        };
        loop {
            let candidate = format!("{base} {number}");
            if !self.name_taken(&candidate) {
                return candidate;
            }
            number += 1;
        }
    }

    /// Adds an empty path, live: default routing and EQs, no plugins,
    /// muted, routed to no device.  Selects it.  Returns its slot.
    pub(crate) fn add_path(&mut self) -> Option<usize> {
        let slot = self.engine.add_path()?;
        let path = SignalPath {
            name: self.new_path_name(),
            colour: self.free_colour(),
            mix: self.engine.path_mix(slot),
            ..SignalPath::default()
        };
        self.paths[slot] = Some(path);
        self.path_order.retain(|&other| other != slot);
        self.path_order.push(slot);
        for settings in self.device_settings_mut() {
            settings.path_map[slot] = PathMapSettings::unrouted();
        }
        self.push_devices_to_engine();
        self.select_path(slot);
        self.push_log(
            format!("Added path {}, muted.", self.path().name),
            LogTone::Teal,
        );
        Some(slot)
    }

    /// Adds a return, live, like [`Self::add_path`] (muted, routed to no
    /// device), named *FX* (or *FX 2*, …), with every send to it off.
    /// Selects it.  Returns its slot.
    pub(crate) fn add_return(&mut self) -> Option<usize> {
        let name = if self.name_taken("FX") {
            self.copy_name("FX")
        } else {
            "FX".to_owned()
        };
        let slot = self.add_path()?;
        if let Some(path) = self.paths[slot].as_mut() {
            path.name = name;
        }
        self.set_path_feed(slot, PathFeed::Sends);
        self.push_paths_to_engine();
        Some(slot)
    }

    /// Adds a muted copy of path `source`, live: its chain, its plugins
    /// with their current state (an open plugin window is synced first),
    /// and its map, level and polarity on every device.  Selects it.
    /// Returns its slot.
    pub(crate) fn duplicate_path(&mut self, source: usize) -> Option<usize> {
        let original = self.paths.get(source)?.clone()?;
        if let Some(key) = self.plugin_gui.as_ref().map(|gui| gui.key) {
            if key.path == source {
                // What the plugin's own window changed since its last sync.
                self.engine.finish_plugin_editor(key.path, key.effect);
            }
        }
        let slot = self.engine.duplicate_path(source)?;
        let path = SignalPath {
            name: self.copy_name(&original.name),
            colour: self.free_colour(),
            mix: PathMix {
                mute: true,
                solo: false,
                ..original.mix
            },
            // The copy loads its own models and its own impulse.
            // Generations count per slot, so the source's stamp could
            // match the new slot.
            nam: original
                .nam
                .iter()
                .map(|block| NamBlock {
                    install: NamInstall::default(),
                    ..block.clone()
                })
                .collect(),
            cab_install: CabInstall::default(),
            cab_raw: None,
            ..original
        };
        self.paths[slot] = Some(path);
        self.path_order.retain(|&other| other != slot);
        self.path_order.push(slot);
        for settings in self.device_settings_mut() {
            settings.path_map[slot] = settings.path_map[source];
        }
        self.push_devices_to_engine();
        self.select_path(slot);
        self.push_log(
            format!(
                "Duplicated {} as {}, muted.",
                original.name,
                self.path().name
            ),
            LogTone::Teal,
        );
        Some(slot)
    }

    /// Removes path `slot`: it fades out in the engine (where it played:
    /// its device entries stay until the slot is reused), and its editors
    /// close.  The last path cannot be removed.
    pub(crate) fn remove_path(&mut self, slot: usize) -> bool {
        if self.paths.get(slot).is_none_or(Option::is_none) || self.path_count() <= 1 {
            return false;
        }
        if !self.engine.remove_path(slot) {
            return false;
        }
        if self
            .plugin_gui
            .as_ref()
            .is_some_and(|gui| gui.key.path == slot)
        {
            self.plugin_gui = None;
        }
        if self.plugin_editor.is_some_and(|key| key.path == slot) {
            self.plugin_editor = None;
            self.plugin_param_filter.clear();
        }
        let removed = self.paths[slot].take();
        self.clear_sends_to(slot);
        if self.match_paths_reference == Some(slot) {
            self.match_paths_reference = None;
        }
        self.path_order.retain(|&other| other != slot);
        if self.selected_path == slot {
            let next = self.selected_slot();
            self.select_path(next);
        }
        if let Some(removed) = removed {
            self.push_log(format!("Removed path {}.", removed.name), LogTone::Amber);
        }
        true
    }

    /// Sets path `slot`'s loudness-match trim; the mixer ramps it in.
    pub(crate) fn set_path_match(&mut self, slot: usize, match_db: f32) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.match_db = crate::audio::sanitize_path_match(match_db);
        }
    }

    pub(crate) fn set_path_level(&mut self, slot: usize, level_db: f32) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.level_db = sanitize_path_level(level_db);
        }
    }

    /// The edited path's EQ.  Goes through [`Self::path_mut`], so a stale
    /// selection is repaired the same way a direct edit was.
    pub(crate) fn set_path_eq(&mut self, index: usize, eq: EqSettings) {
        self.path_mut().mix.eqs[index] = eq;
    }

    /// The edited path's gate.
    pub(crate) fn set_path_gate(&mut self, gate: GateSettings) {
        self.path_mut().mix.gate = gate;
    }

    /// The edited path's cabinet controls.  Does not read an impulse.
    /// The cuts and the level are clamped here, not in the sheet.
    pub(crate) fn set_path_cab(&mut self, cab: crate::audio::CabControls) {
        self.path_mut().mix.cab = cab.sanitized();
        self.push_paths_to_engine();
    }

    /// Mono sum for one path slot.
    pub(crate) fn set_path_mono(&mut self, slot: usize, mono: bool) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.mono = mono;
        }
    }

    /// The edited path's output-channel enables.
    pub(crate) fn set_path_output_enabled(&mut self, enabled: [bool; 2]) {
        self.path_mut().mix.output_enabled = enabled;
    }

    /// Skips every effect on the selected path.  Each effect's own bypass
    /// is left as it is.  Pushed now, so the audio thread sees it before
    /// the next frame.
    pub(crate) fn set_chain_bypass(&mut self, bypass: bool) {
        self.path_mut().mix.chain_bypass = bypass;
        self.push_paths_to_engine();
    }

    pub(crate) fn set_path_mute(&mut self, slot: usize, mute: bool) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.mute = mute;
        }
    }

    /// A return has no solo: it is solo-safe.
    pub(crate) fn set_path_solo(&mut self, slot: usize, solo: bool) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.solo = solo && !path.mix.is_return();
        }
    }

    /// Renames path `slot` to `name`, trimmed.  An empty name, or one
    /// another live path already uses (ignoring case), is refused: the
    /// sidebar, chips and routing rows tell paths apart by name.
    pub(crate) fn rename_path(&mut self, slot: usize, name: &str) -> bool {
        let name = name.trim();
        let taken = self.paths.iter().enumerate().any(|(other, path)| {
            other != slot
                && path
                    .as_ref()
                    .is_some_and(|path| path.name.eq_ignore_ascii_case(name))
        });
        match self.paths.get_mut(slot).and_then(Option::as_mut) {
            Some(path) if !name.is_empty() && !taken => {
                path.name = name.to_owned();
                true
            }
            _ => false,
        }
    }

    pub(crate) fn set_path_colour(&mut self, slot: usize, colour: u8) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.colour = colour % MAX_PATHS as u8;
        }
    }

    /// Path `slot`'s input routing (2 × 4; taps 1/2 are the path's L/R).
    pub(crate) fn set_path_routes(&mut self, slot: usize, routes: [[bool; 4]; 2]) {
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.routes = routes;
        }
    }

    /// What the *Source* button reads: the inputs feeding path `slot` and
    /// *mono* or *stereo* (`In 1 · mono`, `In 1 + In 2 · stereo`), then
    /// `· L only` or `· R only` while one path output is off.
    pub(crate) fn source_label(&self, slot: usize) -> String {
        let Some(path) = self.paths.get(slot).and_then(Option::as_ref) else {
            return String::new();
        };
        let mix = &path.mix;
        if mix.is_return() {
            let senders = self
                .paths
                .iter()
                .flatten()
                .filter(|other| {
                    !other.mix.is_return() && path_level_gain(other.mix.sends[slot]) > 0.0
                })
                .count();
            return match senders {
                0 => "Sends · none yet".to_owned(),
                1 => "Sends · 1 path".to_owned(),
                count => format!("Sends · {count} paths"),
            };
        }
        let fed = |input: usize| mix.routes[input][0] || mix.routes[input][1];
        let inputs = match (fed(0), fed(1)) {
            (true, true) => "In 1 + In 2",
            (true, false) => "In 1",
            (false, true) => "In 2",
            (false, false) => "No input",
        };
        let mut label = format!("{inputs} · {}", if mix.mono { "mono" } else { "stereo" });
        match mix.output_enabled {
            [true, false] => label.push_str(" · L only"),
            [false, true] => label.push_str(" · R only"),
            [false, false] => label.push_str(" · silent"),
            [true, true] => {}
        }
        label
    }

    /// One cell of path `slot`'s source grid: whether input `input` (0 is
    /// *In 1*) feeds its `side` (0 is L).
    pub(crate) fn set_path_source(&mut self, slot: usize, input: usize, side: usize, on: bool) {
        if input >= 2 || side >= 2 {
            return;
        }
        if let Some(path) = self.paths.get_mut(slot).and_then(Option::as_mut) {
            path.mix.routes[input][side] = on;
        }
    }

    /// *Remove* in a path's name menu, a two-click confirm: the first click
    /// arms it (`false`), the second removes the path (`true` once it is
    /// gone).  Arming another path disarms the first.
    pub(crate) fn request_remove_path(&mut self, slot: usize) -> bool {
        if self.chrome.remove_confirm != Some(slot) {
            self.chrome.remove_confirm = Some(slot);
            return false;
        }
        self.chrome.remove_confirm = None;
        self.remove_path(slot)
    }

    /// The path whose strip carries the DSP marker: the one with the
    /// largest share of the block, while the total DSP alarm is raised.
    pub(crate) fn dsp_marker_path(&self) -> Option<usize> {
        self.issue_state.dsp.severity()?;
        let times: Vec<(usize, u64)> = self
            .path_slots()
            .into_iter()
            .filter_map(|slot| Some((slot, u64::from(self.engine.path_dsp_time(slot)?.avg_ns))))
            .collect();
        heaviest_path(&times)
    }

    /// Whether a shown issue is anchored to path `slot`, or to a stage on it.
    pub(crate) fn path_has_issue(&self, slot: usize) -> bool {
        self.anchor_severity(crate::app::issues::Anchor::Path(slot))
            .is_some()
    }

    /// The output devices' settings (0 = clock output, then extra outputs).
    pub(crate) fn device_settings(&self) -> Vec<&OutputDeviceSettings> {
        std::iter::once(&self.clock_output)
            .chain(
                self.extra_outputs
                    .iter()
                    .map(|extra_output| &extra_output.settings),
            )
            .collect()
    }

    pub(super) fn device_settings_mut(
        &mut self,
    ) -> impl Iterator<Item = &mut OutputDeviceSettings> {
        std::iter::once(&mut self.clock_output).chain(
            self.extra_outputs
                .iter_mut()
                .map(|extra_output| &mut extra_output.settings),
        )
    }

    /// Output device `device`'s channel count, as the device rows show it.
    pub(crate) fn device_channels(&self, device: usize) -> usize {
        self.output_device_rows()
            .get(device)
            .map_or(2, |row| row.channels)
    }

    /// Whether path `slot`'s own map reaches some channel of output device
    /// `device` (0 = clock output, then extra outputs).  The mono copy on
    /// unreached channels does not count.
    pub(crate) fn path_reaches_device(&self, slot: usize, device: usize) -> bool {
        if slot >= MAX_PATHS || self.paths[slot].is_none() {
            return false;
        }
        let Some(settings) = self.device_settings().get(device).copied() else {
            return false;
        };
        settings.map(slot).reaches_any(self.device_channels(device))
    }

    /// Path `slot`'s map, level, polarity and trim on output device
    /// `device` (0 = clock output, then extra outputs).
    pub(crate) fn device_path_map(&self, device: usize, slot: usize) -> Option<PathMapSettings> {
        let settings = self.device_settings().get(device).copied()?;
        settings.path_map.get(slot).copied()
    }

    /// Puts music on channels 1 and 2 and the click alone on 3 and 4 of
    /// output device `device` (0 = clock, then extra outputs).  An unrouted
    /// path stays unrouted.  Level, mute, alignment, trims and polarity
    /// stay.  Bits of the click mask at or past the device width stay too.
    pub(crate) fn apply_click_pair(&mut self, device: usize) {
        let channels = self.device_channels(device);
        let Some(settings) = self.device_settings_mut().nth(device) else {
            return;
        };
        for slot in 0..MAX_PATHS {
            let map = settings.path_map[slot].map;
            if map.is_empty() {
                continue;
            }
            settings.path_map[slot].map = map.with_preset(MapPreset::Stereo12, channels);
        }
        settings.unused = UnusedChannels::Silent;
        // Channels 3 and 4 are bits 2 and 3.  Bits outside this device's
        // width are kept, so a smaller device does not forget a wider mask.
        let width = low_bits(channels);
        settings.click_mask = (settings.click_mask & !width) | (0b0000_1100 & width);
    }

    /// Puts the click on channel `channel` of output device `device` (0 =
    /// clock, then extra outputs), or takes it off.  Only that bit of the click
    /// mask moves; routing is unchanged.  The frame pushes it with the
    /// device settings.
    pub(crate) fn set_click_channel(&mut self, device: usize, channel: usize, on: bool) {
        if channel >= MAX_OUTPUT_CHANNELS {
            return;
        }
        let Some(settings) = self.device_settings_mut().nth(device) else {
            return;
        };
        let bit = 1u8 << channel;
        if on {
            settings.click_mask |= bit;
        } else {
            settings.click_mask &= !bit;
        }
    }

    /// Sets path `slot`'s map, level, polarity and trim on output device
    /// `device` (0 = clock output, then extra outputs).  Live, crossfaded.
    pub(crate) fn set_device_path_map(
        &mut self,
        device: usize,
        slot: usize,
        entry: PathMapSettings,
    ) {
        if slot >= MAX_PATHS {
            return;
        }
        if let Some(settings) = self.device_settings_mut().nth(device) {
            settings.path_map[slot] = entry.sanitized();
        }
    }

    /// Pushes every live path's settings to the engine.  Called each frame.
    /// The NAM blocks go separately: their stack is too large for the
    /// [`PathMix`] snapshot.
    pub(crate) fn push_paths_to_engine(&self) {
        for (slot, path) in self.paths.iter().enumerate() {
            if let Some(path) = path {
                self.engine.set_path_mix(slot, &path.mix);
                self.engine.set_path_nam(slot, &path.nam_stack());
            }
        }
    }

    /// Pushes the clock output and the extra outputs to the engine.
    pub(crate) fn push_devices_to_engine(&mut self) {
        self.engine.set_clock_output(&self.clock_output);
        self.push_extra_outputs_to_engine();
    }

    /// Replaces every path with `prefs` (a loaded session) and the
    /// output devices with `clock` and `extra_outputs`, whose `path_map` is in
    /// the order of `prefs`.  The paths reuse the live slots first, in
    /// display order (so a live path just changes its settings, with the
    /// usual ramps), then free slots.  Live paths left over are removed with
    /// a fade, and keep their device entries until it ends.  Plugins are
    /// restored, not opened; a running bridge opens them on the next frame
    /// and the mixer keeps the path silent until then.
    pub(crate) fn install_paths(
        &mut self,
        prefs: &[PathPref],
        clock: OutputDeviceSettings,
        extra_outputs: Vec<ExtraOutputPref>,
        selected: usize,
    ) {
        let prefs: Vec<PathPref> = if prefs.is_empty() {
            vec![PathPref::default()]
        } else {
            prefs.iter().take(MAX_PATHS).cloned().collect()
        };
        // Editors belong to the chains being replaced.
        self.finish_plugin_gui();
        self.plugin_editor = None;
        self.plugin_param_filter.clear();
        self.plugin_effect_at = None;

        // Live in the app, then live only in the engine (a fresh start),
        // then free, then still fading out.
        let mut candidates = self.path_slots();
        for wanted in [PathState::Active, PathState::Inactive, PathState::FadingOut] {
            for slot in 0..MAX_PATHS {
                if !candidates.contains(&slot)
                    && self.paths[slot].is_none()
                    && self.engine.path_state(slot) == wanted
                {
                    candidates.push(slot);
                }
            }
        }
        let targets: Vec<usize> = candidates.into_iter().take(prefs.len()).collect();
        // Slots that keep sounding for a while without a new path: those
        // about to be removed and those already fading out.
        let fading: [bool; MAX_PATHS] = std::array::from_fn(|slot| {
            !targets.contains(&slot)
                && (self.paths[slot].is_some()
                    || self.engine.path_state(slot) != PathState::Inactive)
        });

        let remap = |new: &OutputDeviceSettings, old: Option<&OutputDeviceSettings>| {
            let mut out = *new;
            for (slot, entry) in out.path_map.iter_mut().enumerate() {
                *entry = match targets.iter().position(|&target| target == slot) {
                    Some(position) => new.path_map[position],
                    // A fading path fades where it played; on a device it
                    // did not play on, it stays off.
                    None if fading[slot] => {
                        old.map_or(PathMapSettings::unrouted(), |old| old.path_map[slot])
                    }
                    None => PathMapSettings::default(),
                };
            }
            out
        };
        let old_extra_outputs: Vec<OutputDeviceSettings> = self
            .extra_outputs
            .iter()
            .map(|extra_output| extra_output.settings)
            .collect();
        self.clock_output = remap(&clock, Some(&self.clock_output));
        self.extra_outputs = extra_outputs
            .into_iter()
            .enumerate()
            .map(|(index, extra_output)| ExtraOutputPref {
                settings: remap(&extra_output.settings, old_extra_outputs.get(index)),
                ..extra_output
            })
            .collect();

        for (pref, &slot) in prefs.iter().zip(&targets) {
            let first_id = self.unused_nam_id(slot);
            let mut path = SignalPath::from_pref(pref, first_id);
            path.mix.sends = pref.sends_by_slot(&targets);
            self.engine
                .restore_path(slot, &path.mix, &path.nam_stack(), pref.plugins.clone());
            self.paths[slot] = Some(path);
        }
        for slot in 0..MAX_PATHS {
            if targets.contains(&slot) {
                continue;
            }
            if self.paths[slot].take().is_some()
                || self.engine.path_state(slot) == PathState::Active
            {
                self.engine.remove_path(slot);
            }
        }
        self.path_order = targets.clone();
        self.selected_path = targets[selected.min(targets.len() - 1)];
        if let ChainSelection::Plugin(_) = self.chain_selected {
            self.chain_selected = ChainSelection::Plugins;
        }
        self.push_paths_to_engine();
        self.push_devices_to_engine();
        self.sync_nam_models();
        self.sync_cab_files();
        // A preset that was saved on a bigger buffer may not fit this one.
        // The budget is shared, so every path counts, selected or not.
        self.check_nam_load();
    }

    /// The first NAM block id path slot `slot` has not handed out: past the
    /// live path's ids and past every id the engine still publishes there
    /// (a slot live only in the engine, or still fading out).  0 for a slot
    /// that holds nothing.
    fn unused_nam_id(&self, slot: usize) -> u32 {
        let app = self
            .paths
            .get(slot)
            .and_then(Option::as_ref)
            .map_or(0, |path| path.next_nam_id);
        if slot >= MAX_PATHS || self.engine.path_state(slot) == PathState::Inactive {
            return app;
        }
        let engine = self
            .engine
            .path_nam(slot)
            .blocks()
            .iter()
            .map(|block| block.id.wrapping_add(1))
            .max()
            .unwrap_or(0);
        app.max(engine)
    }

    /// The live paths as they should be saved (fresh plugin state), and
    /// the slot order they are saved in (the display order).
    ///
    /// The open editor is read in full first: a plugin may change a value
    /// without reporting it, and the save lists the effects from what that
    /// read stored.  No editor open makes this a no-op.
    pub(crate) fn saved_paths(&self) -> (Vec<PathPref>, Vec<usize>) {
        self.sync_plugin_editor();
        let order = self.path_slots();
        let prefs = order
            .iter()
            .filter_map(|&slot| {
                let path = self.paths[slot].as_ref()?;
                Some(path.to_pref(self.engine.plugin_prefs(slot), &order))
            })
            .collect();
        (prefs, order)
    }

    /// The selected path as a position in the saved order.
    pub(crate) fn saved_selected_path(&self, order: &[usize]) -> usize {
        let selected = self.selected_slot();
        order.iter().position(|&slot| slot == selected).unwrap_or(0)
    }

    /// The clock output and the extra outputs with `path_map` compacted to the
    /// saved path order.
    pub(crate) fn saved_devices(
        &self,
        order: &[usize],
    ) -> (OutputDeviceSettings, Vec<ExtraOutputPref>) {
        let clock = self.clock_output.with_path_order(order);
        let extra_outputs = self
            .extra_outputs
            .iter()
            .map(|extra_output| ExtraOutputPref {
                settings: extra_output.settings.with_path_order(order),
                ..extra_output.clone()
            })
            .collect();
        (clock, extra_outputs)
    }
}

#[cfg(test)]
mod tests;
