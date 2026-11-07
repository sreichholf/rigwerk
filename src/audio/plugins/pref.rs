//! Plugin preferences, opaque state, and UI views.

/// Which plugin format an insert uses.  Saved inserts written before CLAP
/// support default to VST3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginFormat {
    #[default]
    Vst3,
    Clap,
}

impl PluginFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Vst3 => "VST3",
            Self::Clap => "CLAP",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Vst3 => "V3",
            Self::Clap => "CL",
        }
    }
}

/// A parameter value the person changed, kept with the insert so the plugin
/// opens with the same settings after a restart or a sample-rate change.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginParamPref {
    pub id: u32,
    pub value: f64,
}

/// The layout of [`PluginState::data`] this build writes and reads: the bytes
/// the format's own state call produced (CLAP `clap_plugin_state.save`, or
/// `vst3-host`'s `Plugin::save_state` envelope), base64-encoded.  A blob with
/// any other version is ignored and the insert falls back to its parameters.
pub(crate) const PLUGIN_STATE_VERSION: u32 = 1;

/// A plugin's opaque state, captured on the UI thread whenever preferences
/// are saved.  It covers what parameters cannot (loaded samples, internal
/// modes, a plugin's own preset), so it is restored before the parameters.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PluginState {
    pub version: u32,
    /// Base64 of the plugin's state bytes.
    pub data: String,
    /// [`PluginPref::params`] as it was when the state was captured.  A
    /// parameter changed after the capture differs from this snapshot, and is
    /// applied on top of the restored state so the newer value wins.
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

    /// The state bytes, or `None` when this build cannot use the blob.
    pub(crate) fn bytes(&self) -> Option<Vec<u8>> {
        if self.version != PLUGIN_STATE_VERSION {
            return None;
        }
        base64_decode(&self.data).filter(|bytes| !bytes.is_empty())
    }
}

/// Reads a saved state blob, treating a damaged one as absent: losing the
/// blob falls back to the parameter list, while failing here would drop the
/// whole insert from the preferences.
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

/// One effect in the insert chain, in the form that is saved to preferences.
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
    /// The controls the person adjusted, so the plugin reopens with the same
    /// settings after a restart or a sample-rate change.
    #[serde(default)]
    pub params: Vec<PluginParamPref>,
    /// The plugin's opaque state from the last save.  When it restores, it
    /// takes precedence over `params` (see [`super::params_to_restore`]).  Older
    /// preferences have none, and a damaged one is read as none.
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

/// One adjustable control of a loaded plugin, normalized to 0..1 for the UI.
#[derive(Clone, Debug, Default)]
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

/// What the UI shows for a slot; cheap to clone and independent of the plugin.
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
    pub parameters: Vec<PluginParamView>,
    /// Whether the plugin can draw its own graphical editor.
    pub has_editor: bool,
    /// Which loaded instance this view describes (see
    /// [`super::PluginHandle::instance_id`]); `None` while the slot is not loaded.
    /// An open editor checks it to notice that its insert was replaced.
    pub(crate) instance: Option<usize>,
    /// The latency in samples the insert adds (see
    /// `PluginChain::latency_samples`); 0 when it reports none, is not
    /// loaded, or failed.  Bypassed inserts keep theirs.
    pub latency: u32,
}

impl PluginView {
    /// `· 64 smp` for an insert row, or `None` when the insert adds no
    /// latency.
    pub fn latency_label(&self) -> Option<String> {
        (self.latency > 0).then(|| format!("· {} smp", self.latency))
    }
}
