//! Nullable / First / Last derivation for hop NFAs (prefilter requirements).

use crate::graph::nfa::{Dir, HopNfa, LabelPred};

/// Edge-set requirements at the endpoints of one hop segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HopRequirements {
    pub nullable: bool,
    pub first: Vec<(Dir, LabelPred)>,
    pub last: Vec<(Dir, LabelPred)>,
}

/// Derive First / Last / nullable from a compiled hop NFA.
pub fn derive(nfa: &HopNfa) -> HopRequirements {
    HopRequirements {
        nullable: nfa.nullable,
        first: nfa.first_preds(),
        last: nfa.last_preds(),
    }
}

/// True when First/Last cannot miss a required endpoint edge (no `Any`, not nullable).
pub fn first_is_strict(req: &HopRequirements) -> bool {
    !req.nullable && !req.first.is_empty() && !req.first.iter().any(|(_, p)| p.is_any())
}

pub fn last_is_strict(req: &HopRequirements) -> bool {
    !req.nullable && !req.last.is_empty() && !req.last.iter().any(|(_, p)| p.is_any())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::nfa::{HopNfa, LabelPred};

    #[test]
    fn optional_is_nullable() {
        let nfa = HopNfa::atom(Dir::Out, LabelPred::Exact("nsubj".into())).optional();
        let req = derive(&nfa);
        assert!(req.nullable);
        assert!(!first_is_strict(&req));
    }

    #[test]
    fn exact_hop_is_strict() {
        let nfa = HopNfa::atom(Dir::Out, LabelPred::Exact("dobj".into()));
        let req = derive(&nfa);
        assert!(!req.nullable);
        assert!(first_is_strict(&req));
        assert!(last_is_strict(&req));
    }
}
