use oxc_formatter_core::{
    Buffer, MemoizeFormat, best_fitting,
    builders::{align, expand_parent, group, hard_line_break, line_suffix, space},
    format_args, write,
};
use oxc_span::Span;
use oxc_yaml_parser::ast::{Content, MappingItem};

use crate::{
    comments::{
        Gap, classify_gap, flush_leading_comments, is_suppressed_last_before, write_single_comment,
        write_suppressed_node, write_trailing_same_line_comment,
    },
    options::ProseWrap,
    print::{
        YamlFormatter, column_of, content_start, format_with, is_own_line, mapping_item_start,
        suppression_flush_bound, to_span, write_content,
    },
};

/// Where a mapping item lives; decides the empty-value layout
/// (`{a}` prints the bare key, `[? a]` keeps the explicit form).
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum FlowParent {
    /// Block mapping (`mappingItem`).
    No,
    /// Inside `{...}` (`flowMappingItem` with a `flowMapping` parent).
    Mapping,
    /// A pair inside `[...]` (`flowMappingItem` with a `flowSequence` parent).
    Sequence,
}

/// Ports Prettier's `printMappingItem` (`print/mapping-item.js`) for both
/// block (`mappingItem`) and flow (`flowMappingItem`) items.
///
/// Comment-based conditions from the original are re-derived positionally from
/// the comment cursor instead of pre-attached slots.
pub fn write_mapping_item<'a>(
    item: &'a MappingItem<'a>,
    parent_tag_is_set: bool,
    in_flow: FlowParent,
    f: &mut YamlFormatter<'_, 'a>,
) {
    let key_content = item.key.content.as_ref();
    let value_content = item.value.content.as_ref();

    let is_empty_key = key_content.is_none();
    let is_empty_value = value_content.is_none();

    if is_empty_key && is_empty_value {
        write!(f, ": ");
        // A same-line comment follows WITHOUT another space (`: # c`):
        // Prettier suppresses the line-suffix space for an empty mappingValue.
        if let Some(span) = f.context().comments().peek()
            && span.start >= item.span.end
            && f.context()
                .source_text()
                .all_bytes_match(item.span.end, span.start, |b| matches!(b, b' ' | b'\t'))
        {
            f.context().comments().take_before(span.end);
            let comment = format_with(move |f: &mut YamlFormatter<'_, 'a>| {
                write_single_comment(span, f);
            });
            write!(f, [line_suffix(&comment), expand_parent()]);
        }
        return;
    }

    let space_before_colon = key_content.is_some_and(|c| matches!(c, Content::Alias(_)));

    if is_empty_value {
        if in_flow == FlowParent::Mapping {
            // A lone key in a flow mapping prints as just the key.
            write_key(item, f);
            return;
        }
        // An explicit `? key` with no value is normalized to `key:` too.
        // Prettier's `!hasTrailingComment(key.content)`: with no `:`, a
        // same-line comment (`? key # c`, whitespace-only gap) attaches to the
        // key content and keeps the explicit form. After `key:` the gap
        // contains the `:`, the comment belongs to the value side, and the
        // implicit form stays (the caller emits it as a line suffix).
        let key_content_trailing_comment = f.context().comments().peek().is_some_and(|span| {
            span.start >= item.key.span.end
                && f.context()
                    .source_text()
                    .all_bytes_match(item.key.span.end, span.start, |b| matches!(b, b' ' | b'\t'))
        });
        if in_flow == FlowParent::No
            && is_absolutely_single_line(key_content, f)
            && !parent_tag_is_set
            && !key_content_trailing_comment
        {
            write_key(item, f);
            if space_before_colon {
                write!(f, space());
            }
            write!(f, ":");
            return;
        }
        write!(f, "? ");
        write!(f, align(2, &format_with(|f| write_key(item, f))));
        return;
    }

    if is_empty_key {
        write!(f, ": ");
        write!(f, align(2, &format_with(|f| write_value(item, f))));
        return;
    }

    // Force explicit key: the key isn't an inline node, or the source was
    // already explicit with a comment between `?` and the key, or between the
    // key and `:` (an implicit `key:` with a comment above the value keeps the
    // implicit form — the comment becomes the value's leading comment instead).
    let explicit_comment_before_key = item.key.explicit
        && key_content.is_some_and(|k| {
            f.context().comments().peek().is_some_and(|c| c.end <= content_start(k))
        });
    let value_start = value_content.map_or(item.span.end, |v| content_start(v));
    if !is_inline(key_content)
        || explicit_comment_before_key
        || (item.key.explicit && has_own_line_comment_before_value(item, f))
    {
        write!(f, "? ");
        // Comments between the key and `:` that are indented DEEPER than the
        // item are the key's end comments (inside the key's align); comments
        // at the item's own column lead the value (before the `: ` line).
        // Comments AFTER the `:` are the value's middle comments and stay
        // pending — `write_content` prints them right after the `: `.
        let item_column = column_of(&f.context().source_text(), mapping_item_start(item));
        let colon_bound =
            colon_position(&f.context().source_text(), item.key.span.end, value_start);
        let key_and_comments = format_with(move |f: &mut YamlFormatter<'_, 'a>| {
            write_key(item, f);
            let Some(bound) = colon_bound else { return };
            let source = f.context().source_text();
            while let Some(span) = f.context().comments().peek() {
                if span.end > bound || column_of(&source, span.start) <= item_column {
                    break;
                }
                f.context().comments().take_before(span.end);
                write!(f, hard_line_break());
                write_single_comment(span, f);
            }
        });
        write!(f, align(2, &key_and_comments));
        write!(f, hard_line_break());
        if let Some(bound) = colon_bound {
            let comments = f.context().comments().take_before(bound);
            for &span in comments {
                write_single_comment(span, f);
                write!(f, hard_line_break());
            }
        }
        write!(f, ": ");
        write!(f, align(2, &format_with(|f| write_value(item, f))));
        return;
    }

    // NOT PORTED (non-follow): in a flow collection Prettier prints a key's
    // same-line trailing comment with `breakParent` (`printer-yaml.js` exempts
    // only block-mapping keys), flipping the item to the explicit form while
    // its conditionalGroup keeps the enclosing flow FLAT — a newline inside
    // unbroken brackets with no trailing comma (`{ ? "key" # 1` + newline +
    // `  : value }`), the same inconsistency rejected for spec-example-7-20 /
    // 9-4. The comment instead takes the hardline-separator path below and the
    // flow breaks normally, like every other comment inside one
    // (`{` + newline + `  "key": # 1` ...).
    let key_single_line = is_single_line(key_content, f);
    let key_absolutely_single_line = is_absolutely_single_line(key_content, f);
    let key_trailing_same_line = key_has_trailing_comment(item, value_start, f);

    // Force single line: both sides are definitely single-line and comment-free
    // (a pending comment before the value body is the key's trailing comment or
    // the value's leading/middle comment — all of them break the single line).
    let value_body_start = value_content.map_or(item.span.end, |v| v.span().start);
    if key_single_line
        && !key_trailing_same_line
        && !has_pending_comment_before(value_body_start, f)
        && key_absolutely_single_line
        && is_absolutely_single_line(value_content, f)
    {
        write_key(item, f);
        if space_before_colon {
            write!(f, space());
        }
        write!(f, ": ");
        write_value(item, f);
        return;
    }

    // The general case. Prettier decides implicit vs explicit (`? key`) with
    // `conditionalGroup([[groupedKey, ifBreak(explicit, implicit, {groupId})]])`:
    // a key whose group breaks (multiline content or width overflow) flips the
    // item to the explicit form.
    let tab_width = f.options().indent_width.value();
    let value = value_content.expect("empty value handled above");

    // A key whose PRINTED form keeps hard line breaks always breaks the key
    // group in Prettier (breakParent propagation), i.e. always explicit. With
    // proseWrap preserve that is any multiline scalar; under always/never the
    // scalar re-folds, so only a blank line (paragraph breaks survive folding)
    // or a backslash continuation pins the structure — a merely-long key takes
    // the width check below instead (`a\n  true: ...` refolds to
    // `a true: ...`). Flow collections reformat onto one line as well.
    let key_is_multiline_scalar = !key_single_line
        && !matches!(key_content, Some(Content::FlowMapping(_) | Content::FlowSequence(_)))
        && (f.options().prose_wrap == ProseWrap::Preserve
            || has_forced_break_when_folded(key_content, f));
    if key_is_multiline_scalar {
        write!(f, "? ");
        write!(f, align(2, &format_with(|f| write_key(item, f))));
        write!(f, hard_line_break());
        write!(f, ": ");
        write!(f, align(2, &format_with(|f| write_value(item, f))));
        return;
    }

    // Separator between `:` and the value (mapping-item.js:101-125).
    let block_collection_without_props = match value {
        Content::Mapping(m) => m.props.anchor.is_none() && m.props.tag.is_none(),
        Content::Sequence(s) => s.props.anchor.is_none() && s.props.tag.is_none(),
        _ => false,
    };
    let hardline_separator = block_collection_without_props
        || has_pending_comment_before(value_start, f)
        || (in_flow == FlowParent::No && key_trailing_same_line && is_inline(Some(value)));

    if hardline_separator || !is_inline(Some(value)) {
        // The separator is pinned: hardline (block collection / comments), or
        // a space (block scalar & friends, whose hardlines Prettier's
        // conditionalGroup keeps from re-flowing the key line).
        write_key(item, f);
        let implicit_value = format_with(move |f: &mut YamlFormatter<'_, 'a>| {
            if space_before_colon {
                write!(f, space());
            }
            write!(f, ":");
            if hardline_separator {
                // Key's same-line comment must be emitted before the line break.
                write_trailing_same_line_comment(item.key.span.end, f);
                write!(f, hard_line_break());
            } else {
                write!(f, space());
            }
            // `# prettier-ignore` leading the value suppresses the value node
            // — except a block collection, whose first item claims the marker
            // instead (same delegation as at the document level).
            let value_is_block_collection =
                matches!(value, Content::Mapping(_) | Content::Sequence(_));
            if !value_is_block_collection && is_suppressed_last_before(f, value_start) {
                write_suppressed_node(Span::new(value_start, value.span().end), f);
            } else {
                let flush_bound =
                    suppression_flush_bound(value_is_block_collection, value_start, f);
                flush_leading_comments(flush_bound, f);
                write_content(value, f);
            }
        });
        write!(f, align(tab_width, &implicit_value));
        return;
    }

    // Width-dependent layout via `best_fitting!`. Variants are measured flat
    // with early-exit at hardlines, so:
    // - variant 1 fits = the key + `: ` + the value's FIRST line fit
    //   (a multiline value's own hardlines don't re-flow the key line —
    //   Prettier's conditionalGroup boundary), and
    // - variant 2 fits = the key fits (Prettier's groupedKey break check).
    // Content is memoized so the comment cursor advances only once.
    let key = format_with(|f: &mut YamlFormatter<'_, 'a>| write_key(item, f)).memoized();
    // The group wrapper mirrors Prettier's `genericPrint` (`group(printNode())`)
    // and is what decides variant ① for a multi-paragraph value: the paragraph
    // hardline expands the group, fits then measures its content in expanded
    // mode and exits `Yes` at the FIRST fill separator — so only `key: ` plus
    // the first word must fit and the fill wraps from the key line. A value
    // with no forced break keeps the group flat and is measured in full.
    let value_content_fmt = format_with(move |f: &mut YamlFormatter<'_, 'a>| {
        flush_leading_comments(value_start, f);
        write!(f, group(&format_with(|f| write_content(value, f))));
    })
    .memoized();
    let colon = if space_before_colon { " :" } else { ":" };

    // A definitely-single-line key never flips to the explicit form, no matter
    // how long (Prettier's `conditionalGroup([[printedKey, implicit]])`
    // short-circuit).
    if key_absolutely_single_line && !key_trailing_same_line {
        write!(
            f,
            best_fitting![
                format_args!(key, colon, space(), align(tab_width, &value_content_fmt)),
                format_args!(
                    key,
                    colon,
                    align(tab_width, &format_args!(hard_line_break(), value_content_fmt))
                ),
            ]
        );
        return;
    }

    write!(
        f,
        best_fitting![
            // Everything starting on one line.
            format_args!(key, colon, space(), align(tab_width, &value_content_fmt)),
            // Key line + value on the next line (fits when the key fits).
            format_args!(
                key,
                colon,
                align(tab_width, &format_args!(hard_line_break(), value_content_fmt))
            ),
            // Explicit form (key itself doesn't fit).
            format_args!(
                "? ",
                align(2, &key),
                hard_line_break(),
                ": ",
                align(2, &value_content_fmt)
            ),
        ]
    );
}

