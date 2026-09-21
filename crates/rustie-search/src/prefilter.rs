//! [`CandidateFilter`] → Quickwit [`QueryAst`], decided entirely at plan time.
//!
//! Built structurally instead of via query-string syntax, so terms need no quoting/escaping
//! and match verbatim (the `pipe_tokens` tokenizer does not lowercase, and Odinson matching is
//! case-sensitive).
//!
//! A prefilter must be a **superset** of the real matches; the exact in-memory matcher removes
//! the false positives. That makes dropping a clause always safe (it only costs selectivity),
//! which is what happens to regexes the index engine cannot run — see [`plan`].

use quickwit_query::MatchAllOrNone;
use quickwit_query::query_ast::{
    BoolQuery, FullTextMode, FullTextParams, FullTextQuery, QueryAst, RegexQuery, TermQuery,
};
use rustie_compiler::CandidateFilter;

/// A ready-to-run prefilter and how much of the original filter had to be dropped.
#[derive(Debug)]
pub struct PrefilterPlan {
    pub ast: QueryAst,
    /// Regex clauses dropped because the index engine cannot execute them.
    pub relaxed_clauses: usize,
}

/// Lower `filter` to a Quickwit query.
pub fn plan(filter: &CandidateFilter) -> PrefilterPlan {
    let mut relaxed_clauses = 0;
    let executable = lower_regexes(filter, &mut relaxed_clauses).normalize();
    PrefilterPlan {
        ast: to_query_ast(&executable),
        relaxed_clauses,
    }
}

/// Rewrite regex clauses into a form Quickwit can run, or drop them to `All`.
///
/// Quickwit evaluates term regexes with the `tantivy-fst` engine, which is a strict subset of
/// the `regex` crate that the exact matcher uses (no look-around, `\b`, …). Whole-term
/// matching is implicit there, so leading `^` / trailing `$` are redundant and stripped.
/// Validating with the same engine here means an unsupported regex is known at plan time —
/// no failed request and retry.
fn lower_regexes(filter: &CandidateFilter, relaxed: &mut usize) -> CandidateFilter {
    match filter {
        CandidateFilter::Regex { field, pattern } => match fst_regex(pattern) {
            Some(pattern) => CandidateFilter::Regex {
                field: field.clone(),
                pattern,
            },
            None => {
                *relaxed += 1;
                CandidateFilter::All
            }
        },
        CandidateFilter::And(parts) => {
            CandidateFilter::And(parts.iter().map(|p| lower_regexes(p, relaxed)).collect())
        }
        CandidateFilter::Or(parts) => {
            CandidateFilter::Or(parts.iter().map(|p| lower_regexes(p, relaxed)).collect())
        }
        other => other.clone(),
    }
}

/// `pattern` as an FST-executable regex, if it is one.
fn fst_regex(pattern: &str) -> Option<String> {
    let mut p = pattern;
    p = p.strip_prefix('^').unwrap_or(p);
    if p.ends_with('$') && !p.ends_with("\\$") {
        p = &p[..p.len() - 1];
    }
    tantivy_fst::Regex::new(p).ok().map(|_| p.to_string())
}

fn to_query_ast(filter: &CandidateFilter) -> QueryAst {
    match filter {
        CandidateFilter::All => QueryAst::MatchAll,
        CandidateFilter::Term { field, value } => QueryAst::Term(TermQuery {
            field: field.clone(),
            value: value.clone(),
        }),
        CandidateFilter::Regex { field, pattern } => QueryAst::Regex(RegexQuery {
            field: field.clone(),
            regex: pattern.clone(),
        }),
        // Tokens never contain `|`, and `pipe_tokens` splits on it, so joining with `|`
        // re-tokenizes to exactly `terms`, at consecutive positions.
        CandidateFilter::Phrase { field, terms } => QueryAst::FullText(FullTextQuery {
            field: field.clone(),
            text: terms.join("|"),
            params: FullTextParams {
                tokenizer: None,
                mode: FullTextMode::Phrase { slop: 0 },
                zero_terms_query: MatchAllOrNone::MatchNone,
            },
            lenient: false,
        }),
        CandidateFilter::And(parts) => QueryAst::Bool(BoolQuery {
            must: parts.iter().map(to_query_ast).collect(),
            ..Default::default()
        }),
        CandidateFilter::Or(parts) => QueryAst::Bool(BoolQuery {
            should: parts.iter().map(to_query_ast).collect(),
            minimum_should_match: Some(1),
            ..Default::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(filter: &CandidateFilter) -> (serde_json::Value, usize) {
        let plan = plan(filter);
        (
            serde_json::to_value(plan.ast).unwrap(),
            plan.relaxed_clauses,
        )
    }

    #[test]
    fn and_or_terms_map_to_bool_queries() {
        let (ast, relaxed) = json(&CandidateFilter::And(vec![
            CandidateFilter::term("word", "John"),
            CandidateFilter::Or(vec![
                CandidateFilter::term("pos", "VBZ"),
                CandidateFilter::term("pos", "VBD"),
            ]),
        ]));
        assert_eq!(relaxed, 0);
        assert_eq!(ast["type"], "bool");
        assert_eq!(ast["must"][0]["value"], "John");
        assert_eq!(ast["must"][1]["minimum_should_match"], 1);
    }

    #[test]
    fn phrase_uses_positions_with_zero_slop() {
        let (ast, _) = json(&CandidateFilter::phrase(
            "word",
            vec!["the".into(), "cat".into()],
        ));
        assert_eq!(ast["type"], "full_text");
        assert_eq!(ast["text"], "the|cat");
        assert_eq!(ast["params"]["mode"]["type"], "phrase");
    }

    #[test]
    fn anchors_are_stripped_and_unsupported_regexes_dropped_at_plan_time() {
        // `^…$` is redundant for whole-term matching but not valid FST syntax.
        let (ast, relaxed) = json(&CandidateFilter::regex("word", "^[Cc]ancer.*$"));
        assert_eq!((relaxed, ast["regex"].as_str()), (0, Some("[Cc]ancer.*")));

        // Look-around is beyond the FST engine: the clause is dropped, others survive.
        let (ast, relaxed) = json(&CandidateFilter::And(vec![
            CandidateFilter::term("word", "John"),
            CandidateFilter::regex("lemma", r"(?=x)y"),
        ]));
        assert_eq!(relaxed, 1);
        assert_eq!(ast["type"], "term");

        // An OR with a dropped branch can match anything.
        let (ast, relaxed) = json(&CandidateFilter::Or(vec![
            CandidateFilter::term("word", "a"),
            CandidateFilter::regex("word", r"\bb"),
        ]));
        assert_eq!((relaxed, ast["type"].as_str()), (1, Some("match_all")));
    }
}
