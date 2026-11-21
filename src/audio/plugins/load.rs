//! Plugin loading, the restore order, state capture, and the helpers that
//! keep the saved control values in step with the plugin.  UI thread.

use std::path::PathBuf;
use std::sync::Arc;

use super::super::clap::ClapInstance;
use super::super::lv2::Lv2Instance;
use super::super::vst3::Vst3Controller;
use super::handle::{LoadedPlugin, Saved};
use super::link::PluginShared;
use super::pref::{PluginFormat, PluginParamPref, PluginParamView, PluginPref, PluginState};

/// One control value a plugin reported as changed, with the text its own
/// formatter gives it.
pub(crate) struct ParamChange {
    pub(crate) id: u32,
    pub(crate) value: f64,
    pub(crate) display: Option<String>,
}

/// Applies editor edits to the cached views and the saved values, touching
/// only the ids they name.  Every known change is applied; the
/// return value is `false` when any id was not in the cached views (a control
/// the plugin added or renamed), which asks the caller for a full read.  The
/// saved list keeps [`merge_synced_params`]'s semantics: every live value
/// away from its default, plus saved entries the plugin does not list.
/// Changes that are already at their default therefore leave it.
pub(crate) fn apply_parameter_changes(
    parameters: &mut [PluginParamView],
    saved: &mut Vec<PluginParamPref>,
    changes: &[ParamChange],
) -> bool {
    if changes.is_empty() {
        return true;
    }
    let mut all_known = true;
    for change in changes {
        match parameters
            .iter_mut()
            .find(|parameter| parameter.id == change.id)
        {
            Some(parameter) => {
                parameter.value = change.value;
                if let Some(display) = &change.display {
                    parameter.display = display.clone();
                }
            }
            None => all_known = false,
        }
    }
    *saved = merge_synced_params(saved, parameters);
    all_known
}

/// The capture of `bytes` a plugin saved, with `params` as the blob's
/// snapshot of the saved values.  An empty or oversized blob is a failed
/// capture.
pub(crate) fn captured_state(bytes: &[u8], params: &[PluginParamPref]) -> Capture {
    if bytes.is_empty() {
        Capture::Failed
    } else if bytes.len() > MAX_STATE_BLOB {
        eprintln!(
            "[plugins] not saving a plugin's state: it is larger than {} MB",
            MAX_STATE_BLOB / (1024 * 1024)
        );
        Capture::Failed
    } else {
        Capture::Saved(PluginState::capture(bytes, params))
    }
}

/// The largest state blob kept in the preferences file.
pub(crate) const MAX_STATE_BLOB: usize = 8 * 1024 * 1024;

/// The outcome of [`super::PluginHandle::capture_state`].
#[derive(Debug)]
pub(crate) enum Capture {
    Saved(PluginState),
    /// The plugin cannot save its state; the previous blob is kept.
    Failed,
    /// Values are still queued for the plugin, so its state would be older
    /// than the saved parameters; the slot stays stale and the next save
    /// tries again.
    Deferred,
}

impl Capture {
    /// Whether the slot still needs a capture afterwards.
    pub(crate) fn leaves_stale(&self) -> bool {
        matches!(self, Capture::Deferred)
    }
}

/// The largest block a plugin is asked to process, and the block size it is
/// set up with.  Many plugins allocate fixed internal buffers for the size
/// announced at setup and ignore the smaller `numSamples` passed later, so a
/// big 4096-frame announcement makes them write past those buffers and can
/// take the whole app down (Eye Scream Man does exactly this).  512 frames is
/// the size every VST3/CLAP effect can be expected to handle, and longer
/// bridge blocks are split into chunks of this size below.
pub(crate) const MAX_BLOCK: usize = 512;

/// What a fresh instance gets restored with, in this order: the opaque state
/// blob, then the parameter values it does not already cover.
pub(crate) struct Restore<'a> {
    /// Decoded state bytes, if the pref holds a usable blob.
    pub(crate) state: Option<Vec<u8>>,
    pref: &'a PluginPref,
}

impl<'a> Restore<'a> {
    pub(crate) fn from_pref(pref: &'a PluginPref) -> Self {
        Self {
            state: pref.state.as_ref().and_then(PluginState::bytes),
            pref,
        }
    }

    /// The parameter values to apply once the state blob was (or was not)
    /// loaded.  `None` asks for the parameter's default.
    pub(crate) fn params(&self, state_loaded: bool) -> Vec<(u32, Option<f64>)> {
        let snapshot = if state_loaded {
            self.pref
                .state
                .as_ref()
                .map(|state| state.params.as_slice())
        } else {
            None
        };
        params_to_restore(&self.pref.params, snapshot)
    }
}

/// The values two parameter lists may differ by and still count as equal.
const PARAM_EPSILON: f64 = 0.0005;

/// Which saved parameters still need applying.  Without a restored state
/// blob that is every saved value.  After one, it is only what changed since
/// the blob was captured: a value that differs from (or is missing in) the
/// blob's snapshot, and a parameter that went back to its default (`None`),
/// which the saved list no longer names.  An unchanged list applies
/// nothing, so the blob takes precedence.
pub(crate) fn params_to_restore(
    saved: &[PluginParamPref],
    snapshot: Option<&[PluginParamPref]>,
) -> Vec<(u32, Option<f64>)> {
    let Some(snapshot) = snapshot else {
        return saved
            .iter()
            .map(|saved| (saved.id, Some(saved.value)))
            .collect();
    };
    let changed = saved.iter().filter(|saved| {
        !snapshot.iter().any(|captured| {
            captured.id == saved.id && (captured.value - saved.value).abs() <= PARAM_EPSILON
        })
    });
    let reset = snapshot
        .iter()
        .filter(|captured| !saved.iter().any(|saved| saved.id == captured.id));
    changed
        .map(|saved| (saved.id, Some(saved.value)))
        .chain(reset.map(|captured| (captured.id, None)))
        .collect()
}

