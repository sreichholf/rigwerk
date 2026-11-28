mod path_pref;
mod preferences;
mod serde_util;
mod session;

#[cfg(test)]
mod tests;

pub(crate) use path_pref::{default_path_name, PathPref};
pub(crate) use preferences::{
    bridge_buffer_from_pref, bridge_buffer_to_pref, load_preferences, page_from_key, page_key,
    rate_label, rematch_windows_virtual_output, resolved_wasapi_output_mode,
    sanitize_bridge_buffer_ms, sanitize_buffer_size, sanitize_output_rate, sanitize_sample_rate,
    sanitize_session, store_preferences, store_preferences_backup, EffectOpen, ExtraOutputPref,
    Preferences, BRIDGE_BUFFER_MS_MAX, BRIDGE_BUFFER_MS_MIN, BUFFERS, INPUT_GAIN_MAX_DB,
    INPUT_GAIN_MIN_DB, OUTPUT_GAIN_MAX_DB, OUTPUT_GAIN_MIN_DB, PREFERENCES_BACKUP_KEY,
    PREFERENCES_KEY, PREFERENCES_SCHEMA, RATES,
};
pub(crate) use session::{unique_sessions, Session};

#[cfg(test)]
pub(crate) use preferences::{
    validate_bridge_settings, BridgeSettings, OutputBufferZero, DEFAULT_BRIDGE_BUFFER_MS,
    DEFAULT_BUFFER, DEFAULT_INPUT_GAIN, DEFAULT_OUTPUT_GAIN, DEFAULT_SAMPLE_RATE, MAX_SANE_BUFFER,
    MIN_OUTPUT_BUFFER,
};
