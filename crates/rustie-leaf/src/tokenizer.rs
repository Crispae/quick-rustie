//! Tantivy tokenizer over [`rustie_schema::slot_terms`], registered with the Quickwit fork's
//! tokenizer-registration hook (`quickwit_extensions::register_tokenizer`).
//!
//! Quickwit's own doc-mapping tokenizers are a closed list (simple, ngram, regex, source_code);
//! its regex tokenizer always numbers positions 0, 1, 2, … in match order, so it can never place
//! several terms at one token position. `SlotTokenizer` sets `Token::position` itself: a
//! pipe-joined slot's index is its position, and (in `multi` mode) every comma-joined label
//! within a slot lands at that same position. See [`rustie_schema::slot_terms`] for the format.

use rustie_schema::slot_terms;
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};

/// Registered as `rustie_tokens` (`multi = false`, one term per slot) for token fields (`word`,
/// `lemma`, …) and as `rustie_edges` (`multi = true`, several terms per slot) for the edge-label
/// fields (`incoming_edges`, `outgoing_edges`).
#[derive(Clone)]
pub(crate) struct SlotTokenizer {
    multi: bool,
}

impl SlotTokenizer {
    pub(crate) fn new(multi: bool) -> Self {
        Self { multi }
    }
}

pub(crate) struct SlotTokenStream {
    tokens: Vec<Token>,
    next: usize,
}

impl TokenStream for SlotTokenStream {
    fn advance(&mut self) -> bool {
        self.next += 1;
        self.next <= self.tokens.len()
    }

    fn token(&self) -> &Token {
        &self.tokens[self.next - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.next - 1]
    }
}

impl Tokenizer for SlotTokenizer {
    type TokenStream<'a> = SlotTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> SlotTokenStream {
        // `slot_terms` positions are slot indices, not byte offsets; tantivy's Token also wants
        // offsets (used for highlighting, not matching), so give each term its own slot's byte
        // span. That's an approximation when a slot holds several comma-joined labels (all its
        // labels share the slot's span), which is fine: nothing in this codebase highlights
        // edge-label terms.
        let tokens = slot_terms(text, self.multi)
            .into_iter()
            .map(|(position, term)| Token {
                offset_from: 0,
                offset_to: term.len(),
                position,
                text: term,
                position_length: 1,
            })
            .collect();
        SlotTokenStream { tokens, next: 0 }
    }
}

/// Build the `TextAnalyzer` registered as `rustie_tokens` / `rustie_edges` (see
/// [`crate::register`]). No filters: the encoded strings already carry exactly the terms to
/// index (see `rustie_schema::encode_tokens` / `encode_edges`).
pub(crate) fn slot_text_analyzer(multi: bool) -> TextAnalyzer {
    TextAnalyzer::from(SlotTokenizer::new(multi))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str, multi: bool) -> Vec<(usize, String)> {
        let mut analyzer = slot_text_analyzer(multi);
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while let Some(tok) = stream.next() {
            out.push((tok.position, tok.text.clone()));
        }
        out
    }

    #[test]
    fn single_value_one_term_per_slot() {
        let encoded = rustie_schema::encode_tokens(&[
            "The".to_string(),
            "".to_string(),
            "cat".to_string(),
        ]);
        assert_eq!(
            tokens(&encoded, false),
            vec![(0, "The".to_string()), (2, "cat".to_string())]
        );
    }

    #[test]
    fn multi_value_several_terms_at_one_position() {
        let encoded = rustie_schema::encode_edges(&[
            vec!["det".to_string()],
            vec![],
            vec!["nsubj".to_string(), "advmod".to_string()],
        ]);
        let toks = tokens(&encoded, true);
        assert_eq!(toks[0], (0, "det".to_string()));
        assert_eq!(
            toks[1..].iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            vec![2, 2]
        );
    }
}