fn write_key<'a>(item: &'a MappingItem<'a>, f: &mut YamlFormatter<'_, 'a>) {
    if let Some(key) = &item.key.content {
        write_content(key, f);
    }
}

fn write_value<'a>(item: &'a MappingItem<'a>, f: &mut YamlFormatter<'_, 'a>) {
    if let Some(value) = &item.value.content {
        write_content(value, f);
    }
}

/// `isInlineNode`: scalars, aliases and flow collections are inline.
fn is_inline(content: Option<&Content<'_>>) -> bool {
    !matches!(
        content,
        Some(
            Content::Mapping(_)
                | Content::Sequence(_)
                | Content::BlockLiteral(_)
                | Content::BlockFolded(_)
        )
    )
}

/// The raw source of a flow scalar (plain/quoted); `None` for aliases,
/// collections and block scalars.
fn scalar_raw<'a>(content: &Content<'_>, f: &YamlFormatter<'_, 'a>) -> Option<&'a str> {
    let span = match content {
        Content::Plain(p) => p.span,
        Content::QuoteSingle(s) => s.span,
        Content::QuoteDouble(d) => d.span,
        _ => return None,
    };
    Some(f.context().source_text().text_for(&to_span(span)))
}

/// `isSingleLineNode`: the node occupies one source line.
fn is_single_line(content: Option<&Content<'_>>, f: &YamlFormatter<'_, '_>) -> bool {
    match content {
        None | Some(Content::Alias(_)) => true,
        Some(content) => scalar_raw(content, f).is_some_and(|raw| !raw.contains('\n')),
    }
}

