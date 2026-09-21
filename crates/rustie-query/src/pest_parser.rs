use crate::ast::{Assertion, Constraint, Matcher, Pattern, QuantifierKind, Traversal};
use pest_derive::Parser;

#[derive(Parser)]
#[grammar = "query.pest"]
pub struct QueryParser;

/// Append `p` to a pattern chain. After a graph traversal, a trailing surface
/// atom is part of the traversal's destination span (`[a] >x [b] [c]` has the
/// destination `[b] [c]`), not a sibling of the whole traversal.
fn append_pattern(patterns: &mut Vec<Pattern>, p: Pattern, dst_is_seq: &mut bool) {
    if let Some(Pattern::GraphTraversal { dst, .. }) = patterns.last_mut() {
        if *dst_is_seq {
            if let Pattern::Concatenated(parts) = dst.as_mut() {
                parts.push(p);
                return;
            }
        }
        let old = std::mem::replace(dst.as_mut(), Pattern::Constraint(Constraint::Wildcard));
        **dst = Pattern::Concatenated(vec![old, p]);
        *dst_is_seq = true;
        return;
    }
    patterns.push(p);
}

type Pair<'a> = pest::iterators::Pair<'a, Rule>;

fn word_constraint(matcher: Matcher) -> Pattern {
    Pattern::Constraint(Constraint::Field {
        name: "word".to_string(),
        matcher,
    })
}

/// Only the first child of a rule that wraps exactly one thing.
fn only_child(pair: Pair) -> Pair {
    pair.into_inner().next().unwrap()
}

/// Build the AST for one parse-tree node, dispatching on its grammar rule.
pub fn build_ast(pair: Pair) -> Pattern {
    match pair.as_rule() {
        Rule::query | Rule::atomic_pattern | Rule::group => build_ast(only_child(pair)),
        Rule::default_field_query => build_default_field_query(pair),
        Rule::pattern_chain => build_pattern_chain(pair),
        Rule::sequence => build_sequence(pair),
        Rule::quantified_pattern => build_quantified(pair),
        Rule::bare_word => word_constraint(Matcher::String(pair.as_str().to_string())),
        Rule::named_capture => build_named_capture(pair),
        Rule::constraint => match pair.into_inner().next() {
            Some(body) => Pattern::Constraint(build_constraint(body)),
            None => Pattern::Constraint(Constraint::Wildcard),
        },
        Rule::assertion_pattern => Pattern::Assertion(build_assertion(only_child(pair))),
        _ => panic!("Unsupported pattern for now: {:?}", pair.as_rule()),
    }
}

fn build_default_field_query(pair: Pair) -> Pattern {
    let inner = only_child(pair);
    match inner.as_rule() {
        Rule::default_string => word_constraint(Matcher::String(inner.as_str().to_string())),
        Rule::default_regex => {
            let pattern = &inner.as_str()[1..inner.as_str().len() - 1];
            word_constraint(Matcher::Regex {
                pattern: pattern.to_string(),
                regex: std::sync::Arc::new(regex::Regex::new(pattern).unwrap()),
            })
        }
        _ => unreachable!(),
    }
}

/// Fold the chain so far into the source of a new traversal to `dst`.
fn attach_traversal(patterns: &mut Vec<Pattern>, traversal: Traversal, dst: Pattern) {
    let src = if patterns.len() == 1 {
        patterns.pop().unwrap()
    } else {
        Pattern::Concatenated(std::mem::take(patterns))
    };
    *patterns = vec![Pattern::GraphTraversal {
        src: Box::new(src),
        traversal,
        dst: Box::new(dst),
    }];
}

fn build_pattern_chain(pair: Pair) -> Pattern {
    let mut pairs = pair.into_inner();
    let mut patterns = vec![build_ast(pairs.next().unwrap())];
    // True once we have wrapped the destination of the trailing GraphTraversal in a
    // `Concatenated` ourselves, so later trailing atoms extend it instead of nesting
    // a user-written group.
    let mut dst_is_seq = false;

    for link in pairs {
        // A link is either a bare path/atom, or a `pattern_link` holding a path
        // with its optional destination, or an atom.
        let (item, dst_pair) = match link.as_rule() {
            Rule::pattern_link => {
                let mut inner = link.into_inner();
                let item = inner.next().unwrap();
                (item, inner.next())
            }
            _ => (link, None),
        };
        match item.as_rule() {
            Rule::graph_path => {
                let traversal = build_graph_path(item);
                let dst = dst_pair
                    .map(build_ast)
                    .unwrap_or(Pattern::Constraint(Constraint::Wildcard));
                attach_traversal(&mut patterns, traversal, dst);
                dst_is_seq = false;
            }
            Rule::quantified_pattern => {
                append_pattern(&mut patterns, build_ast(item), &mut dst_is_seq);
            }
            _ => {}
        }
    }

    if patterns.len() == 1 {
        patterns.pop().unwrap()
    } else {
        Pattern::Concatenated(patterns)
    }
}

