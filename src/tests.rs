//! Source scanners.  `tests.rs` is not read: a test may name a banned word
//! on purpose.
use std::path::{Path, PathBuf};

/// The unambiguous words from the "Never in the UI" column.
const BANNED: &[&str] = &[
    "cushion",
    "follower",
    "followers",
    "patch",
    "patches",
    "insert",
    "inserts",
    "quantum",
    "underrun",
    "underruns",
    "xrun",
    "overrun",
    "REAL-style",
    "BYP",
    "publish",
    "published",
    "endpoint",
    "preview",
    "harmonizer",
    "splice",
    "lookahead",
];

/// The platform's own term (`quantum` in PipeWire, `endpoint` in WASAPI).
/// A literal here may use any banned word.
const PLATFORM: &[&str] = &["src/audio/linux/", "src/audio/win/"];

/// Stored preference values, not text a person reads.  Those literals stay.
const STORED_KEYS: &[&str] = &["src/app/latency.rs"];

/// A file and the banned words it may still use.  An entry for a word the
/// file no longer uses fails, so the list only shrinks.
const NOT_MIGRATED: &[(&str, &[&str])] = &[];

#[derive(Debug, PartialEq)]
struct Hit {
    file: String,
    line: usize,
    word: &'static str,
    literal: String,
}

