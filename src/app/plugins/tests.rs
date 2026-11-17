use super::candidates_from_bundle;
use crate::audio::vst3::TestBundles;
use crate::audio::PluginFormat;

/// A module with an effect, its controller, an instrument, and an effect
/// with neither name nor vendor.
const MODULE_INFO: &str = r#"{
  "Factory Info": { "Vendor": "Factory Vendor", },
  "Classes": [
    {
      "CID": "0CDBB66985D548A9BFD8371909D24BB3",
      "Category": "Audio Module Class",
      "Name": "Delay",
      "Vendor": "Class Vendor",
      "Sub Categories": ["Fx", "Delay",],
    },
    {
      "CID": "038E7FA9629A4EAA8541B88918E8952C",
      "Category": "Component Controller Class",
      "Name": "DelayController",
    },
    {
      "CID": "23FC190E02DD4499A8D2230E50617DA3",
      "Category": "Audio Module Class",
      "Name": "Synth",
      "Sub Categories": ["Instrument", "Synth",],
    },
    {
      "CID": "84e8de5f92554f5396fae4133c935a18",
      "Category": "Audio Module Class",
      "Name": " ",
    },
  ],
}"#;

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn a_bundle_lists_its_effect_classes_from_module_info() {
    let dir = TestBundles::new("candidates");
    let bundle = dir.bundle("Pair.vst3", Some(MODULE_INFO));
    let found = candidates_from_bundle(&bundle);
    let rows: Vec<_> = found
        .iter()
        .map(|candidate| {
            (
                candidate.class_id.as_deref(),
                candidate.name.as_str(),
                candidate.vendor.as_str(),
                candidate.category.as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                Some("0CDBB66985D548A9BFD8371909D24BB3"),
                "Delay",
                "Class Vendor",
                "Fx · Delay"
            ),
            (
                Some("84E8DE5F92554F5396FAE4133C935A18"),
                "Pair",
                "Factory Vendor",
                "VST3 effect"
            ),
        ],
        "no controller, no instrument; names and vendors fall back"
    );
    assert!(found
        .iter()
        .all(|candidate| candidate.path == bundle && candidate.format == PluginFormat::Vst3));
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn a_bundle_without_module_info_is_one_row_resolved_at_load() {
    let dir = TestBundles::new("fallback");
    let bundle = dir.bundle("Old Plugin.vst3", None);
    let found = candidates_from_bundle(&bundle);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].class_id, None);
    assert_eq!(found[0].name, "Old Plugin");
    assert_eq!(found[0].category, "VST3 effect");

    // No binary for this build: no row at all.
    dir.file(
        "Foreign.vst3/Contents/Resources/moduleinfo.json",
        MODULE_INFO,
    );
    assert!(candidates_from_bundle(&dir.root.join("Foreign.vst3")).is_empty());
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn a_bundle_whose_audio_classes_are_all_instruments_is_not_listed() {
    let dir = TestBundles::new("instruments");
    let bundle = dir.bundle(
        "Synth.vst3",
        Some(
            r#"{
  "Classes": [
    {
      "CID": "23FC190E02DD4499A8D2230E50617DA3",
      "Category": "Audio Module Class",
      "Name": "Synth",
      "Sub Categories": ["Instrument", "Synth",],
    },
    {
      "CID": "038E7FA9629A4EAA8541B88918E8952C",
      "Category": "Component Controller Class",
      "Name": "SynthController",
    },
  ],
}"#,
        ),
    );
    assert!(
        candidates_from_bundle(&bundle).is_empty(),
        "the fallback row would load the instrument"
    );
}
