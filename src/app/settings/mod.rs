//! What survives a restart: preferences, saved patches and the choices
//! offered for rates and buffers.

mod preferences;
mod preset;
mod serde_util;

#[cfg(test)]
mod tests;

pub(crate) use preferences::{
    cushion_from_pref, cushion_to_pref, load_preferences, page_from_key, page_key,
    preferences_backup_key, rate_label, rematch_windows_virtual_out, resolved_wasapi_output_mode,
    sanitize_preset, store_preferences, FollowerPref, Preferences, BUFFERS, CUSHION_MS_MAX,
    CUSHION_MS_MIN, INPUT_GAIN_MAX_DB, INPUT_GAIN_MIN_DB, OUTPUT_GAIN_MAX_DB, OUTPUT_GAIN_MIN_DB,
    PREFERENCES_KEY, PREFERENCES_SCHEMA, RATES,
};
pub(crate) use preset::{unique_presets, Preset};

#[cfg(test)]
pub(crate) use preferences::{
    validate_bridge_settings, BridgeSettings, OutputBufferZero, DEFAULT_BUFFER, DEFAULT_CUSHION_MS,
    DEFAULT_INPUT_GAIN, DEFAULT_OUTPUT_GAIN, DEFAULT_SAMPLE_RATE, MAX_SANE_BUFFER,
    MIN_OUTPUT_BUFFER,
};