fn sources(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read src")
        .map(|entry| entry.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if path.is_dir() {
            if name != "testdata" {
                sources(&path, root, out);
            }
            continue;
        }
        if !name.ends_with(".rs") || name == "tests.rs" || name.ends_with("_tests.rs") {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .expect("under the crate")
            .to_string_lossy()
            .replace('\\', "/");
        out.push((relative, path));
    }
}

/// String literals, with the line each starts on.  Comments, char literals
/// and lifetimes are skipped, and so is a `#[serde(...)]` literal: those
/// name stored keys and keep their old spelling.
fn literals(source: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut line = 1;
    let mut index = 0;
    let mut serde_depth: Option<usize> = None;
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    while index < chars.len() {
        let c = chars[index];
        let next = chars.get(index + 1).copied();
        match c {
            '\n' => {
                line += 1;
                index += 1;
            }
            '/' if next == Some('/') => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '/' if next == Some('*') => {
                let mut depth = 0;
                while index < chars.len() {
                    if chars[index] == '/' && chars.get(index + 1) == Some(&'*') {
                        depth += 1;
                        index += 2;
                    } else if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                        depth -= 1;
                        index += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if chars[index] == '\n' {
                            line += 1;
                        }
                        index += 1;
                    }
                }
            }
            '#' if next == Some('[') && serde_depth.is_none() => {
                let rest: String = chars[index + 2..].iter().take(6).collect();
                if rest.starts_with("serde") && !rest[5..].starts_with(ident) {
                    serde_depth = Some(0);
                }
                index += 1;
            }
            '[' => {
                if let Some(depth) = serde_depth.as_mut() {
                    *depth += 1;
                }
                index += 1;
            }
            ']' => {
                if let Some(depth) = serde_depth.as_mut() {
                    *depth -= 1;
                    if *depth == 0 {
                        serde_depth = None;
                    }
                }
                index += 1;
            }
            '\'' => {
                // `'\n'`, `'"'` and `'x'` are chars; `'a` is a lifetime.
                if next == Some('\\') {
                    index += 2;
                    while index < chars.len() && chars[index] != '\'' {
                        index += 1;
                    }
                    index += 1;
                } else if chars.get(index + 2) == Some(&'\'') {
                    index += 3;
                } else {
                    index += 1;
                }
            }
            'r' | 'b' | 'c' if index == 0 || !ident(chars[index - 1]) => {
                let mut at = index + 1;
                if c != 'r' && chars.get(at) == Some(&'r') {
                    at += 1;
                }
                let raw = c == 'r' || chars[at - 1] == 'r';
                let mut hashes = 0;
                while raw && chars.get(at + hashes) == Some(&'#') {
                    hashes += 1;
                }
                if chars.get(at + hashes) != Some(&'"') {
                    index += 1;
                    while index < chars.len() && ident(chars[index]) {
                        index += 1;
                    }
                    continue;
                }
                let start_line = line;
                let (text, end) = if raw {
                    read_raw(&chars, at + hashes + 1, hashes)
                } else {
                    read_quoted(&chars, at + hashes + 1)
                };
                line += text.matches('\n').count();
                if serde_depth.is_none() {
                    out.push((start_line, text));
                }
                index = end;
            }
            '"' => {
                let start_line = line;
                let (text, end) = read_quoted(&chars, index + 1);
                line += text.matches('\n').count();
                if serde_depth.is_none() {
                    out.push((start_line, text));
                }
                index = end;
            }
            c if ident(c) => {
                while index < chars.len() && ident(chars[index]) {
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    out
}

/// An escape is not decoded.  Only a real newline counts as a line.
fn read_quoted(chars: &[char], start: usize) -> (String, usize) {
    let mut text = String::new();
    let mut index = start;
    while index < chars.len() {
        match chars[index] {
            '\\' => {
                text.push(' ');
                if chars.get(index + 1) == Some(&'\n') {
                    text.push('\n');
                }
                index += 2;
            }
            '"' => return (text, index + 1),
            c => {
                text.push(c);
                index += 1;
            }
        }
    }
    (text, index)
}

fn read_raw(chars: &[char], start: usize, hashes: usize) -> (String, usize) {
    let mut index = start;
    while index < chars.len() {
        if chars[index] == '"' && (1..=hashes).all(|n| chars.get(index + n) == Some(&'#')) {
            return (chars[start..index].iter().collect(), index + 1 + hashes);
        }
        index += 1;
    }
    (chars[start..].iter().collect(), index)
}

/// Blanks `{name}` and `{name:>4}`: a placeholder names a variable, not a word.
fn readable(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    let mut rest = literal;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(escaped) = after.strip_prefix('{') {
            out.push('{');
            rest = escaped;
            continue;
        }
        let close = after.find('}');
        let placeholder = close.is_some_and(|close| {
            let inner = &after[..close];
            let name = inner.split(':').next().unwrap_or("");
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        });
        if placeholder {
            out.push(' ');
            rest = &after[close.expect("checked") + 1..];
        } else {
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// A hyphen does not continue a word, so `REAL-style` is one word.
fn has_word(text: &str, word: &str) -> bool {
    let text = text.to_lowercase();
    let word = word.to_lowercase();
    let continues = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    text.match_indices(&word).any(|(at, _)| {
        !continues(text[..at].chars().next_back())
            && !continues(text[at + word.len()..].chars().next())
    })
}

fn hits() -> Vec<Hit> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), root, &mut files);
    assert!(files.len() > 100, "found {} source files", files.len());
    let mut hits = Vec::new();
    for (file, path) in files {
        let source = std::fs::read_to_string(&path).expect("read source");
        for (line, literal) in literals(&source) {
            let text = readable(&literal);
            for word in BANNED {
                if has_word(&text, word) {
                    hits.push(Hit {
                        file: file.clone(),
                        line,
                        word,
                        literal: literal.clone(),
                    });
                }
            }
        }
    }
    hits
}

#[test]
fn no_literal_uses_a_word_the_glossary_bans() {
    let hits = hits();
    let not_allowed: Vec<String> = hits
        .iter()
        .filter(|hit| !PLATFORM.iter().any(|dir| hit.file.starts_with(dir)))
        .filter(|hit| !STORED_KEYS.contains(&hit.file.as_str()))
        .filter(|hit| {
            !NOT_MIGRATED
                .iter()
                .any(|(file, words)| *file == hit.file && words.contains(&hit.word))
        })
        .map(|hit| {
            format!(
                "{}:{}: \"{}\" in {:?}",
                hit.file, hit.line, hit.word, hit.literal
            )
        })
        .collect();
    assert!(
        not_allowed.is_empty(),
        "banned words (docs/ui-spec.md §2.1):\n{}",
        not_allowed.join("\n")
    );
}

#[test]
fn the_not_migrated_list_only_names_words_still_used() {
    let hits = hits();
    let mut stale = Vec::new();
    for (file, words) in NOT_MIGRATED {
        for word in *words {
            if !hits
                .iter()
                .any(|hit| hit.file == *file && hit.word == *word)
            {
                stale.push(format!("{file}: {word}"));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "remove these from NOT_MIGRATED:\n{}",
        stale.join("\n")
    );
}

#[test]
fn literals_skip_comments_chars_and_serde_keys() {
    let source = r##"
        // "follower" in a comment
        /* "patch" /* nested */ "insert" */
        let c = '"'; let q = '\''; fn f<'a>(x: &'a str) {}
        #[serde(rename = "cushion_mode", default)]
        let a = "one";
        let b = r#"two "quoted""#;
        let d = b"three";
    "##;
    let found: Vec<String> = literals(source).into_iter().map(|(_, s)| s).collect();
    assert_eq!(found, ["one", r#"two "quoted""#, "three"]);
}

#[test]
fn whole_words_only_and_placeholders_are_not_words() {
    assert!(has_word("Save as new Patch", "patch"));
    assert!(!has_word("dispatch the event", "patch"));
    assert!(!has_word("is_follower", "follower"));
    assert!(has_word("REAL-style · 144 smp", "REAL-style"));
    assert!(has_word("BYP", "BYP"));
    assert!(!has_word(&readable("{follower} is missing"), "follower"));
    assert!(has_word(&readable("{{follower}}"), "follower"));
    assert!(!has_word(&readable("level {insert:>5.1} dB"), "insert"));
}

/// Names a production file must not use.  `app` does name `egui` for the
/// theme and the zoom, so only `audio` lists it.
const LAYERING: &[(&str, &[&str])] = &[
    ("src/audio/", &["crate::app", "crate::ui", "egui", "eframe"]),
    (
        "src/audio.rs",
        &["crate::app", "crate::ui", "egui", "eframe"],
    ),
    ("src/app/", &["crate::ui"]),
    ("src/app.rs", &["crate::ui"]),
];

/// A `//` line is prose, so a doc link is not a reference.  A name inside a
/// longer path, a string or a block comment still counts: those are not
/// stripped.
fn boundary_lines<'a>(source: &str, names: &'a [&'a str]) -> Vec<(usize, &'a str)> {
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for &name in names {
            if has_word(line, name) {
                out.push((index + 1, name));
            }
        }
    }
    out
}

fn boundary_hits() -> Vec<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), root, &mut files);
    assert!(files.len() > 100, "found {} source files", files.len());
    let mut hits = Vec::new();
    for (file, path) in files {
        let Some(layer) = LAYERING.iter().find(|(dir, _)| file.starts_with(dir)) else {
            continue;
        };
        let source = std::fs::read_to_string(&path).expect("read source");
        for (line, name) in boundary_lines(&source, layer.1) {
            hits.push(format!("{file}:{line}: {name}"));
        }
    }
    hits
}

#[test]
fn a_comment_is_not_a_layer_reference() {
    let source = r##"
        //! [`PathPref`](crate::app::settings::PathPref) is a doc link.
        /// egui in another doc comment.
        use crate::app::settings::PathPref;
        let ctx = egui::Context::default();
    "##;
    assert_eq!(
        boundary_lines(source, &["crate::app", "crate::ui", "egui"]),
        [(4, "crate::app"), (5, "egui")]
    );
}

/// `super::super::app` from `src/audio/*.rs` holds none of these names, so
/// this check cannot see it.
#[test]
fn a_layer_does_not_name_the_one_above_it() {
    let hits = boundary_hits();
    assert!(
        hits.is_empty(),
        "a layer names the one above it (AGENTS.md, Separation):\n{}",
        hits.join("\n")
    );
}

/// `app/plugins.rs` has its own `scan_library`, so the two call names are
/// counted apart.  The in-process `clap::scan_library` calls a bundle's
/// `init` in this process.
#[test]
fn the_browser_scans_clap_in_a_child_process() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app/plugins.rs");
    let source = std::fs::read_to_string(&path).expect("read the plugin scan");
    let calls = boundary_lines(
        &source,
        &["clap::scan_library_isolated", "clap::scan_library"],
    );
    let in_process: Vec<String> = calls
        .iter()
        .filter(|(_, name)| *name == "clap::scan_library")
        .map(|(line, _)| format!("src/app/plugins.rs:{line}"))
        .collect();
    assert!(
        calls
            .iter()
            .any(|(_, name)| *name == "clap::scan_library_isolated"),
        "the browser no longer scans CLAP in the child process"
    );
    assert!(
        in_process.is_empty(),
        "the browser scans CLAP in this process (AGENTS.md, audio thread):\n{}",
        in_process.join("\n")
    );
}

type ClapScanCase<'a> = (&'a [&'a str], Option<(&'a str, u64)>);

