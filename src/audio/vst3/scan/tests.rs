use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{
    extra_roots, find_bundles, has_loadable_binary, parse_module_info, read_module_info,
    strip_json5,
};
use crate::audio::vst3::module::{ARCH_FOLDERS, BINARY_EXTENSION};

/// A temp folder of fake bundles, removed on drop.  Each bundle holds an
/// empty binary where this build looks for one; nothing loads it.  Also
/// used by the app's scan tests.
pub(crate) struct TestBundles {
    pub(crate) root: PathBuf,
}

impl TestBundles {
    pub(crate) fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "rigwerk-vst3-scan-{}-{name}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");
        Self { root }
    }

    /// A file at `relative`, with its folders.
    pub(crate) fn file(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("folders");
        std::fs::write(&path, contents).expect("file");
        path
    }

    /// A bundle at `relative` (ending in `.vst3`) with a binary for this
    /// build and, when given, `Contents/Resources/moduleinfo.json`.
    pub(crate) fn bundle(&self, relative: &str, module_info: Option<&str>) -> PathBuf {
        let bundle = self.root.join(relative);
        let stem = bundle.file_stem().expect("a name").to_string_lossy();
        self.file(
            &format!(
                "{relative}/Contents/{}/{stem}.{BINARY_EXTENSION}",
                ARCH_FOLDERS[0]
            ),
            "",
        );
        if let Some(text) = module_info {
            self.file(
                &format!("{relative}/Contents/Resources/moduleinfo.json"),
                text,
            );
        }
        bundle
    }
}

impl Drop for TestBundles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// What `moduleinfotool` writes (ADelay's, trimmed), plus an instrument
/// and a comment.
const MODULE_INFO: &str = r#"{
  "Name": "adelay",
  "Version": "3.8.1.0",
  "Factory Info": {
    "Vendor": "Steinberg Media Technologies",
    "URL": "http://www.steinberg.net",
    "Flags": {
      "Unicode": true,
      "Classes Discardable": false,
    },
  },
  // Classes in factory order.
  "Classes": [
    {
      "CID": "0cdbb66985d548a9bfd8371909d24bb3",
      "Category": "Audio Module Class",
      "Name": "ADelay",
      "Vendor": "",
      "Sub Categories": [
        "Fx",
        "Delay",
      ],
      "Class Flags": 1,
      "Snapshots": [
      ],
    },
    {
      "CID": "038E7FA9629A4EAA8541B88918E8952C",
      "Category": "Component Controller Class",
      "Name": "ADelayController",
      "Vendor": "Steinberg Media Technologies",
    },
    /* A synth in the same module. */
    {
      "CID": "23FC190E02DD4499A8D2230E50617DA3",
      "Category": "Audio Module Class",
      "Name": "Synth",
      "Vendor": "Someone, else",
      "Sub Categories": ["Instrument", "Synth",],
    },
    {
      "CID": "not a class id",
      "Category": "Audio Module Class",
      "Name": "Broken",
    },
  ],
}"#;

