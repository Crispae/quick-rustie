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
    let encoded =
        rustie_schema::encode_tokens(&["The".to_string(), "".to_string(), "cat".to_string()]);
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