/// The saved parameter list after reading the plugin's live controls: every
/// writable control away from its default, plus any saved entry the plugin
/// does not list (an id hidden or renamed by this plugin version), which a
/// sync must not silently delete.  A read-only control is output the plugin
/// reports, not a setting this app owns, so it is never saved.
pub(crate) fn merge_synced_params(
    previous: &[PluginParamPref],
    live: &[PluginParamView],
) -> Vec<PluginParamPref> {
    let unseen = previous
        .iter()
        .filter(|saved| !live.iter().any(|parameter| parameter.id == saved.id))
        .cloned();
    live.iter()
        .filter(|parameter| {
            !parameter.read_only && (parameter.value - parameter.default).abs() > PARAM_EPSILON
        })
        .map(|parameter| PluginParamPref {
            id: parameter.id,
            value: parameter.value,
        })
        .chain(unseen)
        .collect()
}

/// Opens the plugin with its saved state and controls applied.  The flag says
/// whether the saved state blob still describes the new instance exactly (it
/// restored and no newer control value had to be applied on top); the
/// instance starts stale otherwise.
pub(crate) fn load_plugin(
    pref: &PluginPref,
    sample_rate: u32,
) -> Result<(LoadedPlugin, bool), String> {
    #[cfg(test)]
    if pref.path.starts_with(TEST_PATH) {
        return load_for_test(pref, sample_rate);
    }
    let restore = Restore::from_pref(pref);
    let saved = |current: bool| Saved {
        params: pref.params.clone(),
        state: pref.state.clone(),
        stale: !current,
    };
    match pref.format {
        PluginFormat::Vst3 => {
            let path = PathBuf::from(&pref.path);
            if !path.exists() {
                return Err(format!("Plugin not found: {}", path.display()));
            }
            let class_id = pref
                .class_id
                .as_deref()
                .filter(|class_id| !class_id.trim().is_empty());
            let shared = Arc::new(PluginShared::new(0));
            Vst3Controller::load(&path, class_id, sample_rate, &restore, &shared)
                .map_err(|error| format!("Load failed: {error}"))
                .map(|loaded| {
                    let current = loaded.state_current;
                    (LoadedPlugin::vst3(loaded, shared, saved(current)), current)
                })
        }
        PluginFormat::Clap => {
            let plugin_id = pref
                .class_id
                .as_deref()
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| "The CLAP plugin id is missing.".to_owned())?;
            let path = PathBuf::from(&pref.path);
            if !path.exists() {
                return Err(format!("Plugin not found: {}", path.display()));
            }
            let shared = Arc::new(PluginShared::new(0));
            ClapInstance::load(&path, plugin_id, sample_rate, MAX_BLOCK, &restore, &shared).map(
                |loaded| {
                    let current = loaded.state_current;
                    (LoadedPlugin::clap(loaded, shared, saved(current)), current)
                },
            )
        }
        PluginFormat::Lv2 => {
            let plugin_uri = pref
                .class_id
                .as_deref()
                .filter(|uri| !uri.trim().is_empty())
                .ok_or_else(|| "The LV2 plugin URI is missing.".to_owned())?;
            // The effect's path is the bundle folder, not the binary.
            let path = PathBuf::from(&pref.path);
            if !path.exists() {
                return Err(format!("Plugin not found: {}", path.display()));
            }
            let shared = Arc::new(PluginShared::new(0));
            Lv2Instance::load(&path, plugin_uri, sample_rate, MAX_BLOCK, &restore, &shared).map(
                |loaded| {
                    let current = loaded.state_current;
                    (LoadedPlugin::lv2(loaded, shared, saved(current)), current)
                },
            )
        }
    }
}

/// Prefs whose path starts with this load through the loader a test set
/// with [`set_test_loader`] (stand-ins that are no binary).
#[cfg(test)]
pub(crate) const TEST_PATH: &str = "test:";

#[cfg(test)]
type TestLoader = Box<dyn FnMut(&PluginPref, u32) -> Result<(LoadedPlugin, bool), String>>;

#[cfg(test)]
thread_local! {
    static TEST_LOADER: std::cell::RefCell<Option<TestLoader>> =
        const { std::cell::RefCell::new(None) };
}

/// Makes [`load_plugin`] on this thread load [`TEST_PATH`] prefs through
/// `loader`, until the guard drops.
#[cfg(test)]
pub(crate) fn set_test_loader(
    loader: impl FnMut(&PluginPref, u32) -> Result<(LoadedPlugin, bool), String> + 'static,
) -> TestLoaderGuard {
    TEST_LOADER.with(|slot| *slot.borrow_mut() = Some(Box::new(loader)));
    TestLoaderGuard(())
}

#[cfg(test)]
pub(crate) struct TestLoaderGuard(());

#[cfg(test)]
impl Drop for TestLoaderGuard {
    fn drop(&mut self) {
        TEST_LOADER.with(|slot| slot.borrow_mut().take());
    }
}

#[cfg(test)]
fn load_for_test(pref: &PluginPref, rate: u32) -> Result<(LoadedPlugin, bool), String> {
    // Taken out while it runs, so a loader may load (or drop) plugins.
    let mut loader = TEST_LOADER
        .with(|slot| slot.borrow_mut().take())
        .ok_or_else(|| "No test loader is set.".to_owned())?;
    let loaded = loader(pref, rate);
    TEST_LOADER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(loader);
        }
    });
    loaded
}
