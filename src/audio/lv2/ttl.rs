//! In-memory Turtle triples for the LV2 scan, before any binary is loaded.
//!
//! [`TtlStore::load_file`] appends one file, with that file's `file://` URI
//! as the base, so a relative IRI resolves against the file's folder.
//! Terms are owned `String`s.  `oxrdf` is not a dependency, so a term is the
//! parser's display form: a named node in angle brackets, a literal as
//! quoted Turtle.  A `file://` URI becomes a path only through
//! [`path_from_file_uri`].

use std::path::{Path, PathBuf};

use oxttl::TurtleParser;

pub(crate) const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

#[derive(Clone, Debug)]
struct Triple {
    subject: String,
    predicate: String,
    /// A named node's IRI, a literal's lexical value, or a blank node label.
    object: String,
    literal: bool,
    /// A literal's datatype IRI; `None` for a plain or language-tagged one.
    datatype: Option<String>,
    language: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Object {
    Iri(String),
    Blank(String),
    Literal {
        value: String,
        datatype: Option<String>,
        language: bool,
    },
}

#[derive(Default)]
pub(crate) struct TtlStore {
    triples: Vec<Triple>,
    /// Blank node labels stay per file.
    files: usize,
}

impl TtlStore {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The file's own `file://` URI is the base, so a relative IRI resolves
    /// against its folder.
    pub(crate) fn load_file(&mut self, path: &Path) -> Result<(), String> {
        let file = self.files;
        self.files += 1;
        let base = file_uri(path);
        let text = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let parser = TurtleParser::new()
            .with_base_iri(base)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        for triple in parser.for_slice(&text) {
            let triple = triple.map_err(|error| format!("{}: {error}", path.display()))?;
            let is_literal = triple.object.is_literal();
            let display = triple.object.to_string();
            let (datatype, language) = if is_literal {
                literal_suffix(&display)
            } else {
                (None, false)
            };
            let object = if is_literal {
                literal_text(&display)
            } else {
                scoped(
                    iri_text(&triple.object),
                    triple.object.is_blank_node(),
                    file,
                )
            };
            self.triples.push(Triple {
                subject: scoped(
                    iri_text(&triple.subject),
                    triple.subject.is_blank_node(),
                    file,
                ),
                predicate: iri_text(&triple.predicate),
                object,
                literal: is_literal,
                datatype,
                language,
            });
        }
        Ok(())
    }

    pub(crate) fn objects(&self, subject: &str, predicate: &str) -> Vec<String> {
        self.triples
            .iter()
            .filter(|triple| triple.subject == subject && triple.predicate == predicate)
            .map(|triple| triple.object.clone())
            .collect()
    }

    pub(crate) fn iris(&self, subject: &str, predicate: &str) -> Vec<String> {
        self.triples
            .iter()
            .filter(|triple| {
                triple.subject == subject && triple.predicate == predicate && !triple.literal
            })
            .map(|triple| triple.object.clone())
            .collect()
    }

    pub(crate) fn literals(&self, subject: &str, predicate: &str) -> Vec<String> {
        self.triples
            .iter()
            .filter(|triple| {
                triple.subject == subject && triple.predicate == predicate && triple.literal
            })
            .map(|triple| triple.object.clone())
            .collect()
    }

    pub(crate) fn literal(&self, subject: &str, predicate: &str) -> Option<String> {
        self.literals(subject, predicate).into_iter().next()
    }

    pub(crate) fn iri(&self, subject: &str, predicate: &str) -> Option<String> {
        self.iris(subject, predicate).into_iter().next()
    }

    pub(crate) fn subjects_of_type(&self, rdf_type: &str) -> Vec<String> {
        let mut seen = Vec::new();
        self.triples
            .iter()
            .filter(|triple| triple.predicate == RDF_TYPE && triple.object == rdf_type)
            .filter_map(|triple| {
                if seen.contains(&triple.subject) {
                    None
                } else {
                    seen.push(triple.subject.clone());
                    Some(triple.subject.clone())
                }
            })
            .collect()
    }

    pub(crate) fn statements(&self, subject: &str) -> Vec<(String, Object)> {
        self.triples
            .iter()
            .filter(|triple| triple.subject == subject)
            .map(|triple| {
                let object = if triple.literal {
                    Object::Literal {
                        value: triple.object.clone(),
                        datatype: triple.datatype.clone(),
                        language: triple.language,
                    }
                } else if triple.object.starts_with("_:") {
                    Object::Blank(triple.object.clone())
                } else {
                    Object::Iri(triple.object.clone())
                };
                (triple.predicate.clone(), object)
            })
            .collect()
    }

