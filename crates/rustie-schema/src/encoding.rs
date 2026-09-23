//! Pipe-join encoding so Tantivy/Quickwit positions match linguistic token indices.
//!
//! Compatible with RustIE `encode_position_aware_tokens` / escape rules.

/// Placeholder for an empty token when emitting Quickwit JSON.
///
/// Quickwit's stock regex tokenizer (`[^|]+`) cannot emit empty tokens; this
/// private-use codepoint keeps position slots aligned.
pub const EMPTY_TOKEN_SENTINEL: &str = "\u{E000}";

/// Escape `|`, `,`, and `\` so they survive position-aware joins.
pub fn escape_join_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || c == '|' || c == ',' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Undo [`escape_join_field`].
pub fn unescape_join_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split `s` on `sep`, honoring backslash escapes, keeping the backslash in the piece.
pub fn split_escaped_raw(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            cur.push('\\');
            if let Some(n) = chars.next() {
                cur.push(n);
            }
        } else if c == sep {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

/// One `(position, term)` pair a position-aware tokenizer emits from a pipe-joined string, as
/// produced by [`encode_tokens`] / [`encode_edges`].
///
/// - Split on unescaped `|`; the slot's index becomes its position, so a field's position always
///   equals its linguistic token index.
/// - When `multi`, each slot is further split on unescaped `,` (several terms at one position,
///   e.g. edge labels).
/// - An empty piece contributes no term, but the position still advances: this is what keeps an
///   empty slot from shifting every later token's position.
/// - Within one slot, duplicate terms are removed: postings only need presence, not count.
///
/// Pure logic (no tantivy dependency) so it can be unit tested here; the tantivy `Tokenizer` that
/// wraps it lives in `rustie-leaf` (the crate that depends on tantivy).
pub fn slot_terms(text: &str, multi: bool) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (position, slot) in split_escaped_raw(text, '|').iter().enumerate() {
        if multi {
            let mut seen = std::collections::BTreeSet::new();
            for piece in split_escaped_raw(slot, ',') {
                let term = unescape_join_field(&piece);
                if !term.is_empty() && seen.insert(term.clone()) {
                    out.push((position, term));
                }
            }
        } else {
            let term = unescape_join_field(slot);
            if !term.is_empty() {
                out.push((position, term));
            }
        }
    }
    out
}

/// Encode tokens into `a|b|c` form (RustIE-compatible, preserves empty slots).
pub fn encode_tokens(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|s| escape_join_field(s))
        .collect::<Vec<_>>()
        .join("|")
}

/// Decode a pipe-joined string produced by [`encode_tokens`].
pub fn decode_tokens(encoded: &str) -> Vec<String> {
    if encoded.is_empty() {
        return Vec::new();
    }
    split_escaped_raw(encoded, '|')
        .into_iter()
        .map(|p| unescape_join_field(&p))
        .collect()
}

