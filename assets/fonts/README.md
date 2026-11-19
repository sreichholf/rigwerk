# Fonts

Subsets of IBM Plex Sans (Regular, Medium, SemiBold) and IBM Plex Mono
(Regular, Medium), under the SIL Open Font License 1.1 (`OFL.txt`).
`src/ui/fonts.rs` embeds them with `include_bytes!`.

## Source

The static TTFs from IBM's repository, `master`, fetched 2026-11-18:

- `https://github.com/IBM/plex/tree/master/packages/plex-sans/fonts/complete/ttf`
  (font version 3.006, `@ibm/plex-sans` 1.3.6)
- `https://github.com/IBM/plex/tree/master/packages/plex-mono/fonts/complete/ttf`
  (font version 2.005, `@ibm/plex-mono` 2.5.0)

`OFL.txt` is that package's `LICENSE.txt`.

## Names

"Plex" is a Reserved Font Name, and a subset is a Modified Version under
the OFL.  So the subsets are renamed: the family names inside the files
read *RigWerk Sans* and *RigWerk Mono*, and the files are named to match.
The copyright and licence entries are kept.

| File | From | Bytes |
|---|---|---|
| `RigWerkSans-Regular.ttf` | `IBMPlexSans-Regular.ttf` | 62 148 |
| `RigWerkSans-Medium.ttf` | `IBMPlexSans-Medium.ttf` | 62 480 |
| `RigWerkSans-SemiBold.ttf` | `IBMPlexSans-SemiBold.ttf` | 62 732 |
| `RigWerkMono-Regular.ttf` | `IBMPlexMono-Regular.ttf` | 41 348 |
| `RigWerkMono-Medium.ttf` | `IBMPlexMono-Medium.ttf` | 41 764 |

## Subset

fontTools 4.68.0, hinting and layout tables kept, glyph names, `DSIG` and
`meta` dropped:

```
pyftsubset <file> --output-file=<out> \
  --unicodes=U+0020-007E,U+00A0-00FF,U+2000-206F,U+2190-21FF,U+2200-22FF,U+25B4,U+25BE \
  --desubroutinize --no-glyph-names --notdef-outline \
  --name-IDs=0,1,2,3,4,5,6,13,14,16,17 --drop-tables+=DSIG,meta
```

then "IBM Plex" → "RigWerk" and "IBMPlex" → "RigWerk" in the `name` table.

The ranges are Basic Latin, Latin-1 Supplement, General Punctuation,
Arrows, Mathematical Operators and the two small triangles.  Plex has no
⇧ (U+21E7), ▴ (U+25B4) or ▾ (U+25BE); egui's default fonts, kept as the
fallback, draw those.
