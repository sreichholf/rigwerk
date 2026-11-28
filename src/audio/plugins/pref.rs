use std::sync::Arc;

/// Saved effects written before CLAP support default to VST3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginFormat {
    #[default]
    Vst3,
    Clap,
    Lv2,
}

impl PluginFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vst3 => "VST3",
            Self::Clap => "CLAP",
            Self::Lv2 => "LV2",
        }
    }

    /// The host has no LV2 UI, so an LV2 effect only shows its controls.
    pub(crate) fn has_editor_support(self) -> bool {
        !matches!(self, Self::Lv2)
    }
}

/// A value the person changed, so a restart or a sample-rate change
/// reopens the plugin with it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginParamPref {
    pub id: u32,
    pub value: f64,
}

/// Layout of [`PluginState::data`]: the format's own state bytes,
/// base64-encoded.  Any other version is ignored and the effect falls
/// back to its parameters.
pub(crate) const PLUGIN_STATE_VERSION: u32 = 1;

/// Opaque state: what parameters cannot cover (samples, internal modes,
/// the plugin's preset).  Restored before the parameters.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginState {
    pub version: u32,
    pub data: String,
    /// [`PluginPref::params`] at capture.  A value that differs, or was
    /// still queued (left out), is applied on top; one back at its default
    /// is listed here so restore resets it.
    #[serde(default)]
    pub params: Vec<PluginParamPref>,
}

impl PluginState {
    pub(crate) fn capture(bytes: &[u8], params: &[PluginParamPref]) -> Self {
        Self {
            version: PLUGIN_STATE_VERSION,
            data: base64_encode(bytes),
            params: params.to_vec(),
        }
    }

    pub(crate) fn bytes(&self) -> Option<Vec<u8>> {
        if self.version != PLUGIN_STATE_VERSION {
            return None;
        }
        base64_decode(&self.data).filter(|bytes| !bytes.is_empty())
    }
}

/// Reads a saved state blob, treating a damaged one as absent: losing the
/// blob falls back to the parameter list, while failing here would drop the
/// whole effect from the preferences.
fn lenient_state<'de, D>(deserializer: D) -> Result<Option<PluginState>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Good(Option<PluginState>),
        Bad(serde::de::IgnoredAny),
    }
    Ok(
        match <Stored as serde::Deserialize>::deserialize(deserializer)? {
            Stored::Good(state) => state,
            Stored::Bad(_) => None,
        },
    )
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(BASE64_ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim().as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    for (chunk_index, chunk) in text.chunks(4).enumerate() {
        let last = chunk_index + 1 == text.len() / 4;
        let padding = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - padding] {
            let value = BASE64_ALPHABET.iter().position(|&a| a == c)? as u32;
            n = (n << 6) | value;
        }
        n <<= 6 * padding as u32;
        let decoded = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&decoded[..3 - padding]);
    }
    Some(out)
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginPref {
    pub path: String,
    #[serde(default)]
    pub format: PluginFormat,
    #[serde(default)]
    pub class_id: Option<String>,
    /// Filled in from the file name when a saved entry lacks it.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub vendor: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub bypass: bool,
    #[serde(default = "default_mix")]
    pub mix: f32,
    #[serde(default)]
    pub params: Vec<PluginParamPref>,
    /// Opaque state from the last save.  On restore it takes precedence
    /// over `params` (see [`super::params_to_restore`]).  A damaged blob
    /// is read as none.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_state"
    )]
    pub state: Option<PluginState>,
}

fn default_mix() -> f32 {
    1.0
}

impl Default for PluginPref {
    fn default() -> Self {
        Self {
            path: String::new(),
            format: PluginFormat::Vst3,
            class_id: None,
            name: String::new(),
            vendor: String::new(),
            category: String::new(),
            bypass: false,
            mix: 1.0,
            params: Vec::new(),
            state: None,
        }
    }
}

/// One control, normalized to 0..1.
///
/// `PartialEq` compares values, not bits, so `-0.0` equals `0.0`.  A `NaN`
/// never equals itself, so an effect that reports one copies its list on
/// every republish (see [`PluginView::parameters`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PluginParamView {
    pub id: u32,
    pub name: String,
    pub value: f64,
    pub default: f64,
    /// 0 for a continuous control, otherwise the number of steps (so a toggle
    /// reports 1).
    pub step_count: i32,
    pub unit: String,
    /// What the plugin itself would show, e.g. "440.00 Hz" or "Sine".
    pub display: String,
    pub read_only: bool,
}

/// What the UI shows for a slot, with no plugin instance.  Cloning shares
/// the control list, so a republish copies controls only when they moved.
#[derive(Clone, Debug, Default)]
pub struct PluginView {
    pub path: String,
    pub format: PluginFormat,
    pub class_id: Option<String>,
    pub name: String,
    pub vendor: String,
    pub category: String,
    pub bypass: bool,
    pub mix: f32,
    pub error: Option<String>,
    /// Shared with every published view set it was in.
    pub parameters: Arc<[PluginParamView]>,
    pub has_editor: bool,
    /// [`super::PluginHandle::instance_id`].  `None` while the slot is not
    /// loaded.  An open editor checks it to notice a replaced effect.
    pub(crate) instance: Option<usize>,
    /// Samples of latency.  0 when it reports none, is not loaded, or
    /// failed.  A bypassed effect keeps its latency.
    pub latency: u32,
    /// Restart or reactivation pause: blocks play dry, delayed by its
    /// latency.  A failed effect is false and shows its error instead.
    pub paused: bool,
}

impl PluginView {
    /// A saved effect stays unopened until the bridge starts or something
    /// asks for its editor or controls.
    pub(crate) fn loaded(&self) -> bool {
        self.instance.is_some()
    }
}
