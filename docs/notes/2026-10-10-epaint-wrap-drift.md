# Agent Note

Status: implemented
Class: dependency
Date: 2026-10-10

## Problem

In `sessions/100` the transcript lost its left edge: reasoning text started
off-screen behind the sidebar, there was no way to scroll to it, and it wrapped
at about 860 px instead of the column's 649. The composer and the turn rail
stayed where they belonged. Only text drawn *after* one assistant message was
affected, and that message was a run of text-mode `run-pwsh` calls: long
PowerShell lines full of `["…"]` and `C:\…` paths with no spaces in them.

The cause is a bug in `epaint` 0.28.1, `text_layout::line_break`. A job whose
first row is indented (`leading_space`) keeps `first_row_indentation` after
that row breaks normally at a space. Further down the same paragraph, a row
with no word boundary since its start (a path wider than the row) takes the
"empty first row" branch meant for the first row. That branch shifts every
glyph after it left by the indentation, so the galley's next row starts at
`-indent`.

A `Label` in a wrapping horizontal layout is the only caller that indents
the first row, and it places itself at `ui.max_rect().left()` and then
`allocate_rect`s each row. A row at `-indent` widens the `Ui`'s `max_rect`
to the left, the next label starts from the widened edge (with a larger
indentation), and the shift compounds. egui_commonmark lays out every
paragraph that way and splits the text at every `[`, `]` and code span. In
`sessions/100` one paragraph moved the column's left edge from x=466 to
x=−253. Everything later in the transcript was then laid out in that wider
column, which is why it wrapped wide and ran off the left. The panel clips at
its own edge and the transcript only scrolls vertically, so the text could
not be reached.

## Decision

`vendor/epaint` is epaint 0.28.1 from crates.io with one line added, which
resets `first_row_indentation = 0.0` once the first row breaks normally. The
root `Cargo.toml` swaps it in through `[patch.crates-io]` and keeps
`vendor/` out of the workspace members. The rest of the crate is unchanged,
fonts included (the `default_fonts` feature reads them).

`messages::tests::wrapped_text_with_an_unbreakable_run_stays_in_its_column`
pins it. It fails on stock 0.28.1 (content at x≈−52 against a column at 8)
and passes with the patch.

## Consequences

Wrapped text now stays inside its column everywhere: markdown paragraphs,
and anything else drawn in a `horizontal_wrapped`. The repo carries about
1.9 MB of third-party source, 1.4 MB of it egui's bundled fonts.

The upstream bug is still in egui `main` and in 0.29.1, which is in the local
registry. An egui upgrade therefore has to re-vendor the new epaint and carry
the same line, or drop the patch once a release fixes it. The regression test
fails if the patch is lost.

## Alternatives

- **Upgrade egui.** It doesn't help: no release has the fix yet.
- **A git fork of egui as the patch source.** It keeps the repo small, but
  the user then has to keep a fork in sync, and a fresh checkout needs
  network access to build.
- **Contain markdown in the app.** Drawing each assistant block in a child
  `Ui` and advancing the parent by a rect clamped to the column would keep
  later turns intact. The offending message itself would still draw shifted
  left and clipped, because the drift happens inside egui_commonmark's own
  wrapped row, where the app has no hook.
- **Rewrite long tokens before rendering.** epaint only treats whitespace and
  CJK as word boundaries. Breaking long paths would mean inserting visible
  characters into what the user copies.