fn build_sequence(pair: Pair) -> Pattern {
    let mut patterns = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::quantified_pattern | Rule::named_capture | Rule::sequence => {
                patterns.push(build_ast(inner));
            }
            Rule::WHITESPACE => {}
            _ => panic!("Unexpected rule in sequence: {:?}", inner.as_rule()),
        }
    }
    if patterns.len() == 1 {
        patterns.pop().unwrap()
    } else {
        Pattern::Concatenated(patterns)
    }
}

fn build_quantified(pair: Pair) -> Pattern {
    let mut pairs = pair.into_inner();
    let atomic = build_ast(pairs.next().unwrap());
    let Some(quant_pair) = pairs.next() else {
        return atomic;
    };
    // `pattern_quantifier` holds a greedy_quantifier or lazy_quantifier.
    let (min, max, is_lazy) = parse_quantifier(only_child(quant_pair));
    Pattern::Repetition {
        pattern: Box::new(atomic),
        min,
        max,
        kind: if is_lazy { QuantifierKind::Lazy } else { QuantifierKind::Greedy },
    }
}

fn build_named_capture(pair: Pair) -> Pattern {
    let mut inner = pair.into_inner();
    let name = inner.next().unwrap().as_str().to_string();
    let pattern = build_ast(inner.next().unwrap());
    Pattern::NamedCapture {
        name,
        pattern: Box::new(pattern),
    }
}

fn parse_quantifier(pair: pest::iterators::Pair<Rule>) -> (usize, Option<usize>, bool) {
    let text = pair.as_str();
    match pair.as_rule() {
        Rule::greedy_quantifier => {
            // Check if it's a simple string literal first
            match text {
                "*" => return (0, None, false),
                "+" => return (1, None, false),
                "?" => return (0, Some(1), false),
                _ => {}
            }

            // Otherwise, it should be a range_quantifier
            let mut inner_pairs = pair.into_inner();
            if let Some(inner) = inner_pairs.next() {
                if inner.as_rule() == Rule::range_quantifier {
                    let mut pairs = inner.into_inner();
                    let min_str = pairs.next();
                    let max_str = pairs.next();
                    let min = min_str.map(|p| p.as_str().parse().unwrap()).unwrap_or(0);
                    let max = max_str.and_then(|p| p.as_str().parse::<usize>().ok());
                    (min, max, false)
                } else {
                    panic!(
                        "Unexpected inner rule in greedy_quantifier: {:?}, text: {}",
                        inner.as_rule(),
                        text
                    )
                }
            } else {
                // Fallback: try to parse as range quantifier from text
                if text.starts_with('{') && text.ends_with('}') {
                    let content = &text[1..text.len() - 1];
                    let parts: Vec<&str> = content.split(',').collect();
                    let min = parts
                        .first()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    let max = parts.get(1).and_then(|s| s.trim().parse::<usize>().ok());
                    (min, max, false)
                } else {
                    panic!("Unexpected greedy quantifier: {}", text)
                }
            }
        }
        Rule::lazy_quantifier => {
            // Check if it's a simple string literal first
            match text {
                "*?" => return (0, None, true),
                "+?" => return (1, None, true),
                "??" => return (0, Some(1), true),
                _ => {}
            }

            // Otherwise, it should be a lazy_range_quantifier
            let mut inner_pairs = pair.into_inner();
            if let Some(inner) = inner_pairs.next() {
                if inner.as_rule() == Rule::lazy_range_quantifier {
                    let mut pairs = inner.into_inner();
                    let min_str = pairs.next();
                    let max_str = pairs.next();
                    let min = min_str.map(|p| p.as_str().parse().unwrap()).unwrap_or(0);
                    let max = max_str.and_then(|p| p.as_str().parse::<usize>().ok());
                    (min, max, true)
                } else {
                    panic!(
                        "Unexpected inner rule in lazy_quantifier: {:?}, text: {}",
                        inner.as_rule(),
                        text
                    )
                }
            } else {
                // Fallback: try to parse as lazy range quantifier from text
                if text.starts_with('{') && text.ends_with("}?") {
                    let content = &text[1..text.len() - 2];
                    let parts: Vec<&str> = content.split(',').collect();
                    let min = parts
                        .first()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                    let max = parts.get(1).and_then(|s| s.trim().parse::<usize>().ok());
                    (min, max, true)
                } else {
                    panic!("Unexpected lazy quantifier: {}", text)
                }
            }
        }
        _ => panic!(
            "Unexpected quantifier rule: {:?}, text: {}",
            pair.as_rule(),
            text
        ),
    }
}

