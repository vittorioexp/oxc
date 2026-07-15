# Coding agent guides for `crates/oxc_formatter_yaml`

## Overview

Prettier compatible YAML formatter (`oxfmt`'s Tier 1 backend), using the `oxc_formatter_core` APIs.

- Built on `oxc_formatter_core` for the language-agnostic IR + Printer + builders + macros
  - See `crates/oxc_formatter_core/AGENTS.md` for the IR/pipeline details
- Two entry points:
  - `format()`: standalone files (returns a printable `Formatted`)
  - `format_to_ir()`: embedded use via the dispatcher (e.g. yaml-in-markdown);
    allocates from the shared `EmbeddedContext` arena, emits no BOM / trailing newline,
    and leaves `propagate_expand()` to the parent document
- The canonical reference is Prettier 3.9's `src/language-yaml/printer-yaml.js` + `print/*.js`
  - port its layout decisions — EXCEPT where they are bugs or internally inconsistent (see below)

### Known divergences (non-follow)

Follow Prettier by default. The exceptions — shared policy across all the formatter
crates — are the cases where **consistent output beats conformance percentage**:

- Prettier bugs acknowledged as open issues
- behaviors that are internally inconsistent (same construct, different output depending
  on node kind or context)

Affected conformance fixtures stay counted as failures
(see `tasks/prettier_conformance/snapshots/prettier.yaml.snap.md`). Style debates
(`status:needs discussion` issues) are still followed — do not "improve" on taste.

Current divergences:

- **anchor/tag order** (prettier#19524): source order is preserved, never reordered
- **`# prettier-ignore` range** (prettier#13008): suppresses exactly one node,
  never every following node
- **anchor next-line comments** (prettier#10518 / #9327): structurally avoided —
  the positional cursor makes them the next node's leading comments
- **blank lines** (prettier#15528): one unified rule — a blank line right after a node is
  preserved (normalized to one) if the source had one, never invented, identical for every
  node kind and context. Prettier's matrix (block collections only between documents;
  mappings only before end comments; unconditional insertion after block scalars) is not
  ported. This also keeps `proseWrap: never` idempotent where Prettier is not (prettier#10776)
- **folded scalar more-indented lines** (prettier#16126): never re-flowed under
  `proseWrap: always` — their line breaks are literal per YAML folding, so Prettier's
  wrapping at the print width changes the parsed value and breaks idempotency
- **"broken but not broken" flow collections**: Prettier sometimes emits a newline inside
  flow brackets while keeping them flat (no trailing comma, `]`/`}` on the content line) —
  multiline pairs (spec-example-7-20 / 9-4) and key trailing comments
  (flow-mapping/comments/key, see the NOT PORTED note in `src/print/mapping_item.rs`).
  Here a flow collection either fits on one line or breaks normally
- **comment position** (spec-example-6-1): a comment stays at its syntactic position;
  Prettier hoists a comment after `[` onto the `key:` line
- **trailing comment width** (`key: | # ...`): a same-line trailing comment is a
  `line_suffix` and never counts toward the `fits` measurement — the same treatment
  Prettier itself gives JS/JSON line comments and yaml flow collections. Prettier's yaml
  printer measures the one after a block scalar header inline and breaks the key line
  (`key:\n  | # ...`); that break is not ported
- **pragma** (`--insert-pragma` / `--require-pragma`): unsupported (ignored in conformance)

### Parser

Prettier parses with `yaml` + `yaml-unist-parser`. This crate uses
[`oxc-yaml-parser`](https://crates.io/crates/oxc-yaml-parser), a hand-written scanner/parser
whose AST mirrors yaml-unist-parser's node shapes; comments are kept as span-only trivia
on `Root`, never attached to nodes.

- Developed in a separate repository; the workspace `[patch.crates-io]` may temporarily
  point at a local checkout during coordinated changes
- Span caveats the printer relies on:
  - anchor/tag props sit OUTSIDE the node span — use `content_start()` / `mapping_item_start()`
  - an explicit key's span includes the `?`; a `MappingValue` span does NOT include the `:`
  - a block scalar's span consumes its trailing line breaks — gap measurement needs
    `item_gap_anchor` / `document_gap_anchor`

### Error semantics

`oxc-yaml-parser` is fail-fast (no partial AST), so `format()` / `format_to_ir()` return
`Err` on any syntax error and never format a broken AST. The caller (oxfmt) decides what
happens next. Under-indented multi-line flow scalars (prettier#8602) are one such error —
Prettier also rejects them since its `yaml@2` upgrade, so the string corruption reported
there cannot happen in either implementation.

### Line endings

The source is normalized to `\n`-only up front (`parse_root`): scalars are printed as raw
source slices throughout and the core IR forbids raw `\r`, so normalizing once keeps every
slice site (`split('\n')`, column/gap byte scans) free of CR handling. The printer re-emits
the configured `end_of_line` at the final stage. A leading BOM is stripped before parsing
and re-emitted by `format()`.

### Comments

Positional cursor (`Comments` in `src/comments.rs`), same approach as graphql/json —
yaml-unist-parser's attach algorithm is NOT ported. Placement is decided at print sites:

- `classify_gap` classifies inter-token gaps (same-line / line / blank);
  blank = a whitespace-only line strictly INSIDE the gap
- `write_trailing_same_line_comment`: same-line only when the gap holds nothing but
  whitespace and structural punctuation (`,` `:`)
- `flush_container_end_comments`: own-line comments indented deeper than the item column
  become the previous item's end comments (Prettier's `shouldOwnEndComment` re-derived
  positionally); block scalar values are excluded — comments under them lead the next item
- comments between an explicit key and its `:` split by column (deeper = key end comments,
  item column = before the `: ` line); comments AFTER the `:` stay pending and print as the
  value's middle comments (`: # c` + hardline)

### Notable layout techniques

- **Prettier's `conditionalGroup` → `best_fitting!` + `.memoized()`** (`src/print/mapping_item.rs`):
  variants are measured flat with early-exit at hardlines, so "the key line fits" is exactly
  Prettier's grouped-key break check. Memoization keeps the comment cursor from being
  consumed once per variant. Multiline scalar keys are pinned to the explicit `? ` form
- **consecutive hardlines collapse in the core printer**: blank-line runs inside block
  scalars / flow scalars / flow collections are emitted as raw `"\n"` text followed by a
  break that only re-arms indentation
- `collection_depth` (Prettier's `parentIndent`) and `last_descendant_end` live on the
  context for block scalar indentation / chomping decisions

## Verification

```sh
cargo c -p oxc_formatter_yaml
```

Run `clippy` and resolve all warnings.

### Fixture tests

Snapshot tests driven by fixture files under `tests/fixtures/yaml/`, covering what the
Prettier conformance suite does not (`# oxfmt-ignore`, divergence shapes, etc).
`build.rs` auto-generates a test case from every `.yaml` file using the core `test_support`
harness; add a case by dropping a new file into the directory.

```sh
cargo test -p oxc_formatter_yaml
# Review / accept snapshots after intentional changes
cargo insta review -p oxc_formatter_yaml
```

### Prettier conformance

Compares output against Prettier's snapshots and tracks failures (not passes); results live
in `tasks/prettier_conformance/snapshots/prettier.yaml.snap.md`. The `yaml` language is part
of the shared conformance binary. Options wired: printWidth / tabWidth / useTabs / endOfLine /
proseWrap / singleQuote / bracketSpacing / trailingComma.

Note: `useTabs` is a deliberate no-op on both sides — YAML forbids tab indentation, so
Prettier's yaml printer routes everything through `alignWithSpaces` (string-form `align`,
never converted to tabs) and this crate prints exclusively with `align()`, never `indent()`.
Prettier's `spec` fixtures run with `useTabs: true` as a regression test that output stays
space-indented; keep it that way when adding print code.

```sh
cargo run -p oxc_prettier_conformance
# Debug a specific test
cargo run -p oxc_prettier_conformance -- --filter yaml/<dir>/<file>
```

Failures must be either fixed or classified: a new failure is acceptable only when it falls
under the non-follow policy above, and it must be documented there.

### Manual checks

```sh
cargo run -p oxc_formatter_yaml --example yaml_formatter [filename]
# Dump the formatter IR
DUMP_IR=1 cargo run -p oxc_formatter_yaml --example yaml_formatter [filename]
# Compare with Prettier
npx prettier --parser=yaml [filename]
```
