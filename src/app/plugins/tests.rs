use super::{
    candidate_from_clap, candidates_from_bundle, category_tag, declared_channels, position_phrase,
    sanitize_effect_ids, visible_effects, EffectFilter, EffectId, PluginCandidate,
};
use crate::audio::vst3::TestBundles;
use crate::audio::PluginFormat;
use std::path::PathBuf;

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
fn an_effect_that_cannot_open_an_editor_opens_its_parameters() {
    use crate::app::paths::EffectKey;
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
    assert_eq!(app.plugin_editor, Some(EffectKey { path: 0, effect: 1 }));
    assert!(app.plugin_gui.is_none());

    // The same effect keeps its filter; another effect starts clean.
    app.plugin_param_filter = "gain".to_owned();
    app.open_plugin_parameters(1);
    assert_eq!(app.plugin_param_filter, "gain");
    app.open_plugin_parameters(0);
    assert_eq!(app.plugin_editor, Some(EffectKey { path: 0, effect: 0 }));
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

fn listed(name: &str, vendor: &str, category: &str) -> PluginCandidate {
    PluginCandidate {
        path: PathBuf::from(format!("{name}.vst3")),
        format: PluginFormat::Vst3,
        class_id: None,
        name: name.to_owned(),
        vendor: vendor.to_owned(),
        category: category.to_owned(),
        channels: Some(2),
        scan_error: None,
    }
}

fn id_of(candidate: &PluginCandidate) -> EffectId {
    EffectId::from_candidate(candidate)
}

#[test]
fn search_ranks_a_prefix_then_a_favourite_then_a_recent_effect() {
    let rover = listed("Rover", "", "Delay");
    let bedroom = listed("Bedroom", "", "Reverb");
    let chorus = listed("Chorus", "Roland", "Modulation");
    let amp = listed("Amp", "", "Amp");
    let library = vec![amp.clone(), chorus.clone(), bedroom.clone(), rover.clone()];
    let favourites = vec![id_of(&bedroom)];
    let recent = vec![id_of(&chorus)];
    let names: Vec<_> = visible_effects(
        &library,
        "ro",
        &EffectFilter::All,
        true,
        true,
        &favourites,
        &recent,
    )
    .into_iter()
    .map(|candidate| candidate.name.as_str())
    .collect();
    // "ro" is a prefix of Rover, inside Bedroom, and inside the maker Roland.
    // Amp does not match.  Bedroom is a favourite, so it comes before Chorus.
    assert_eq!(names, ["Rover", "Bedroom", "Chorus"]);
}

#[test]
fn the_recent_list_keeps_the_latest_ten_and_drops_duplicates() {
    let mut recent: Vec<EffectId> = (0..12)
        .map(|index| EffectId {
            path: format!("e{index}.vst3"),
            format: PluginFormat::Vst3,
            class_id: None,
            name: format!("Effect {index}"),
        })
        .collect();
    recent.push(recent[3].clone());
    sanitize_effect_ids(&mut recent, true);
    assert_eq!(recent.len(), 10);
    assert_eq!(recent.last().unwrap().name, "Effect 3");
    assert!(recent
        .iter()
        .all(|id| id.name != "Effect 0" && id.name != "Effect 1"));
}

#[test]
fn favourites_keep_the_first_of_each_effect() {
    let room = listed("Room", "", "Reverb");
    let mut favourites = vec![id_of(&room), id_of(&room)];
    favourites[1].name = "room".to_owned();
    favourites.push(EffectId {
        path: "  ".to_owned(),
        format: PluginFormat::Clap,
        class_id: None,
        name: " ".to_owned(),
    });
    sanitize_effect_ids(&mut favourites, false);
    assert_eq!(favourites.len(), 1);
    assert_eq!(favourites[0].name, "Room");
}

#[test]
fn recent_use_does_not_reorder_the_library() {
    let alpha = listed("Alpha", "", "Delay");
    let beta = listed("Beta", "", "Delay");
    let library = vec![beta.clone(), alpha.clone()];
    let recent = vec![id_of(&beta)];
    let names = |filter| {
        visible_effects(&library, "", filter, true, true, &[], &recent)
            .into_iter()
            .map(|candidate| candidate.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&EffectFilter::All), ["Alpha", "Beta"]);
    assert_eq!(names(&EffectFilter::Recent), ["Beta"]);
}

#[test]
fn the_format_filter_hides_the_other_format() {
    let vst = listed("Room", "", "Reverb");
    let mut clap = listed("Amp", "", "Amp");
    clap.format = PluginFormat::Clap;
    clap.path = PathBuf::from("Amp.clap");
    let library = vec![vst, clap];
    let names = |vst3, clap_on| {
        visible_effects(&library, "", &EffectFilter::All, vst3, clap_on, &[], &[])
            .into_iter()
            .map(|candidate| candidate.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(true, false), ["Room"]);
    assert_eq!(names(false, true), ["Amp"]);
    assert!(names(false, false).is_empty());
}

#[test]
fn the_browser_adds_at_the_position_and_stays_open() {
    let _main = crate::audio::claim_main_thread();
    let _loader = crate::audio::set_test_loader(|_pref, _rate| {
        Ok((
            crate::audio::clap::tests::fake_plugin_with_latency_for_test(0),
            false,
        ))
    });
    let mut app = crate::app::blank_app();
    app.engine.restore_plugins(
        0,
        vec![
            crate::audio::PluginPref {
                name: "A".to_owned(),
                path: "a.vst3".to_owned(),
                ..Default::default()
            },
            crate::audio::PluginPref {
                name: "C".to_owned(),
                path: "c.vst3".to_owned(),
                ..Default::default()
            },
        ],
    );
    let mut added = listed("B", "Rig", "Delay");
    added.path = PathBuf::from(format!("{}b", crate::audio::TEST_PATH));
    app.open_effect_browser(0, 1);
    assert!(matches!(app.page, crate::app::Page::Live));
    assert!(app.plugin_browser.open);
    assert!(app.add_listed_effect(&added, false));
    let names: Vec<_> = app
        .engine
        .plugin_views(0)
        .iter()
        .map(|view| view.name.clone())
        .collect();
    assert_eq!(names, ["A", "B", "C"]);
    assert!(app.plugin_browser.open);
    assert_eq!(app.plugin_browser.position, 2);
    assert_eq!(app.plugin_browser.recent.len(), 1);
    let notice = &app.plugin_browser.notice.as_ref().expect("notice").0;
    assert!(notice.contains("Added B"), "{notice}");
}

#[test]
fn a_missing_path_does_not_move_the_browser() {
    let mut app = crate::app::blank_app();
    app.open_effect_browser(0, 0);
    app.plugin_browser.path = 9;
    assert!(!app.add_listed_effect(&listed("Room", "", "Reverb"), false));
    assert_eq!(app.plugin_browser.position, 0);
    assert!(app.plugin_browser.recent.is_empty());
    let notice = &app.plugin_browser.notice.as_ref().expect("notice").0;
    assert!(notice.contains("not available"), "{notice}");
}

#[test]
fn a_missing_file_reports_that_the_path_does_not_exist() {
    let mut app = crate::app::blank_app();
    app.plugin_browser.path_input = "/no/such/effect.vst3".to_owned();
    app.add_plugin_from_path();
    let notice = &app.plugin_browser.notice.as_ref().expect("notice").0;
    assert!(notice.contains("does not exist"), "{notice}");
}

#[test]
fn the_position_line_names_the_effect_before_the_gap() {
    let names = vec!["Limiter Stereo".to_owned()];
    assert_eq!(position_phrase(&names, 0), "at the start");
    assert_eq!(position_phrase(&names, 1), "after Limiter Stereo");
    assert_eq!(position_phrase(&[], 0), "at the start");
    assert_eq!(
        position_phrase(&["Limiter Stereo".to_owned(), "Room".to_owned()], 1),
        "after Limiter Stereo"
    );
    assert_eq!(category_tag("Delay · Stereo"), "Delay");
}

fn three_effects() -> crate::app::RigWerkApp {
    let app = crate::app::blank_app();
    app.engine.restore_plugins(
        0,
        ["A", "B", "C"]
            .into_iter()
            .map(|name| crate::audio::PluginPref {
                name: name.to_owned(),
                path: format!("{name}.vst3"),
                ..Default::default()
            })
            .collect(),
    );
    app
}

fn names(app: &crate::app::RigWerkApp) -> Vec<String> {
    app.engine
        .plugin_views(0)
        .iter()
        .map(|view| view.name.clone())
        .collect()
}

#[test]
fn a_dragged_effect_lands_where_it_is_dropped_and_its_sheet_follows() {
    use crate::app::screen::Sheet;
    let mut app = three_effects();
    app.open_sheet(Sheet::Effect { path: 0, index: 0 });
    app.move_effect(0, 0, 2);
    assert_eq!(names(&app), ["B", "C", "A"]);
    assert_eq!(app.chrome.sheet, Some(Sheet::Effect { path: 0, index: 2 }));
    app.open_sheet(Sheet::Effect { path: 0, index: 1 });
    app.move_effect(0, 2, -2);
    assert_eq!(names(&app), ["A", "B", "C"]);
    assert_eq!(
        app.chrome.sheet,
        Some(Sheet::Effect { path: 0, index: 2 }),
        "the open effect slid down"
    );
    app.move_effect(0, 0, -1);
    assert_eq!(names(&app), ["A", "B", "C"], "nothing above the first");
}

#[test]
fn removing_an_effect_closes_its_sheet_and_moves_a_later_one() {
    use crate::app::screen::Sheet;
    let mut app = three_effects();
    app.open_sheet(Sheet::Effect { path: 0, index: 2 });
    app.remove_effect(0, 0);
    assert_eq!(names(&app), ["B", "C"]);
    assert_eq!(app.chrome.sheet, Some(Sheet::Effect { path: 0, index: 1 }));
    app.remove_effect(0, 1);
    assert_eq!(app.chrome.sheet, None);
}

#[test]
fn a_double_click_opens_the_controls_when_the_preference_says_so() {
    use crate::app::screen::Sheet;
    let mut app = three_effects();
    app.effect_double_click = crate::app::settings::EffectOpen::Controls;
    app.open_effect(0, 1);
    assert_eq!(app.chrome.sheet, Some(Sheet::Effect { path: 0, index: 1 }));
}

#[test]
fn the_browser_opens_in_the_drawer() {
    use crate::app::screen::Drawer;
    let mut app = three_effects();
    app.open_effect_browser(0, 3);
    assert_eq!(app.chrome.drawer, Some(Drawer::Browser));
    assert!(app.plugin_browser.open);
    app.close_drawer();
    assert!(!app.plugin_browser.open);
}