#[test]
fn clap_scan_arguments_are_parsed_or_refused() {
    let cases: &[ClapScanCase] = &[
        (
            &["rigwerk", "--clap-scan", "/tmp/scan.ron", "1500"],
            Some(("/tmp/scan.ron", 1500)),
        ),
        (&["rigwerk", "--clap-scan"], None),
        (&["rigwerk", "--clap-scan", "/tmp/scan.ron"], None),
        (&["rigwerk", "--clap-scan", "/tmp/scan.ron", "-1"], None),
        (&["rigwerk", "--clap-scan", "/tmp/scan.ron", "soon"], None),
        (
            &["rigwerk", "--clap-scan", "/tmp/scan.ron", "900", "extra"],
            Some(("/tmp/scan.ron", 900)),
        ),
    ];
    for &(argv, expected) in cases {
        let (requested, scan) = crate::parse_clap_scan(argv.iter().map(std::ffi::OsString::from));
        assert!(requested, "{argv:?}");
        let seen = scan.map(|(path, limit)| (path.display().to_string(), limit.as_millis() as u64));
        let wanted = expected.map(|(path, limit)| (path.to_owned(), limit));
        assert_eq!(seen, wanted, "{argv:?}");
    }
    assert_eq!(
        crate::parse_clap_scan(
            ["--screenshot", "shot.png"]
                .iter()
                .map(std::ffi::OsString::from)
        ),
        (false, None)
    );
}

