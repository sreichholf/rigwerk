use super::{candidate_from_clap, candidates_from_bundle, declared_channels};
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

#[test]
fn an_insert_that_cannot_open_an_editor_opens_its_parameters() {
    use crate::app::paths::InsertKey;
    use crate::audio::PluginPref;
    let missing = |path: &str| PluginPref {
        path: path.to_owned(),
        name: path.to_owned(),
        ..PluginPref::default()
    };
    let mut app = crate::app::blank_app();
    app.engine
        .restore_plugins(0, vec![missing("A.vst3"), missing("B.vst3")]);

    // Neither plugin loads, so neither has an editor to open.
    app.open_plugin_editor(1);
    assert_eq!(app.plugin_editor, Some(InsertKey { path: 0, insert: 1 }));
    assert!(app.plugin_gui.is_none());

    // The same insert keeps its filter; another insert starts clean.
    app.plugin_param_filter = "gain".to_owned();
    app.open_plugin_parameters(1);
    assert_eq!(app.plugin_param_filter, "gain");
    app.open_plugin_parameters(0);
    assert_eq!(app.plugin_editor, Some(InsertKey { path: 0, insert: 0 }));
    assert!(app.plugin_param_filter.is_empty());
}

#[test]
fn mono_and_stereo_tags_declare_the_channel_count() {
    let tags = |list: &[&str]| list.iter().map(|tag| (*tag).to_owned()).collect::<Vec<_>>();
    assert_eq!(declared_channels(&tags(&["Fx", "Mono"])), Some(1));
    assert_eq!(
        declared_channels(&tags(&["audio-effect", "stereo"])),
        Some(2)
    );
    assert_eq!(declared_channels(&tags(&["Fx", "STEREO"])), Some(2));
    assert_eq!(declared_channels(&tags(&["Fx", "Delay"])), None);
    assert_eq!(declared_channels(&tags(&["surround"])), None);
    assert_eq!(declared_channels(&tags(&["mono", "stereo"])), None, "both");
    assert_eq!(declared_channels(&[]), None);
}

#[test]
fn a_clap_candidate_reads_its_channels_from_its_features() {
    let candidate = |features: &[&str]| crate::audio::clap::ClapCandidate {
        path: "amp.clap".into(),
        plugin_id: "rig.amp".to_owned(),
        name: "Amp".to_owned(),
        vendor: "Rig".to_owned(),
        category: features.join(" · "),
        features: features
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect(),
    };
    let mono = candidate_from_clap(candidate(&["audio-effect", "mono"]));
    assert_eq!(mono.channels, Some(1));
    assert_eq!(mono.scan_error, None);
    assert_eq!(mono.category, "audio-effect · mono");
    let open = candidate_from_clap(candidate(&["audio-effect", "distortion"]));
    assert_eq!(open.channels, None);
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn a_vst3_class_reads_its_channels_from_its_sub_categories() {
    let dir = TestBundles::new("channels");
    let bundle = dir.bundle(
        "Pair.vst3",
        Some(
            r#"{
  "Classes": [
    {
      "CID": "0CDBB66985D548A9BFD8371909D24BB3",
      "Category": "Audio Module Class",
      "Name": "Mono Delay",
      "Sub Categories": ["Fx", "Delay", "Mono",],
    },
    {
      "CID": "84E8DE5F92554F5396FAE4133C935A18",
      "Category": "Audio Module Class",
      "Name": "Stereo Delay",
      "Sub Categories": ["Fx", "Delay", "Stereo",],
    },
  ],
}"#,
        ),
    );
    let found = candidates_from_bundle(&bundle);
    let rows: Vec<_> = found
        .iter()
        .map(|candidate| (candidate.name.as_str(), candidate.channels))
        .collect();
    assert_eq!(rows, [("Mono Delay", Some(1)), ("Stereo Delay", Some(2))]);
    assert!(found.iter().all(|candidate| candidate.scan_error.is_none()));
    // The module above says neither.
    let other = dir.bundle("Other.vst3", Some(MODULE_INFO));
    assert!(candidates_from_bundle(&other)
        .iter()
        .all(|candidate| candidate.channels.is_none()));
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn an_unreadable_module_info_keeps_the_row_with_its_scan_error() {
    let dir = TestBundles::new("broken-info");
    let bundle = dir.bundle("Broken.vst3", Some("{ this is not json"));
    let found = candidates_from_bundle(&bundle);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].class_id, None);
    assert_eq!(found[0].name, "Broken");
    assert_eq!(found[0].channels, None);
    let error = found[0].scan_error.as_deref().expect("a scan error");
    assert!(error.contains("moduleinfo.json"), "{error}");
    // A bundle with no module info at all is not an error.
    let old = dir.bundle("Old.vst3", None);
    assert_eq!(candidates_from_bundle(&old)[0].scan_error, None);
}
