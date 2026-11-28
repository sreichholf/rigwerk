//! Keeps a damaged entry in its slot.  Dropping it would shift every later
//! index.  The caller supplies the replacement; this module cannot see `app`.

use serde::de::{Deserializer, IgnoredAny};
use serde::Deserialize;

/// `IgnoredAny` accepts any entry, so only a type mismatch fails.
#[derive(Deserialize)]
#[serde(untagged)]
enum Entry<T> {
    Good(T),
    Empty(()),
    Bad(IgnoredAny),
}

/// `placeholder` replaces an entry that does not parse, `empty` one written
/// as `()`.  For structs, `()` is an empty entry and reads as the default.
/// For numbers and routing rows the app never writes `()`, so it is damage.
pub(crate) fn read_or_placeholder<'de, D, T>(
    deserializer: D,
    placeholder: fn() -> T,
    empty: fn() -> T,
) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    let entries = Vec::<Entry<T>>::deserialize(deserializer)?;
    Ok(entries
        .into_iter()
        .map(|entry| match entry {
            Entry::Good(value) => value,
            Entry::Empty(()) => empty(),
            Entry::Bad(_) => placeholder(),
        })
        .collect())
}
