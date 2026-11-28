//! `controlPanel` runs on the bridge thread that created the driver,
//! between callbacks.

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
    // Not a reset: some drivers send this during start or for an unchanged
    // external clock.  A real rate change arrives as `kAsioResetRequest`.
}

pub(crate) unsafe extern "system" fn asio_message(
    selector: azo::sys::MessageSelector,
    value: std::ffi::c_long,
    _message: *const std::ffi::c_void,
    _opt: *const f64,
) -> std::ffi::c_long {
    use azo::sys::{Bool, MessageSelector};

    // Drivers probe selectors first.  Claim ASIO 2 and time-info so native
    // interfaces stay on this callback instead of an unsupported path.
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
    // A panel rate or buffer change needs the whole bridge torn down.  The
    // callback only records it; the bridge thread ends the run.
    if selector == MessageSelector::RESET_REQUEST {
        callback::ASIO_HOST.request_reset();
        return Bool::TRUE.0;
    }
    Bool::FALSE.0
}

pub(crate) unsafe extern "system" fn asio_buffer_switch_time_info(
    params: *mut azo::sys::Time,
    double_buffer_index: std::ffi::c_long,
    _direct: azo::sys::Bool,
) -> *mut azo::sys::Time {
    // ASIO 2 drivers deliver audio here instead of `buffer_switch`.
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
