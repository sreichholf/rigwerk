//! The `IComponentHandler` a plugin's controller reports to: editor edits
//! and restart requests.
//!
//! The callbacks never do the work themselves.  `performEdit` queues the
//! value for the audio side and the edit log ([`ParamOut::push_edit`]), and
//! `restartComponent` only records its flags in [`PluginShared`]; the UI's
//! next service frame acts on both.  A plugin may call from any thread:
//! off the UI thread an edit only try-locks and is dropped when busy, and
//! the restart flags are atomics, so no callback ever waits.

use std::sync::Arc;

use ::vst3::Class;
use ::vst3::Steinberg::Vst::{
    IComponentHandler, IComponentHandler2, IComponentHandler2Trait, IComponentHandlerTrait,
    ParamID, ParamValue,
};
use ::vst3::Steinberg::{kResultFalse, kResultOk, tresult, FIDString, TBool};

use crate::audio::plugins::{ParamOut, PluginShared};

pub(crate) struct ComponentHandler {
    out: Arc<ParamOut>,
    shared: Arc<PluginShared>,
}

impl ComponentHandler {
    pub(crate) fn new(out: Arc<ParamOut>, shared: Arc<PluginShared>) -> Self {
        Self { out, shared }
    }
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler, IComponentHandler2);
}

impl IComponentHandlerTrait for ComponentHandler {
    /// Gestures stay on the plugin's side: no automation is recorded.
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        if self.out.push_edit(id, value.clamp(0.0, 1.0)) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        self.shared.request_restart(flags as u32);
        kResultOk
    }
}

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, state: TBool) -> tresult {
        if state != 0 {
            self.shared.mark_state_dirty();
        }
        kResultOk
    }

    unsafe fn requestOpenEditor(&self, _name: FIDString) -> tresult {
        kResultFalse
    }

    unsafe fn startGroupEdit(&self) -> tresult {
        kResultOk
    }

    unsafe fn finishGroupEdit(&self) -> tresult {
        kResultOk
    }
}

#[cfg(test)]
mod tests;