fn build_constraint(pair: pest::iterators::Pair<Rule>) -> Constraint {
    match pair.as_rule() {
        Rule::constraint_body => {
            // Delegate to the inner node
            let inner = pair.into_inner().next().unwrap();
            build_constraint(inner)
        }
        Rule::constraint_expr | Rule::disjunction => {
            let mut children = pair.into_inner().map(build_constraint).collect::<Vec<_>>();
            if children.len() == 1 {
                children.pop().unwrap()
            } else {
                Constraint::Disjunctive(children)
            }
        }
        Rule::conjunction => {
            let mut children = pair.into_inner().map(build_constraint).collect::<Vec<_>>();
            if children.len() == 1 {
                children.pop().unwrap()
            } else {
                Constraint::Conjunctive(children)
            }
        }
        Rule::negated_atom => {
            let mut inner = pair.into_inner();
            let first = inner.next().unwrap();
            match first.as_rule() {
                Rule::prefix_negation => {
                    let atom = first.into_inner().next().unwrap();
                    Constraint::Negated(Box::new(build_constraint(atom)))
                }
                _ => build_constraint(first),
            }
        }
        Rule::prefix_negation => {
            let atom = pair.into_inner().next().unwrap();
            Constraint::Negated(Box::new(build_constraint(atom)))
        }
        Rule::atom => {
            let inner = pair.into_inner().next().unwrap();
            build_constraint(inner)
        }
        Rule::group_constraint => {
            let inner = pair.into_inner().next().unwrap();
            build_constraint(inner)
        }
        Rule::field_constraint => {
            let mut inner = pair.into_inner();
            let field = inner.next().unwrap().as_str().to_string();
            let op = inner.next().unwrap().as_str();
            let value_pair = inner.next().unwrap();
            let fuzzy_op = inner.next();

            let matcher = match value_pair.as_rule() {
                Rule::value => {
                    let value = value_pair.as_str().to_string();
                    if fuzzy_op.is_some() {
                        // Fuzzy matching - store as Fuzzy constraint
                        return Constraint::Fuzzy {
                            name: field,
                            matcher: value,
                        };
                    }
                    Matcher::String(value)
                }
                Rule::regex_value => {
                    let pattern = &value_pair.as_str()[1..value_pair.as_str().len() - 1];
                    Matcher::Regex {
                        pattern: pattern.to_string(),
                        regex: std::sync::Arc::new(regex::Regex::new(pattern).unwrap()),
                    }
                }
                _ => unreachable!(),
            };

            if op == "!=" {
                Constraint::Negated(Box::new(Constraint::Field {
                    name: field,
                    matcher,
                }))
            } else {
                Constraint::Field {
                    name: field,
                    matcher,
                }
            }
        }
        Rule::wildcard => Constraint::Wildcard,
        _ => panic!("Unsupported constraint: {:?}", pair.as_rule()),
    }
}

fn build_assertion(pair: pest::iterators::Pair<Rule>) -> Assertion {
    match pair.as_rule() {
        Rule::lookahead_assertion => {
            let inner = pair.into_inner().next().unwrap();
            build_assertion(inner)
        }
        Rule::lookbehind_assertion => {
            let inner = pair.into_inner().next().unwrap();
            build_assertion(inner)
        }
        Rule::positive_lookahead => {
            let inner = pair.into_inner().next().unwrap();
            Assertion::PositiveLookahead(Box::new(build_ast(inner)))
        }
        Rule::negative_lookahead => {
            let inner = pair.into_inner().next().unwrap();
            Assertion::NegativeLookahead(Box::new(build_ast(inner)))
        }
        Rule::positive_lookbehind => {
            let inner = pair.into_inner().next().unwrap();
            Assertion::PositiveLookbehind(Box::new(build_ast(inner)))
        }
        Rule::negative_lookbehind => {
            let inner = pair.into_inner().next().unwrap();
            Assertion::NegativeLookbehind(Box::new(build_ast(inner)))
        }
        _ => panic!("Unsupported assertion: {:?}", pair.as_rule()),
    }
}

