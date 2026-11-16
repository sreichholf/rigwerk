use std::sync::Arc;

use ::vst3::ComWrapper;
use ::vst3::Steinberg::Vst::{
    IComponentHandler, IComponentHandler2Trait, IComponentHandlerTrait, RestartFlags_,
};
use ::vst3::Steinberg::{kResultFalse, kResultOk};

use super::ComponentHandler;
use crate::audio::plugins::{param_transfer, ParamIds, ParamOut, PluginShared};

#[test]
fn component_handler_perform_edit_reaches_the_transfer_and_the_edit_log() {
    let ids = Arc::new(ParamIds::new(vec![10, 20]));
    let (sender, mut receiver) = param_transfer(ids.len());
    let out = Arc::new(ParamOut::new(Arc::clone(&ids), sender));
    let shared = Arc::new(PluginShared::new(0));
    let handler = ComWrapper::new(ComponentHandler::new(Arc::clone(&out), Arc::clone(&shared)));
    let handler = handler
        .to_com_ptr::<IComponentHandler>()
        .expect("an IComponentHandler");
    // SAFETY: a live handler; the calls are the plugin's.
    unsafe {
        assert_eq!(handler.beginEdit(20), kResultOk);
        assert_eq!(handler.performEdit(20, 0.25), kResultOk);
        assert_eq!(handler.performEdit(20, 0.5), kResultOk);
        assert_eq!(handler.endEdit(20), kResultOk);
        // An id the plugin never listed goes nowhere.
        assert_eq!(handler.performEdit(30, 0.5), kResultFalse);
    }
    assert!(out.has_queued());
    let mut delivered = Vec::new();
    receiver.drain(|index, value, _| delivered.push((index, value)));
    assert_eq!(delivered.last(), Some(&(1, 0.5)));
    let mut edits = Vec::new();
    out.take_edits(&mut edits);
    assert_eq!(edits, vec![(20, 0.5)]);

    // Restart requests and the dirty mark only set flags.
    let flags = RestartFlags_::kLatencyChanged | RestartFlags_::kParamValuesChanged;
    // SAFETY: as above.
    unsafe {
        assert_eq!(handler.restartComponent(flags), kResultOk);
    }
    assert_eq!(shared.take_restart(), flags as u32);
    let handler2 = handler
        .cast::<::vst3::Steinberg::Vst::IComponentHandler2>()
        .expect("an IComponentHandler2");
    // SAFETY: as above.
    unsafe {
        assert_eq!(handler2.setDirty(1), kResultOk);
    }
    assert!(shared.state_dirty());
}