    pub(crate) fn types(&self, subject: &str) -> Vec<String> {
        self.iris(subject, RDF_TYPE)
    }
}

fn iri_text(term: &impl std::fmt::Display) -> String {
    let text = term.to_string();
    match text
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    {
        Some(iri) => iri.to_owned(),
        None => text,
    }
}

/// Blank node labels are scoped to their document.  Prefixing every label
/// with the file it came from keeps `_:p1` in two appended files apart, which
/// a merged store would otherwise read as one node.
fn scoped(text: String, blank: bool, file: usize) -> String {
    if blank {
        format!("_:{file}_{}", text.trim_start_matches("_:"))
    } else {
        text
    }
}

fn literal_text(text: &str) -> String {
    let mut chars = text.chars();
    if chars.next() != Some('"') {
        return text.to_owned();
    }
    let mut out = String::new();
    while let Some(character) = chars.next() {
        match character {
            '"' => break,
            '\\' => match chars.next() {
                Some('b') => out.push('\u{8}'),
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('f') => out.push('\u{c}'),
                Some('r') => out.push('\r'),
                Some('u') => push_hex(&mut out, &mut chars, 4),
                Some('U') => push_hex(&mut out, &mut chars, 8),
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

/// What follows the closing quote in the parser's display form.
fn literal_suffix(text: &str) -> (Option<String>, bool) {
    let mut chars = text.char_indices();
    if !matches!(chars.next(), Some((_, '"'))) {
        return (None, false);
    }
    let mut escaped = false;
    for (index, character) in chars {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            let rest = &text[index + 1..];
            let datatype = rest
                .strip_prefix("^^<")
                .and_then(|rest| rest.strip_suffix('>'))
                .map(str::to_owned);
            return (datatype, rest.starts_with('@'));
        }
    }
    (None, false)
}

fn push_hex(out: &mut String, chars: &mut std::str::Chars<'_>, digits: u32) {
    let mut value = 0u32;
    for _ in 0..digits {
        let Some(digit) = chars.next().and_then(|character| character.to_digit(16)) else {
            return;
        };
        value = value * 16 + digit;
    }
    if let Some(character) = char::from_u32(value) {
        out.push(character);
    }
}

/// Escaped so it can be a Turtle base.
pub(crate) fn file_uri(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut text = absolute.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    let mut uri = String::from("file://");
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b':'
            | b'@'
            | b'+'
            | b','
            | b'='
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b';' => uri.push(byte as char),
            other => {
                uri.push('%');
                uri.push(HEX[(other >> 4) as usize] as char);
                uri.push(HEX[(other & 0x0f) as usize] as char);
            }
        }
    }
    uri
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Linux `/usr/...` and Windows `file:///C:/...` are both accepted.  RFC
/// 8089: an empty authority, `localhost` and no authority (`file:/...`) are
/// local.  Another host, another scheme, or a relative path is refused.
/// `.` and `..` resolve lexically, so `bundle/../elsewhere` cannot leave
/// the bundle.
pub(crate) fn path_from_file_uri(uri: &str) -> Result<PathBuf, String> {
    let Some(scheme) = uri.get(..5) else {
        return Err(format!("not a file URI: {uri}"));
    };
    if !scheme.eq_ignore_ascii_case("file:") {
        return Err(format!("not a file URI: {uri}"));
    }
    let rest = &uri[5..];
    let path = match rest.strip_prefix("//") {
        Some(authority_and_path) => {
            let (authority, path) = authority_and_path
                .find('/')
                .map_or((authority_and_path, ""), |slash| {
                    authority_and_path.split_at(slash)
                });
            if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
                return Err(format!("a file URI with a host is not supported: {uri}"));
            }
            path
        }
        None => rest,
    };
    if !path.starts_with('/') {
        return Err(format!("not an absolute file URI: {uri}"));
    }
    let decoded = percent_decode(path)?;
    // `file:///C:/...` keeps the drive letter; the leading slash is the URI's
    // root, not part of the Windows path.
    let bytes = decoded.as_bytes();
    let path = if bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        decoded[1..].to_owned()
    } else {
        decoded
    };
    Ok(lexical(Path::new(&path)))
}

/// Lexical only: no file system.  `..` at the root stays.
pub(crate) fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else if !out.has_root() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Binaries a bundle names must stay inside this.
pub(crate) fn bundle_root(bundle: &Path) -> PathBuf {
    lexical(&std::path::absolute(bundle).unwrap_or_else(|_| bundle.to_path_buf()))
}

fn percent_decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes
                .get(index + 1)
                .and_then(|byte| hex_value(*byte))
                .ok_or_else(|| format!("bad percent escape in {text}"))?;
            let low = bytes
                .get(index + 2)
                .and_then(|byte| hex_value(*byte))
                .ok_or_else(|| format!("bad percent escape in {text}"))?;
            out.push(high << 4 | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("a file URI that is not UTF-8: {text}"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