/// `isAbsolutelyPrintedAsSingleLineNode`: the node WILL print on one line
/// regardless of width (so implicit style can be forced).
fn is_absolutely_single_line(content: Option<&Content<'_>>, f: &YamlFormatter<'_, '_>) -> bool {
    let Some(content) = content else { return true };
    if matches!(content, Content::Alias(_)) {
        return true;
    }
    let Some(raw) = scalar_raw(content, f) else { return false };

    let prose_wrap = f.options().prose_wrap;
    if prose_wrap == ProseWrap::Preserve {
        return !raw.contains('\n');
    }
    if has_backslash_continuation(raw) {
        return false;
    }
    if prose_wrap == ProseWrap::Never {
        // `never` folds every newline away, so only blank lines (which
        // survive folding) keep it multi-line.
        !has_blank_line(raw)
    } else {
        // `always` may wrap at any space.
        !raw.contains('\n') && !raw.contains(' ')
    }
}

fn has_blank_line(raw: &str) -> bool {
    let mut lines = raw.split('\n');
    lines.next();
    lines.any(|l| l.trim().is_empty())
}

/// A backslash at a line end (quoteDouble continuation) pins the line structure.
fn has_backslash_continuation(raw: &str) -> bool {
    raw.split('\n').any(|line| line.ends_with('\\'))
}

