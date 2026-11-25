//! The read that keeps a damaged entry in its own slot of a positional saved
//! list.
//!
//! Both the saved preferences (`app::settings`) and a device's routing rows
//! are indexed by position: dropping a damaged entry would move every entry
//! after it.  This is the read only, because the device types live here and
//! `audio` cannot reach into `app`; the replacement value and the report
//! belong to the caller.

use serde::de::{Deserializer, IgnoredAny};
use serde::Deserialize;

/// One stored list entry, readable or not.  `IgnoredAny` takes whatever the
/// entry is, so only the type mismatch fails.
#[derive(Deserialize)]
#[serde(untagged)]
enum Entry<T> {
    Good(T),
    Empty(()),
    Bad(IgnoredAny),
}

/// Reads a list entry by entry, keeping every index.  `placeholder` replaces
/// an entry that does not parse, `empty` one written as RON's `()`.
///
/// The two are separate because `()` means different things per list: for a
/// list of structs it is an entry written as an empty struct and reads as the
/// struct's default, while for a list of numbers or routing rows it is not a
/// value the app ever writes and is treated as damage.
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