/// Decode Quickwit-stored pipe string, mapping the legacy empty sentinel back to `""`.
///
/// A no-op beyond [`decode_tokens`] for anything [`encode_tokens`] produced (the current
/// encoding, read by `rustie_tokens`): the sentinel only ever appears in a split indexed
/// before the `rustie_tokens`/`rustie_edges` tokenizers, back when Quickwit's stock regex
/// tokenizer (`[^|]+`) could not match an empty slot and needed a placeholder to keep
/// positions aligned. Kept so those splits still render correctly.
pub fn decode_tokens_from_quickwit(encoded: &str) -> Vec<String> {
    decode_tokens(encoded)
        .into_iter()
        .map(|t| {
            if t == EMPTY_TOKEN_SENTINEL {
                String::new()
            } else {
                t
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Edge-label encodings (incoming_edges / outgoing_edges)
// ---------------------------------------------------------------------------

/// Encode edge labels per token position (RustIE `encode_position_aware_edges`).
///
/// Format: `|nsubj||dobj,prep|` — `|` separates token slots, `,` separates labels
/// at the same slot.
pub fn encode_edges(edges_per_position: &[Vec<String>]) -> String {
    edges_per_position
        .iter()
        .map(|labels| {
            labels
                .iter()
                .map(|s| escape_join_field(s))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Decode [`encode_edges`] output.
pub fn decode_edges(encoded: &str) -> Vec<Vec<String>> {
    if encoded.is_empty() {
        return Vec::new();
    }
    split_escaped_raw(encoded, '|')
        .into_iter()
        .map(|pos| {
            if pos.is_empty() {
                Vec::new()
            } else {
                split_escaped_raw(&pos, ',')
                    .into_iter()
                    .map(|s| unescape_join_field(&s))
                    .collect()
            }
        })
        .collect()
}

/// Build outgoing / incoming label lists from `(from, to, rel)` edges.
pub fn labels_by_direction(
    num_tokens: usize,
    edges: &[(u32, u32, String)],
) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let mut outgoing = vec![Vec::new(); num_tokens];
    let mut incoming = vec![Vec::new(); num_tokens];
    for (from, to, rel) in edges {
        let f = *from as usize;
        let t = *to as usize;
        if f < num_tokens {
            outgoing[f].push(rel.clone());
        }
        if t < num_tokens {
            incoming[t].push(rel.clone());
        }
    }
    (outgoing, incoming)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple() {
        let tokens = vec!["John".into(), "eats".into(), "pizza".into()];
        assert_eq!(decode_tokens(&encode_tokens(&tokens)), tokens);
    }

    #[test]
    fn roundtrip_empty_slot() {
        let tokens = vec!["a".into(), "".into(), "c".into()];
        assert_eq!(decode_tokens(&encode_tokens(&tokens)), tokens);
    }

    #[test]
    fn escapes_pipe_and_comma() {
        let tokens = vec!["a|b".into(), "c,d".into(), r"e\f".into()];
        let encoded = encode_tokens(&tokens);
        assert_eq!(decode_tokens(&encoded), tokens);
    }

    #[test]
    fn legacy_quickwit_empty_sentinel_still_decodes() {
        // A split indexed before `rustie_tokens` stored this sentinel for an empty slot.
        let legacy = format!("a|{EMPTY_TOKEN_SENTINEL}|c");
        assert_eq!(
            decode_tokens_from_quickwit(&legacy),
            vec!["a".to_string(), "".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn edge_encode_roundtrip() {
        let edges = vec![
            vec![],
            vec!["nsubj".into()],
            vec![],
            vec!["dobj".into(), "prep".into()],
            vec![],
        ];
        assert_eq!(decode_edges(&encode_edges(&edges)), edges);
    }

    #[test]
    fn labels_by_direction_builds_lists() {
        let edges = vec![(1u32, 0u32, "det".into()), (2u32, 1u32, "nsubj".into())];
        let (out, inc) = labels_by_direction(3, &edges);
        assert_eq!(out[1], vec!["det".to_string()]);
        assert_eq!(out[2], vec!["nsubj".to_string()]);
        assert_eq!(inc[0], vec!["det".to_string()]);
        assert_eq!(inc[1], vec!["nsubj".to_string()]);
    }

    #[test]
    fn slot_terms_single_value_skips_empty_but_advances_position() {
        let terms = slot_terms(&encode_tokens(&["The".into(), "".into(), "cat".into()]), false);
        assert_eq!(
            terms,
            vec![(0, "The".to_string()), (2, "cat".to_string())]
        );
    }

    #[test]
    fn slot_terms_multi_splits_on_comma_and_dedupes() {
        let edges = vec![
            vec!["det".into()],
            vec![],
            vec!["nsubj".into(), "advmod".into(), "nsubj".into()],
        ];
        let terms = slot_terms(&encode_edges(&edges), true);
        assert_eq!(
            terms,
            vec![
                (0, "det".to_string()),
                (2, "nsubj".to_string()),
                (2, "advmod".to_string()),
            ]
        );
    }

    #[test]
    fn slot_terms_unescapes_pipe_comma_and_backslash() {
        let tokens = vec!["a|b".into(), "c,d".into(), r"e\f".into()];
        let terms = slot_terms(&encode_tokens(&tokens), false);
        assert_eq!(
            terms,
            vec![
                (0, "a|b".to_string()),
                (1, "c,d".to_string()),
                (2, r"e\f".to_string()),
            ]
        );
    }
}