/// The exit-code table is pinned by `app::screenshot`'s
/// `every_ending_has_its_own_exit_code`.
#[test]
fn a_recorded_screenshot_ending_is_the_process_status() {
    use crate::app::screenshot::{EXIT_RUN_TIMED_OUT, NO_ENDING};

    // Saved, viewport mismatch, save failed, capture timed out.  A launch
    // result never overrides a recorded ending.
    for ending in [0, 1, 3, 5] {
        assert_eq!(crate::screenshot_exit_status(ending, true), Some(ending));
        assert_eq!(crate::screenshot_exit_status(ending, false), Some(ending));
    }
    // Nothing recorded: a clean close never saved, while a launch error
    // keeps eframe's own report and failure.
    assert_eq!(
        crate::screenshot_exit_status(NO_ENDING, true),
        Some(EXIT_RUN_TIMED_OUT)
    );
    assert_eq!(crate::screenshot_exit_status(NO_ENDING, false), None);
}

/// Callback files that must enter `RtGuard`.  ASIO's one file covers the
/// silence and panic paths too.
const GUARDED_CALLBACKS: &[&str] = &[
    "src/audio/win/asio/callback.rs",
    "src/audio/input.rs",
    "src/audio/linux/duplex/filter.rs",
    "src/audio/linux/native.rs",
    "src/audio/linux/duplex/virtual_source.rs",
    "src/audio/linux/sink_monitor.rs",
    "src/audio/mixer/mod.rs",
    "src/audio/extra_output/mixer.rs",
];

/// A tripwire for entry points a normal test cannot enter on every platform.
/// A test's own `RtGuard` is the outermost, and `RtGuard` counts only the
/// outermost, so a test under a guard still reads zero when the callback's
/// own guard is gone.  `audio/mixer` and `extra_output` have the same blind
/// spot.  File-level on purpose: a brace match would be a fragile stand-in
/// for the same evidence.
#[test]
fn every_audio_callback_holds_the_allocation_guard() {
    assert!(
        !GUARDED_CALLBACKS.is_empty(),
        "the guarded-callback list is empty"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), root, &mut files);
    let mut missing = Vec::new();
    for &callback in GUARDED_CALLBACKS {
        let Some((_, path)) = files.iter().find(|(file, _)| file.as_str() == callback) else {
            missing.push(format!("{callback}: no such source file"));
            continue;
        };
        let source = std::fs::read_to_string(path).expect("read source");
        // A prose line may name the guard without entering it.
        if boundary_lines(&source, &["RtGuard::enter()"]).is_empty() {
            missing.push(format!("{callback}: no `RtGuard::enter()`"));
        }
    }
    assert!(
        missing.is_empty(),
        "an audio callback enters no allocation guard (AGENTS.md, Audio thread):\n{}",
        missing.join("\n")
    );
}

/// Callback files and the call that keeps a panic inside RigWerk.
/// File-level, as [`GUARDED_CALLBACKS`] is.
const CONTAINED_CALLBACKS: &[(&str, &str)] = &[
    ("src/audio/input.rs", "contain("),
    // The clock output's callback goes through `render_or_silence`, which is
    // defined in the same file: matching `contain(` here would also hit the
    // wrapper's own body.
    ("src/audio/output.rs", "render_or_silence("),
    ("src/audio/extra_output/stream.rs", "render_or_silence("),
    ("src/audio/linux/native.rs", "contain("),
    ("src/audio/linux/sink_monitor.rs", "contain("),
    ("src/audio/linux/duplex/filter.rs", "contain("),
    ("src/audio/linux/duplex/virtual_source.rs", "contain("),
    ("src/audio/win/wasapi/real.rs", "contain("),
    ("src/audio/win/wasapi/exclusive.rs", "contain("),
    ("src/audio/win/asio/callback.rs", "catch_unwind"),
];

/// An unwind across the host's C callback is undefined behaviour or an
/// abort (`audio/panic_guard`).  These callbacks need a device or a live
/// graph, so this checks the source, as the allocation guard does.
#[test]
fn every_audio_callback_contains_its_body() {
    assert!(
        !CONTAINED_CALLBACKS.is_empty(),
        "the contained-callback list is empty"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), root, &mut files);
    let mut missing = Vec::new();
    for &(callback, marker) in CONTAINED_CALLBACKS {
        let Some((_, path)) = files.iter().find(|(file, _)| file.as_str() == callback) else {
            missing.push(format!("{callback}: no such source file"));
            continue;
        };
        let source = std::fs::read_to_string(path).expect("read source");
        // A prose line may name the guard without calling it.
        if boundary_lines(&source, &[marker]).is_empty() {
            missing.push(format!("{callback}: no `{marker}`"));
        }
    }
    assert!(
        missing.is_empty(),
        "an audio callback lets a panic reach the host (AGENTS.md, Audio thread):\n{}",
        missing.join("\n")
    );
}

