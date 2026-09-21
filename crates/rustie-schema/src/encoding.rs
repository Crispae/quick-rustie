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

/// Encode for Quickwit `pipe_tokens` regex tokenizer: empty → [`EMPTY_TOKEN_SENTINEL`].
///
/// Tokens containing `|` are escaped for storage but the stock regex tokenizer
/// will not unescape them; prefer validating absence of `|` at flatten time.
pub fn encode_tokens_for_quickwit(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|s| {
            if s.is_empty() {
                EMPTY_TOKEN_SENTINEL.to_string()
            } else {
                escape_join_field(s)
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Decode Quickwit-stored pipe string, mapping the empty sentinel back to `""`.
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

/// Encode for Quickwit `pipe_tokens`: empty positions → [`EMPTY_TOKEN_SENTINEL`].
///
/// Multi-label slots stay comma-joined (`dobj,prep`) as one regex token under stock
/// Quickwit; prefer stored [`crate::SentenceGraph`] JSON for exact multi-edge work.
pub fn encode_edges_for_quickwit(edges_per_position: &[Vec<String>]) -> String {
    edges_per_position
        .iter()
        .map(|labels| {
            if labels.is_empty() {
                EMPTY_TOKEN_SENTINEL.to_string()
            } else {
                labels
                    .iter()
                    .map(|s| escape_join_field(s))
                    .collect::<Vec<_>>()
                    .join(",")
            }
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// Decode Quickwit edge encoding, mapping empty sentinel → empty label list.
pub fn decode_edges_from_quickwit(encoded: &str) -> Vec<Vec<String>> {
    decode_edges(encoded)
        .into_iter()
        .map(|labels| {
            if labels.len() == 1 && labels[0] == EMPTY_TOKEN_SENTINEL {
                Vec::new()
            } else {
                labels
                    .into_iter()
                    .filter(|l| l != EMPTY_TOKEN_SENTINEL)
                    .collect()
            }
        })
        .collect()
}

/// Encode the *set* of edge labels present in a sentence for the Quickwit postings fields
/// (`outgoing_edges` / `incoming_edges`): distinct labels, sorted, joined with `|`.
///
/// Why a set and not [`encode_edges_for_quickwit`]: Quickwit's tokenizers cannot emit several
/// tokens at one position (RustIE's custom `edge_positions` tokenizer can), so a per-token
/// encoding would either lose labels (comma-joined slots become one opaque term) or misalign
/// positions. What a Quickwit prefilter can actually use is document-level membership, and
/// one term per label gives exactly that. Per-token structure lives in the stored graph JSON.
///
/// Returns `None` when there are no labels (the field is then omitted).
pub fn encode_edge_label_set(edges_per_position: &[Vec<String>]) -> Option<String> {
    let labels: std::collections::BTreeSet<&str> = edges_per_position
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    if labels.is_empty() {
        None
    } else {
        Some(labels.into_iter().collect::<Vec<_>>().join("|"))
    }
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
    fn edge_label_set_is_distinct_sorted_and_optional() {
        let edges = vec![
            vec![],
            vec!["nsubj".to_string(), "advmod".to_string()],
            vec!["nsubj".to_string()],
        ];
        assert_eq!(
            encode_edge_label_set(&edges).as_deref(),
            Some("advmod|nsubj")
        );
        assert_eq!(encode_edge_label_set(&[vec![], vec![]]), None);
    }

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
    fn quickwit_empty_sentinel_roundtrip() {
        let tokens = vec!["a".into(), "".into(), "c".into()];
        let encoded = encode_tokens_for_quickwit(&tokens);
        assert!(encoded.contains(EMPTY_TOKEN_SENTINEL));
        assert_eq!(decode_tokens_from_quickwit(&encoded), tokens);
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
    fn quickwit_edge_empty_sentinel() {
        let edges = vec![vec![], vec!["nsubj".into()], vec![]];
        let enc = encode_edges_for_quickwit(&edges);
        assert!(enc.contains(EMPTY_TOKEN_SENTINEL));
        assert_eq!(decode_edges_from_quickwit(&enc), edges);
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
}
