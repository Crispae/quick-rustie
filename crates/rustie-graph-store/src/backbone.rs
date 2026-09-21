//! Backbone tree selection: one incoming edge per dependent, acyclic.

use std::collections::BTreeSet;

/// Sentinel stored in the backbone `head` column (all-ones of `head_w`).
pub const ROOT: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackboneSplit {
    pub head: Vec<u32>,
    pub rel: Vec<String>,
    pub overlay: Vec<(u32, u32, String)>,
}

/// Split deduplicated edge set `E` into an acyclic backbone plus overlay.
///
/// For each dependent `d`, choose among incoming edges: (1) the governor equal
/// to the basic head, if supplied; (2) otherwise the closest governor that does
/// not create a cycle; (3) otherwise ROOT. Every other edge, including
/// self-loops, goes to the overlay.
pub fn split_backbone(
    n_tokens: usize,
    edges: &[(u32, u32, String)],
    basic_heads: Option<&[u32]>,
) -> BackboneSplit {
    let mut uniq: BTreeSet<(u32, u32, String)> = BTreeSet::new();
    for (g, d, r) in edges {
        uniq.insert((*g, *d, r.clone()));
    }
    let edges: Vec<(u32, u32, String)> = uniq.into_iter().collect();

    let mut incoming: Vec<Vec<(u32, String)>> = vec![Vec::new(); n_tokens];
    for (g, d, r) in &edges {
        if (*d as usize) < n_tokens {
            incoming[*d as usize].push((*g, r.clone()));
        }
    }
    for list in &mut incoming {
        list.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    }

    let mut head = vec![ROOT; n_tokens];
    let mut rel = vec![String::new(); n_tokens];
    let mut chosen: Vec<Option<(u32, String)>> = vec![None; n_tokens];

    for d in 0..n_tokens {
        let incoming_d = &incoming[d];
        let pick = pick_backbone(d as u32, incoming_d, basic_heads, &head);
        if let Some((g, r)) = pick {
            head[d] = g;
            rel[d] = r.clone();
            chosen[d] = Some((g, r));
        }
    }

    let mut overlay = Vec::new();
    for (g, d, r) in &edges {
        let d_us = *d as usize;
        let keep_backbone = chosen
            .get(d_us)
            .and_then(|c| c.as_ref())
            .is_some_and(|(cg, cr)| *cg == *g && cr == r);
        if !keep_backbone {
            overlay.push((*g, *d, r.clone()));
        }
    }
    overlay.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)).then(a.2.cmp(&b.2)));

    BackboneSplit { head, rel, overlay }
}

fn pick_backbone(
    d: u32,
    incoming: &[(u32, String)],
    basic_heads: Option<&[u32]>,
    head: &[u32],
) -> Option<(u32, String)> {
    if let Some(basic) = basic_heads {
        let bh = basic.get(d as usize).copied().unwrap_or(ROOT);
        if bh != ROOT && bh != d {
            if let Some((g, r)) = incoming.iter().find(|(g, _)| *g == bh) {
                if !creates_cycle(head, d, *g) {
                    return Some((*g, r.clone()));
                }
            }
        }
    }

    let mut cands: Vec<(u32, String)> = incoming.iter().filter(|(g, _)| *g != d).cloned().collect();
    cands.sort_by_key(|(g, _)| {
        let dist = (*g as i64 - d as i64).unsigned_abs();
        (dist, *g)
    });
    for (g, r) in cands {
        if !creates_cycle(head, d, g) {
            return Some((g, r));
        }
    }
    None
}

fn creates_cycle(head: &[u32], d: u32, g: u32) -> bool {
    if g == d {
        return true;
    }
    let n = head.len() as u32;
    let mut cur = g;
    let mut guard = 0u32;
    while cur != ROOT && cur < n && guard <= n {
        if cur == d {
            return true;
        }
        cur = head[cur as usize];
        guard += 1;
    }
    false
}

pub fn backbone_is_acyclic(head: &[u32]) -> bool {
    let n = head.len();
    for d in 0..n {
        let mut seen = vec![false; n];
        let mut cur = d as u32;
        let mut steps = 0;
        while cur != ROOT && (cur as usize) < n && steps <= n {
            if seen[cur as usize] {
                return false;
            }
            seen[cur as usize] = true;
            cur = head[cur as usize];
            steps += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_all_backbone() {
        let edges = vec![(1, 0, "nsubj".into()), (1, 2, "dobj".into())];
        let split = split_backbone(3, &edges, None);
        assert_eq!(split.head, vec![1, ROOT, 1]);
        assert!(split.overlay.is_empty());
        assert!(backbone_is_acyclic(&split.head));
    }

    #[test]
    fn self_loop_goes_to_overlay() {
        let edges = vec![(0, 0, "ref".into()), (1, 0, "nsubj".into())];
        let split = split_backbone(2, &edges, None);
        assert_eq!(split.head[0], 1);
        assert_eq!(split.overlay, vec![(0, 0, "ref".into())]);
    }

    #[test]
    fn cycle_broken_to_overlay() {
        let edges = vec![(1, 0, "a".into()), (0, 1, "b".into())];
        let split = split_backbone(2, &edges, None);
        assert!(backbone_is_acyclic(&split.head));
        assert_eq!(split.overlay.len(), 1);
        let union: BTreeSet<_> = split
            .overlay
            .iter()
            .cloned()
            .chain((0..2).filter_map(|d| {
                if split.head[d] == ROOT {
                    None
                } else {
                    Some((split.head[d], d as u32, split.rel[d].clone()))
                }
            }))
            .collect();
        assert_eq!(union.len(), 2);
    }

    #[test]
    fn prefers_basic_head() {
        let edges = vec![(2, 0, "nsubj".into()), (1, 0, "nsubj".into())];
        let basic = vec![2, ROOT, ROOT];
        let split = split_backbone(3, &edges, Some(&basic));
        assert_eq!(split.head[0], 2);
        assert_eq!(split.overlay, vec![(1, 0, "nsubj".into())]);
    }

    #[test]
    fn duplicates_dropped() {
        let edges = vec![(1, 0, "nsubj".into()), (1, 0, "nsubj".into())];
        let split = split_backbone(2, &edges, None);
        assert_eq!(split.head[0], 1);
        assert!(split.overlay.is_empty());
    }
}