/// Whether a scalar keeps a forced line break even after `proseWrap`
/// always/never re-folding: a blank line (paragraph break) survives folding,
/// and a backslash line continuation pins the line structure.
fn has_forced_break_when_folded(content: Option<&Content<'_>>, f: &YamlFormatter<'_, '_>) -> bool {
    let Some(content) = content else { return false };
    // Defensive: non-scalars never re-fold.
    let Some(raw) = scalar_raw(content, f) else { return true };
    has_blank_line(raw) || has_backslash_continuation(raw)
}

/// Position of the `:` between an explicit key's end and its value, skipping
/// comments (whose text may contain `:`). Only whitespace and comments can
/// occupy that gap.
fn colon_position(source: &str, from: u32, to: u32) -> Option<u32> {
    let bytes = source.as_bytes();
    let end = (to as usize).min(bytes.len());
    let mut i = from as usize;
    while i < end {
        match bytes[i] {
            b'#' => {
                while i < end && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b':' => return u32::try_from(i).ok(),
            _ => i += 1,
        }
    }
    None
}

/// Is there a pending comment on the key's line, BEFORE the value starts?
/// (`key: # comment` with the value on the next line — a comment after the
/// value like `key: value # comment` is the value's trailing comment instead.)
fn key_has_trailing_comment(
    item: &MappingItem<'_>,
    value_start: u32,
    f: &YamlFormatter<'_, '_>,
) -> bool {
    let Some(span) = f.context().comments().peek() else { return false };
    let source = f.context().source_text();
    span.start >= item.key.span.end
        && span.end <= value_start
        && classify_gap(source.bytes_range(item.key.span.end, span.start)) == Gap::None
}

/// Is there any pending comment before `bound` (a leading comment of the value)?
fn has_pending_comment_before(bound: u32, f: &YamlFormatter<'_, '_>) -> bool {
    f.context().comments().peek().is_some_and(|c| c.end <= bound)
}

/// Own-line comment between the key and the value forces the explicit form.
/// A comment trailing the `:` (`key: # comment`) does NOT — it leads the value.
fn has_own_line_comment_before_value(item: &MappingItem<'_>, f: &YamlFormatter<'_, '_>) -> bool {
    let Some(value) = &item.value.content else { return false };
    let bound = content_start(value);
    let Some(span) = f.context().comments().peek() else { return false };
    if span.end > bound {
        return false;
    }
    let source = f.context().source_text();
    // Same-line-after-key comments are trailing, not leading-of-value.
    classify_gap(source.bytes_range(item.key.span.end, span.start)) != Gap::None
        && is_own_line(&source, span.start)
}