#[test]
fn module_info_with_trailing_commas_and_comments_lists_every_valid_class() {
    let info = parse_module_info(MODULE_INFO).expect("parses");
    assert_eq!(info.factory.vendor, "Steinberg Media Technologies");
    let classes: Vec<_> = info
        .classes
        .iter()
        .map(|class| {
            (
                class.class_id.as_str(),
                class.category.as_str(),
                class.name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        classes,
        [
            (
                "0CDBB66985D548A9BFD8371909D24BB3",
                "Audio Module Class",
                "ADelay"
            ),
            (
                "038E7FA9629A4EAA8541B88918E8952C",
                "Component Controller Class",
                "ADelayController"
            ),
            (
                "23FC190E02DD4499A8D2230E50617DA3",
                "Audio Module Class",
                "Synth"
            ),
        ],
        "ids upper case, the broken one dropped"
    );
    assert_eq!(info.classes[0].sub_categories, ["Fx", "Delay"]);
    assert_eq!(info.classes[0].vendor, "");
    assert!(info.classes[1].sub_categories.is_empty());
    assert_eq!(info.classes[2].sub_categories, ["Instrument", "Synth"]);
    assert_eq!(info.classes[2].vendor, "Someone, else");
}

#[test]
fn module_info_tolerates_a_byte_order_mark_and_missing_sections() {
    let info = parse_module_info("\u{feff}{ \"Name\": \"empty\", }").expect("parses");
    assert!(info.factory.vendor.is_empty());
    assert!(info.classes.is_empty());
    assert!(parse_module_info("{ \"Classes\": [ { \"CID\": 5 } ] }").is_err());
    assert!(parse_module_info("not json").is_err());
}

#[test]
fn json5_stripping_leaves_strings_alone() {
    let text = r#"{ "url": "http://a//b/*c*/", "quote": "a \" ,]", "list": [1, 2, /* x */ ], // end
}"#;
    let json = strip_json5(text).expect("strips");
    let value: serde_json::Value = serde_json::from_str(&json).expect(&json);
    assert_eq!(value["url"], "http://a//b/*c*/");
    assert_eq!(value["quote"], "a \" ,]");
    assert_eq!(value["list"], serde_json::json!([1, 2]));
    assert!(strip_json5("{ /* open").is_err());
    assert!(strip_json5("{ \"open").is_err());
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn module_info_is_read_from_either_location() {
    let dir = TestBundles::new("info");
    let current = dir.bundle("Current.vst3", Some(MODULE_INFO));
    let info = read_module_info(&current).expect("reads").expect("present");
    assert_eq!(info.classes.len(), 3);
    // From a file inside the bundle, as an added path may name the binary.
    let binary = current
        .join("Contents")
        .join(ARCH_FOLDERS[0])
        .join(format!("Current.{BINARY_EXTENSION}"));
    assert!(read_module_info(&binary).expect("reads").is_some());

    let legacy = dir.bundle("Legacy.vst3", None);
    dir.file("Legacy.vst3/Contents/moduleinfo.json", MODULE_INFO);
    assert!(read_module_info(&legacy).expect("reads").is_some());

    let none = dir.bundle("None.vst3", None);
    assert!(read_module_info(&none).expect("no error").is_none());

    let broken = dir.bundle("Broken.vst3", Some("{ \"Classes\": ["));
    assert!(read_module_info(&broken).is_err());
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn a_bundle_is_listed_only_with_a_binary_for_this_arch() {
    let dir = TestBundles::new("arch");
    let own = dir.bundle("Own.vst3", None);
    assert!(has_loadable_binary(&own));
    // A bundle shipping only another architecture's binary.
    dir.file(
        &format!("Other.vst3/Contents/other-arch/Other.{BINARY_EXTENSION}"),
        "",
    );
    assert!(!has_loadable_binary(&dir.root.join("Other.vst3")));
    // A binary named unlike the bundle still counts.
    dir.file(
        &format!(
            "Renamed.vst3/Contents/{}/plugin.{BINARY_EXTENSION}",
            ARCH_FOLDERS[0]
        ),
        "",
    );
    assert!(has_loadable_binary(&dir.root.join("Renamed.vst3")));
    // The Windows single-file layout loads as given.
    let single = dir.file("Single.vst3", "");
    assert!(has_loadable_binary(&single));
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn the_walk_finds_nested_bundles_but_does_not_search_inside_one() {
    let dir = TestBundles::new("walk");
    let top = dir.bundle("Top.vst3", None);
    let nested = dir.bundle("Vendor/Effects/Nested.VST3", None);
    // A `.vst3` inside a bundle (a Windows binary, a bundled extra) is the
    // bundle's own business.
    dir.file("Top.vst3/Contents/Resources/Inner.vst3", "");
    dir.file("Vendor/readme.txt", "");
    let other_root = TestBundles::new("walk-root");
    let elsewhere = other_root.bundle("Elsewhere.vst3", None);

    let found = find_bundles(&[
        dir.root.clone(),
        other_root.root.clone(),
        // A root that is a bundle, and one that does not exist.
        elsewhere.clone(),
        dir.root.join("missing"),
    ]);
    let mut expected = vec![top, nested, elsewhere];
    expected.sort();
    assert_eq!(found, expected);
}

#[cfg(unix)]
#[test]
fn the_walk_ends_on_a_symlink_loop() {
    let dir = TestBundles::new("loop");
    let bundle = dir.bundle("Real.vst3", None);
    std::fs::create_dir_all(dir.root.join("sub")).expect("folder");
    std::os::unix::fs::symlink(&dir.root, dir.root.join("sub/loop")).expect("symlink");
    assert_eq!(find_bundles(std::slice::from_ref(&dir.root)), [bundle]);
}

#[test]
fn extra_roots_split_on_semicolons() {
    assert_eq!(
        extra_roots(" /a/vst3 ;; /b ; "),
        [Path::new("/a/vst3"), Path::new("/b")]
    );
    assert!(extra_roots("").is_empty());
}
