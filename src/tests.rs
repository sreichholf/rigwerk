//! The glossary test (docs/ui-spec.md §2.1, §11): no string literal in the
//! code uses a word the glossary keeps out of the UI.  It reads the source
//! text, not the binary, so it sees every literal, including log lines and
//! hover text.  `tests.rs` files are not read: a test may name the old word
//! on purpose (an old save, a rejected label).  The same walk also checks the
//! layer direction AGENTS.md states, in production files: `audio` names no
//! `app`, `ui`, `egui` or `eframe`, and `app` names no `ui`.  `tests.rs` files
//! are exempt, so `app/tests.rs` and `app/appearance/tests.rs` still name
//! `crate::ui`.
//!
//! One gate reads a single file: the browser must reach CLAP through
//! `clap::scan_library_isolated`, the child process, because the in-process
//! `clap::scan_library` loads every bundle into this process.  The last test
//! here covers the binary's own `--clap-scan` arguments.
//!
//! A third check pins the allocation guard at the entry point of each audio
//! callback a normal test cannot enter on every platform.

use std::path::{Path, PathBuf};

/// The unambiguous words from the "Never in the UI" column.  Whole words,
/// case-insensitive, so *patch* does not match *dispatch*.
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
];

/// Platform code, where the word is the platform's own term (`quantum` in
/// PipeWire, `endpoint` in WASAPI).  Every literal in these folders may use
/// any banned word.  It stays.
const PLATFORM: &[&str] = &["src/audio/linux/", "src/audio/win/"];

/// Files whose literals are stored preference values, not text a person
/// reads.  §2.4 keeps the stored keys unchanged, so those literals stay.
const STORED_KEYS: &[&str] = &[
    // The saved output mode, read from files written before the rename.
    "src/app/latency.rs",
];

/// Files not migrated yet, with the banned words each one may still use.
/// Each entry says which step of the rewrite removes it (docs/ui-spec.md
/// §2.4 for a code rename, §12 for a step); the test fails when an entry
/// allows a word the file no longer uses, so the list only shrinks.  Step 8
/// empties it.
const NOT_MIGRATED: &[(&str, &[&str])] = &[];

/// One banned word in one literal.
#[derive(Debug, PartialEq)]
struct Hit {
    file: String,
    line: usize,
    word: &'static str,
    literal: String,
}

/// The `.rs` files under `dir`, as paths relative to the crate, sorted.
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

/// The string literals in `source` with the line each starts on.  Comments,
/// char literals and lifetimes are skipped, and so is every literal inside a
/// `#[serde(...)]` attribute: those name stored keys, which keep their old
/// spelling on purpose.
fn literals(source: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut line = 1;
    let mut index = 0;
    // Bracket depth inside a `#[serde(...)]`, while one is open.
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
                // `r"…"`, `r#"…"#`, `br"…"`, `b"…"`, `c"…"`.
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
                    // An identifier that starts with r, b or c.
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

/// A `"…"` body from `start`, and the index after its closing quote.
/// Escapes are kept as written; `\n` counts as a line only when real.
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

/// A raw string body from `start`, closed by `"` and `hashes` hashes.
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

/// The text a person reads: `{name}` and `{name:>4}` placeholders in a
/// format string name variables, not words, and are blanked.
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

/// Whether `text` holds `word` as a whole word, ignoring case.  Letters,
/// digits and `_` continue a word; a hyphen does not.
fn has_word(text: &str, word: &str) -> bool {
    let text = text.to_lowercase();
    let word = word.to_lowercase();
    let continues = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    text.match_indices(&word).any(|(at, _)| {
        !continues(text[..at].chars().next_back())
            && !continues(text[at + word.len()..].chars().next())
    })
}

/// Every banned word in every literal of the files under `src/`.
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

/// The layer direction AGENTS.md states ("Separation"), by prefix, for
/// production files: a file in `src/audio/` (or `src/audio.rs`) names neither
/// `crate::app` nor `crate::ui` and names neither `egui` nor `eframe`; a
/// file in `src/app/` (or `src/app.rs`) names no `crate::ui`.  `app` does
/// name `egui` for the theme and the zoom, so only `audio` lists it.
const LAYERING: &[(&str, &[&str])] = &[
    ("src/audio/", &["crate::app", "crate::ui", "egui", "eframe"]),
    (
        "src/audio.rs",
        &["crate::app", "crate::ui", "egui", "eframe"],
    ),
    ("src/app/", &["crate::ui"]),
    ("src/app.rs", &["crate::ui"]),
];

/// The 1-based lines of `source` that name one of `names`, with the name each
/// one used.  The check counts a name anywhere on a code line, including
/// inside a longer path or a string, and it does not strip block comments:
/// only a line whose first characters are `//` is prose.  `audio/nam.rs`
/// links to `crate::app::settings::PathPref` in a doc comment, and a doc link
/// is not a reference.
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

/// Every place a file names the layer above it, as `file:line: name`.
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

/// A residual gap: `super::super::app` is a valid path from `src/audio/*.rs`
/// and holds none of these names, so this text check cannot see it.
#[test]
fn a_layer_does_not_name_the_one_above_it() {
    let hits = boundary_hits();
    assert!(
        hits.is_empty(),
        "a layer names the one above it (AGENTS.md, Separation):\n{}",
        hits.join("\n")
    );
}

/// CLAP is the one scan that runs third-party code, so the browser must reach
/// it through the child process: the in-process `clap::scan_library` calls a
/// bundle's `init` in this process.  `app/plugins.rs` has its own
/// `scan_library` for the whole library, which is why the two call names are
/// counted apart.
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

/// One `--clap-scan` table row: the argv, and the file and limit expected.
type ClapScanCase<'a> = (&'a [&'a str], Option<(&'a str, u64)>);

/// `--clap-scan` takes a file and a limit and nothing else.  A missing path, a
/// missing or unusable limit, or a negative one leaves `main` to exit 2; a
/// trailing argument is ignored rather than read as part of the limit.
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

/// The files whose audio callback must hold `RtGuard`: ASIO's dispatch (the
/// silence and panic paths included), the duplex filter, the native capture,
/// the virtual source, the sink monitor, and both mixer renders.
const GUARDED_CALLBACKS: &[&str] = &[
    "src/audio/win/asio/callback.rs",
    "src/audio/linux/duplex/filter.rs",
    "src/audio/linux/native.rs",
    "src/audio/linux/duplex/virtual_source.rs",
    "src/audio/linux/sink_monitor.rs",
    "src/audio/mixer/mod.rs",
    "src/audio/extra_output/mixer.rs",
];

/// A tripwire for the entry points a normal test cannot enter on every
/// platform.  A test's own `RtGuard` is the outermost one, and `RtGuard`
/// counts only the outermost, so a test that drives the code under a guard
/// still reads zero when the callback's own guard is gone; `audio/mixer`'s
/// and `extra_output`'s counted blocks have the same blind spot.  This check
/// is file-level on purpose: following a signature to its closing brace would
/// be a fragile indirection for the same evidence.
#[test]
fn every_audio_callback_holds_the_allocation_guard() {
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