/// Files that must not name a [`DARK_ONLY`] token, plus any name of their own.
const PALETTE_ONLY: &[(&str, &[&str])] = &[
    ("src/ui/calibration.rs", &[]),
    ("src/ui/frame.rs", &[]),
    // A literal point size is banned here the same way a dark token is.
    ("src/ui/pages/tuner.rs", &["FontId::new"]),
    ("src/ui/widgets/mod.rs", &[]),
];

/// Per-theme constants and colour constructors a surface must not name.
/// `from_black_alpha` stays off this list: the measure window's shadow is
/// black in both themes, and a palette tint goes through
/// [`crate::ui::theme::with_alpha`].
const DARK_ONLY: &[&str] = &[
    "DARK",
    "LIGHT",
    "CARD",
    "BORDER_SOFT",
    "BORDER",
    "TEXT",
    "MUTED",
    "DIM",
    "TEAL",
    "AMBER",
    "GREEN",
    "from_rgb",
    "from_rgb_additive",
    "from_rgba_unmultiplied",
    "from_rgba_unmultiplied_const",
    "from_rgba_premultiplied",
    "from_gray",
    "from_white_alpha",
    "from_hex",
];

/// `line` with comments removed.  `depth` carries an open block comment
/// across lines.  String literals are not lexed, so a `//` inside one ends
/// the line early: that can hide a name, never invent one.
fn code_only(line: &str, depth: &mut usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut index = 0;
    while index < chars.len() {
        if *depth > 0 {
            if chars[index] == '/' && chars.get(index + 1) == Some(&'*') {
                *depth += 1;
                index += 2;
            } else if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                *depth -= 1;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if chars[index] == '/' && chars.get(index + 1) == Some(&'/') {
            break;
        }
        if chars[index] == '/' && chars.get(index + 1) == Some(&'*') {
            *depth += 1;
            index += 2;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// Case-sensitive, whole name: `text` is a field and `text3` a token,
/// `TEXT` is the constant.
fn dark_only_lines<'a>(source: &str, names: &[&'a str]) -> Vec<(usize, &'a str)> {
    let continues = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut depth = 0;
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let code = code_only(line, &mut depth);
        for &name in names {
            let used = code.match_indices(name).any(|(at, _)| {
                !continues(code[..at].chars().next_back())
                    && !continues(code[at + name.len()..].chars().next())
            });
            if used {
                out.push((index + 1, name));
            }
        }
    }
    out
}

#[test]
fn the_light_theme_surfaces_read_the_palette() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), root, &mut files);
    let mut seen = 0;
    let mut hits = Vec::new();
    for (file, path) in files {
        let Some(entry) = PALETTE_ONLY.iter().find(|entry| entry.0 == file.as_str()) else {
            continue;
        };
        seen += 1;
        let source = std::fs::read_to_string(&path).expect("read source");
        let mut names = DARK_ONLY.to_vec();
        names.extend_from_slice(entry.1);
        for (line, name) in dark_only_lines(&source, &names) {
            hits.push(format!("{file}:{line}: {name}"));
        }
    }
    assert_eq!(
        seen,
        PALETTE_ONLY.len(),
        "PALETTE_ONLY names a file the walk did not find"
    );
    assert!(
        hits.is_empty(),
        "these surfaces must read the palette (finding U8):\n{}",
        hits.join("\n")
    );
}

#[test]
fn a_dark_token_scan_is_case_sensitive_and_skips_comments() {
    let source = "// CARD and TEXT in prose\n\
         let a = colours.text3;\n\
         let b = DARK.strip;  // the old card\n\
         /* CARD\n\
            spans lines */\n\
         let c = egui::Color32::from_rgb(1, 2, 3);\n";
    assert_eq!(
        dark_only_lines(source, DARK_ONLY),
        [(3, "DARK"), (6, "from_rgb")]
    );

    let light = "let a = LIGHT.bg;\nlet b = Color32::from_hex(\"#ffffff\");\n";
    assert_eq!(
        dark_only_lines(light, DARK_ONLY),
        [(1, "LIGHT"), (2, "from_hex")]
    );

    let note = "    let big = egui::FontId::new(104.0, family);\n";
    assert_eq!(
        dark_only_lines(note, &["FontId::new"]),
        [(1, "FontId::new")]
    );
}

#[test]
fn every_dark_only_token_matches_a_sample() {
    // The palette scan would pass silently if a token stopped matching
    // anything, so every token must be found in a sample that uses it.  The
    // constructors are the ones this egui version has.
    let sample = concat!(
        "let a = DARK.bg;\n",
        "let b = LIGHT.bg;\n",
        "let c = CARD.fill;\n",
        "let d = BORDER_SOFT;\n",
        "let e = BORDER;\n",
        "let f = TEXT;\n",
        "let g = MUTED;\n",
        "let h = DIM;\n",
        "let i = TEAL;\n",
        "let j = AMBER;\n",
        "let k = GREEN;\n",
        "let l = egui::Color32::from_rgb(1, 2, 3);\n",
        "let m = egui::Color32::from_rgb_additive(1, 2, 3);\n",
        "let n = egui::Color32::from_rgba_unmultiplied(1, 2, 3, 4);\n",
        "let o = egui::Color32::from_rgba_unmultiplied_const(1, 2, 3, 4);\n",
        "let p = egui::Color32::from_rgba_premultiplied(1, 2, 3, 4);\n",
        "let q = egui::Color32::from_gray(1);\n",
        "let r = egui::Color32::from_white_alpha(1);\n",
        "let s = egui::Color32::from_hex(\"#ffffff\");\n",
    );
    for &token in DARK_ONLY {
        assert!(
            dark_only_lines(sample, DARK_ONLY)
                .iter()
                .any(|&(_, used)| used == token),
            "DARK_ONLY token {token:?} matches no sample"
        );
    }
}

