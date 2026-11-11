//! ASIO on Windows: the driver callbacks and the paths that use ASIO on
//! one or both sides of the bridge.  `controlPanel` is posted to the bridge
//! thread that created the driver and run between callbacks.

mod callback;
mod duplex;
mod input;
mod output;
mod panel;
mod samples;

pub(crate) use input::run_asio_bridge;
pub(crate) use output::run_asio_output_bridge;

pub(crate) static ASIO_INPUT_CALLBACKS: azo::sys::Callbacks = azo::sys::Callbacks {
    buffer_switch: asio_buffer_switch,
    sample_rate_did_change: asio_sample_rate_did_change,
    asio_message,
    buffer_switch_time_info: asio_buffer_switch_time_info,
};

pub(crate) static ASIO_OUTPUT_CALLBACKS: azo::sys::Callbacks = azo::sys::Callbacks {
    buffer_switch: asio_output_buffer_switch,
    sample_rate_did_change: asio_sample_rate_did_change,
    asio_message,
    buffer_switch_time_info: asio_output_buffer_switch_time_info,
};

pub(crate) unsafe extern "system" fn asio_buffer_switch(
    buffer_index: std::ffi::c_long,
    _direct: azo::sys::Bool,
) {
    callback::dispatch_input(buffer_index);
}

pub(crate) unsafe extern "system" fn asio_output_buffer_switch(
    buffer_index: std::ffi::c_long,
    _direct: azo::sys::Bool,
) {
    callback::dispatch_output(buffer_index);
}

pub(crate) unsafe extern "system" fn asio_sample_rate_did_change(_rate: azo::sys::SampleRate) {
    // Not treated as a reset: some drivers send this during their own start
    // or for an unchanged external clock.  A driver whose rate really changed
    // from its panel sends `kAsioResetRequest`, handled below.
}

pub(crate) unsafe extern "system" fn asio_message(
    selector: azo::sys::MessageSelector,
    value: std::ffi::c_long,
    _message: *const std::ffi::c_void,
    _opt: *const f64,
) -> std::ffi::c_long {
    use azo::sys::{Bool, MessageSelector};

    // Drivers send selector-introspection first.  Claiming ASIO 2 support and
    // time-info support keeps native interfaces on the modern callback instead
    // of falling back to an unsupported path.
    if selector == MessageSelector::SELECTOR_SUPPORTED {
        let requested = MessageSelector::from(value);
        let supported = requested == MessageSelector::ENGINE_VERSION
            || requested == MessageSelector::RESET_REQUEST
            || requested == MessageSelector::SUPPORTS_TIME_INFO
            || requested == MessageSelector::SUPPORTS_TIME_CODE;
        return if supported {
            Bool::TRUE.0
        } else {
            Bool::FALSE.0
        };
    }
    if selector == MessageSelector::ENGINE_VERSION {
        return 2;
    }
    if selector == MessageSelector::SUPPORTS_TIME_INFO
        || selector == MessageSelector::SUPPORTS_TIME_CODE
    {
        return Bool::TRUE.0;
    }
    // A reset (new rate or buffer size from the driver's panel) needs the
    // whole bridge torn down.  The bridge thread sees the request and ends
    // the session with an explanation; the callback cannot do that itself.
    if selector == MessageSelector::RESET_REQUEST {
        callback::note_driver_reset();
        return Bool::TRUE.0;
    }
    Bool::FALSE.0
}

pub(crate) unsafe extern "system" fn asio_buffer_switch_time_info(
    params: *mut azo::sys::Time,
    double_buffer_index: std::ffi::c_long,
    _direct: azo::sys::Bool,
) -> *mut azo::sys::Time {
    // ASIO 2 drivers deliver audio here instead of `buffer_switch`.  Ignoring
    // this callback made real interfaces look like they had stopped.
    callback::dispatch_input(double_buffer_index);
    params
}

pub(crate) unsafe extern "system" fn asio_output_buffer_switch_time_info(
    params: *mut azo::sys::Time,
    double_buffer_index: std::ffi::c_long,
    _direct: azo::sys::Bool,
) -> *mut azo::sys::Time {
    callback::dispatch_output(double_buffer_index);
    params
}