fn build_graph_path(pair: pest::iterators::Pair<Rule>) -> crate::ast::Traversal {
    let steps: Vec<crate::ast::Traversal> = pair
        .into_inner()
        .map(build_traversal_step)
        .collect();
    if steps.len() == 1 {
        steps.into_iter().next().unwrap()
    } else {
        crate::ast::Traversal::Concatenated(steps)
    }
}

fn build_traversal_step(pair: pest::iterators::Pair<Rule>) -> crate::ast::Traversal {
    let mut inner = pair.into_inner();
    let op_pair = inner.next().unwrap();
    let op_inner = op_pair.into_inner().next().unwrap();
    let base = build_traversal_op(op_inner);
    if let Some(q_pair) = inner.next() {
        apply_hop_quantifier(base, q_pair.as_str())
    } else {
        base
    }
}

fn apply_hop_quantifier(
    base: crate::ast::Traversal,
    quantifier: &str,
) -> crate::ast::Traversal {
    match quantifier {
        "*" => crate::ast::Traversal::KleeneStar(Box::new(base)),
        "?" => crate::ast::Traversal::Optional(Box::new(base)),
        "+" => crate::ast::Traversal::Concatenated(vec![
            base.clone(),
            crate::ast::Traversal::KleeneStar(Box::new(base)),
        ]),
        _ => base,
    }
}

fn build_traversal_op(pair: pest::iterators::Pair<Rule>) -> crate::ast::Traversal {
    match pair.as_rule() {
        Rule::outgoing_wildcard => crate::ast::Traversal::OutgoingWildcard,
        Rule::incoming_wildcard => crate::ast::Traversal::IncomingWildcard,
        Rule::outgoing => {
            let label_pair = pair.into_inner().next().unwrap();
            build_traversal_label(label_pair, true)
        }
        Rule::incoming => {
            let label_pair = pair.into_inner().next().unwrap();
            build_traversal_label(label_pair, false)
        }
        Rule::outgoing_disjunctive => {
            let mut labels = Vec::new();
            for label_pair in pair.into_inner() {
                if label_pair.as_rule() == Rule::traversal_label {
                    labels.push(build_traversal_label(label_pair, true));
                }
            }
            if labels.len() == 1 {
                labels.pop().unwrap()
            } else {
                crate::ast::Traversal::Disjunctive(labels)
            }
        }
        Rule::incoming_disjunctive => {
            let mut labels = Vec::new();
            for label_pair in pair.into_inner() {
                if label_pair.as_rule() == Rule::traversal_label {
                    labels.push(build_traversal_label(label_pair, false));
                }
            }
            if labels.len() == 1 {
                labels.pop().unwrap()
            } else {
                crate::ast::Traversal::Disjunctive(labels)
            }
        }
        other => {
            eprintln!(
                "[DEBUG] Unexpected traversal op rule: {:?}, text: {}",
                other,
                pair.as_str()
            );
            unreachable!()
        }
    }
}

fn build_traversal_label(
    pair: pest::iterators::Pair<Rule>,
    outgoing: bool,
) -> crate::ast::Traversal {
    match pair.as_rule() {
        Rule::label => {
            let matcher = crate::ast::Matcher::String(pair.as_str().to_string());
            if outgoing {
                crate::ast::Traversal::Outgoing(matcher)
            } else {
                crate::ast::Traversal::Incoming(matcher)
            }
        }
        Rule::traversal_regex => {
            let pattern = &pair.as_str()[1..pair.as_str().len() - 1];
            let matcher = crate::ast::Matcher::Regex {
                pattern: pattern.to_string(),
                regex: std::sync::Arc::new(regex::Regex::new(pattern).unwrap()),
            };
            if outgoing {
                crate::ast::Traversal::Outgoing(matcher)
            } else {
                crate::ast::Traversal::Incoming(matcher)
            }
        }
        Rule::traversal_label => {
            let inner = pair.into_inner().next().unwrap();
            build_traversal_label(inner, outgoing)
        }
        _ => {
            eprintln!(
                "[DEBUG] Unexpected traversal label rule: {:?}, text: {}",
                pair.as_rule(),
                pair.as_str()
            );
            unreachable!()
        }
    }
}