/// Each format lives behind `PluginHostUi`.  The handle still names the
/// host types for its editor and CLAP accessors, so only the enum is scanned.
#[test]
fn the_plugin_handle_has_no_plugin_instance_enum() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/audio/plugins/handle.rs");
    let source = std::fs::read_to_string(&path).expect("read the plugin handle");
    assert!(
        !source.contains("PluginInstance"),
        "the plugin handle still names PluginInstance"
    );
}

/// Files that run on an audio callback, where a blocking lock can stall a
/// device.  `audio/clap/host.rs` is here because a plugin may log from its
/// `process` thread.
const CALLBACK_FILES: &[&str] = &[
    "src/audio/input.rs",
    "src/audio/output.rs",
    "src/audio/linux/native.rs",
    "src/audio/linux/duplex/filter.rs",
    "src/audio/linux/duplex/virtual_source.rs",
    "src/audio/linux/sink_monitor.rs",
    "src/audio/extra_output/stream.rs",
    "src/audio/win/asio/callback.rs",
    "src/audio/win/wasapi/real.rs",
    "src/audio/win/wasapi/exclusive.rs",
    "src/audio/clap/host.rs",
];

/// Allocating constructors.  `host_log` must not use one: a plugin may call
/// it from its process thread.
const ALLOCATING: &[&str] = &[
    "to_owned",
    "to_string",
    "to_string_lossy",
    "format!",
    "String::from",
    "String::new",
    "String::with_capacity",
    "String::from_utf8",
    "String::from_utf8_lossy",
    "String::from_utf16",
    "Vec::new",
    "Vec::from",
    "Vec::with_capacity",
    "vec!",
    "to_vec",
    "into_owned",
    "collect(",
    "collect::<",
    "Box::new",
    "Rc::new",
    "Arc::new",
    "cstring(",
];

/// Lines that call `.method(`.  `try_lock` is not `.lock`, and a comment
/// is not code.
fn method_call_lines(source: &str, method: &str) -> Vec<usize> {
    let continues = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut depth = 0;
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let code = code_only(line, &mut depth);
        for (at, _) in code.match_indices(method) {
            let before = code[..at].chars().next_back();
            let after = code[at + method.len()..].chars().next();
            if continues(before) || continues(after) {
                continue;
            }
            let receiver = code[..at].trim_end().chars().next_back();
            let call = code[at + method.len()..].trim_start().chars().next();
            if receiver == Some('.') && call == Some('(') {
                out.push(index + 1);
            }
        }
    }
    out
}

/// Lines that use `token` as a whole name at its start: `to_string` does
/// not match `to_string_lossy`.
fn token_lines(source: &str, token: &str) -> Vec<usize> {
    let continues = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let ends_in_name = continues(token.chars().next_back());
    let mut depth = 0;
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let code = code_only(line, &mut depth);
        for (at, _) in code.match_indices(token) {
            let before = code[..at].chars().next_back();
            let after = code[at + token.len()..].chars().next();
            if !continues(before) && (!ends_in_name || !continues(after)) {
                out.push(index + 1);
            }
        }
    }
    out
}

/// From the line containing `opener` to its matching `}`.  Braces are
/// counted raw, so a brace in a string would end the body early.  The
/// pinned functions keep none.
fn block_body(source: &str, opener: &str) -> Option<String> {
    let mut out = String::new();
    let mut inside = false;
    let mut opened = false;
    let mut depth = 0usize;
    for line in source.lines() {
        if !inside {
            if !line.contains(opener) {
                continue;
            }
            inside = true;
        }
        out.push_str(line);
        out.push('\n');
        opened |= line.contains('{');
        depth += line.matches('{').count();
        depth = depth.saturating_sub(line.matches('}').count());
        if opened && depth == 0 {
            break;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// A plugin may call the CLAP log path from its `process` thread, so
/// `host_log` and `impl LogSlot` allocate nothing.
///
/// This reads the files as text.  It cannot see a lock or an allocation in
/// a helper, a macro, an alias, or `try_read`/`try_write`.  Comments are
/// skipped; a string is not, so `.lock()` in one is a false positive, never
/// a missed lock.  `block_body` counts braces raw; the scan checks each body
/// still reaches its last item, so a brace in a literal fails instead of
/// narrowing the scan.
#[test]
fn audio_callbacks_take_no_blocking_lock() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), root, &mut files);
    let path_of = |want: &str| {
        files
            .iter()
            .find(|(file, _)| file == want)
            .map(|(_, path)| path.as_path())
            .unwrap_or_else(|| panic!("the walk did not find {want}"))
    };

    let mut hits = Vec::new();
    for file in CALLBACK_FILES {
        let source = std::fs::read_to_string(path_of(file)).expect("read source");
        for line in method_call_lines(&source, "lock") {
            hits.push(format!("{file}:{line}: .lock()"));
        }
        for path in ["Mutex::lock", "RwLock::read", "RwLock::write"] {
            for line in token_lines(&source, path) {
                hits.push(format!("{file}:{line}: {path}()"));
            }
        }
        // `.read()` and `.write()` are only unambiguous when the file names
        // the type: without it, `File::read` and `LogSlot::write` look the
        // same.
        if !token_lines(&source, "RwLock").is_empty() {
            for method in ["read", "write"] {
                for line in method_call_lines(&source, method) {
                    hits.push(format!("{file}:{line}: .{method}()"));
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "a callback waits on a lock (AGENTS.md, Audio thread):\n{}",
        hits.join("\n")
    );

    let host = "src/audio/clap/host.rs";
    let source = std::fs::read_to_string(path_of(host)).expect("read source");
    let mut hits = Vec::new();
    for (name, opener, tail) in [
        ("host_log", "fn host_log(", "try_lock_now"),
        ("impl LogSlot", "impl LogSlot {", "fn push"),
    ] {
        let body = block_body(&source, opener).unwrap_or_else(|| panic!("{name} is in {host}"));
        assert!(
            body.contains(tail),
            "{name}'s body ended before `{tail}`: a brace in a literal?"
        );
        for token in ALLOCATING {
            for line in token_lines(&body, token) {
                hits.push(format!("{host}: {name} uses {token} (body line {line})"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "the CLAP log path allocates on the plugin's process thread:\n{}",
        hits.join("\n")
    );
}

#[test]
fn every_allocation_token_matches_a_sample() {
    // The CLAP log scan would pass silently if a token stopped matching
    // anything, so every token must be found in a sample that uses it.
    let sample = concat!(
        "let a = x.to_owned();\n",
        "let b = x.to_string();\n",
        "let c = x.to_string_lossy();\n",
        "let d = format!(\"{x}\");\n",
        "let e = String::from(\"x\");\n",
        "let f = String::new();\n",
        "let g = String::with_capacity(8);\n",
        "let h = String::from_utf8(v)?;\n",
        "let i = String::from_utf8_lossy(&v);\n",
        "let j = String::from_utf16(&v)?;\n",
        "let k = Vec::new();\n",
        "let l = Vec::from(v);\n",
        "let m = Vec::with_capacity(8);\n",
        "let n = vec![0u8; 8];\n",
        "let o = v.to_vec();\n",
        "let p = cow.into_owned();\n",
        "let q = v.iter().collect();\n",
        "let q = v.iter().collect::<Vec<_>>();\n",
        "let r = Box::new(0);\n",
        "let s = Rc::new(0);\n",
        "let t = Arc::new(0);\n",
        "let u = cstring(\"x\");\n",
    );
    for token in ALLOCATING {
        assert!(
            !token_lines(sample, token).is_empty(),
            "ALLOCATING token {token:?} matches no sample"
        );
    }
}

#[test]
fn a_try_lock_is_not_a_blocking_lock() {
    let source = "// .lock() in prose\nlet a = lock.try_lock();\nlet b = m\n    .lock();\n";
    assert_eq!(method_call_lines(source, "lock"), [4]);
    assert_eq!(token_lines("let g = Mutex::lock(&m);", "Mutex::lock"), [1]);
    assert_eq!(
        token_lines("let g = RwLock::read(&r);", "RwLock::read"),
        [1]
    );
    assert_eq!(
        token_lines("let g = RwLock::write(&r);", "RwLock::write"),
        [1]
    );
    assert_eq!(method_call_lines("let g = lock.read();", "read"), [1]);
    assert!(method_call_lines("let g = lock.try_read();", "read").is_empty());
    assert!(token_lines("let a = text.to_string_lossy();", "to_string").is_empty());
    assert_eq!(token_lines("let a = text.to_owned();", "to_owned"), [1]);
    assert_eq!(
        token_lines("let mut s = String::new();", "String::new"),
        [1]
    );
    assert_eq!(
        token_lines("let s = String::with_capacity(8);", "String::with_capacity"),
        [1]
    );
    assert_eq!(token_lines("let mut v = Vec::new();", "Vec::new"), [1]);
    assert_eq!(token_lines("let v = vec![0u8; 8];", "vec!"), [1]);
    assert_eq!(token_lines("let b = Box::new(0);", "Box::new"), [1]);
    // A literal is not lexed, so `.lock()` in one is a false positive.
    assert_eq!(method_call_lines("let s = \"x.lock()\";", "lock"), [1]);
}

/// Lines that read `.engine`, not a longer name such as `engine_state`.
/// Comments are skipped.
fn engine_references(source: &str) -> Vec<usize> {
    let mut depth = 0;
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let code = code_only(line, &mut depth);
        for (at, _) in code.match_indices(".engine") {
            let after = code[at + ".engine".len()..].chars().next();
            if after.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            out.push(index + 1);
        }
    }
    out
}

/// No production file under `src/ui` reads `.engine` directly.  `tests.rs`
/// is exempt.  This cannot see an access through another module's helper.
#[test]
fn the_ui_never_reaches_the_engine_directly() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src/ui"), root, &mut files);
    assert!(
        files.iter().any(|(file, _)| file == "src/ui/shell.rs"),
        "the walk lost the UI source"
    );
    let mut hits = Vec::new();
    for (file, path) in &files {
        let source = std::fs::read_to_string(path).expect("read source");
        for line in engine_references(&source) {
            hits.push(format!("{file}:{line}: .engine"));
        }
    }
    assert!(
        hits.is_empty(),
        "a production UI file reads the engine directly instead of a RigWerkApp method:\n{}",
        hits.join("\n")
    );
}

#[test]
fn the_engine_scan_matches_the_field_not_the_wrapper_names() {
    let source = concat!(
        "// self.engine.state\n",
        "/* self.engine.state */\n",
        "let a = self.engine.state;\n",
        "let b = self.engine_state();\n",
        "let c = app\n",
        "    .engine\n",
        "    .plugin_views(0);\n",
    );
    assert_eq!(engine_references(source), [3, 6]);
}

/// A `#` line is prose.  A trailing comment is not part of the value.
fn action_refs(source: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("- uses:")
            .or_else(|| trimmed.strip_prefix("uses:"))
        else {
            continue;
        };
        let value = rest.split('#').next().unwrap_or("").trim();
        if !value.is_empty() {
            out.push((index + 1, value.to_owned()));
        }
    }
    out
}

/// A `./` action is a path in this repository, so it is pinned by construction.
fn action_is_pinned(reference: &str) -> bool {
    if reference.starts_with("./") {
        return true;
    }
    reference.rsplit_once('@').is_some_and(|(_, sha)| {
        sha.len() == 40 && sha.chars().all(|character| character.is_ascii_hexdigit())
    })
}

#[test]
fn an_action_ref_is_read_from_its_line() {
    let source = concat!(
        "steps:\n",
        "  - uses: actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09 # v5\n",
        "  # - uses: actions/checkout@v5\n",
        "    uses: actions/cache@v5\n",
    );
    let refs = action_refs(source);
    assert_eq!(refs.len(), 2, "{refs:?}");
    assert_eq!(refs[0].0, 2);
    assert_eq!(
        refs[0].1,
        "actions/checkout@fbc6f3992d24b796d5a048ff273f7fcc4a7b6c09"
    );
    assert_eq!(refs[1], (4, "actions/cache@v5".to_owned()));
    assert!(action_is_pinned(&refs[0].1));
    assert!(!action_is_pinned(&refs[1].1), "a tag is not a commit");
    assert!(!action_is_pinned("actions/checkout"));
    assert!(action_is_pinned("./.github/actions/linux-deps"));
}

fn yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .expect("read the workflow and action directories")
        .map(|entry| entry.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            yaml_files(&path, out);
            continue;
        }
        let yaml = path
            .extension()
            .is_some_and(|extension| extension == "yml" || extension == "yaml");
        if yaml {
            out.push(path);
        }
    }
}

#[test]
fn every_workflow_action_is_pinned_to_a_commit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    yaml_files(&root.join(".github/workflows"), &mut paths);
    // A composite action's own `uses:` lines are refs too.
    yaml_files(&root.join(".github/actions"), &mut paths);
    paths.sort();
    assert!(!paths.is_empty(), "found no workflows or actions");

    let mut pinned = 0;
    let mut unpinned = Vec::new();
    for path in paths {
        let name = path
            .strip_prefix(root)
            .expect("under the crate")
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(&path).expect("read workflow or action");
        for (line, reference) in action_refs(&source) {
            if action_is_pinned(&reference) {
                pinned += 1;
            } else {
                unpinned.push(format!("{name}:{line}: {reference} is not a 40-hex SHA"));
            }
        }
    }
    assert!(pinned > 0, "found no pinned actions");
    assert!(
        unpinned.is_empty(),
        "every action must pin a commit SHA (workflows and composite actions):\n{}",
        unpinned.join("\n")
    );
}
