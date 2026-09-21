//! Full-match label / attribute set compilation over a segment dictionary.

use regex::Regex;

/// Bitset over a segment relation dictionary. Id membership is O(1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelSet {
    bits: Vec<u64>,
    pub any: bool,
}

impl LabelSet {
    pub fn empty(dict_len: usize) -> Self {
        Self {
            bits: vec![0; words(dict_len)],
            any: false,
        }
    }

    pub fn all(dict_len: usize) -> Self {
        let mut s = Self::empty(dict_len);
        s.any = true;
        for id in 0..dict_len {
            s.insert(id);
        }
        s
    }

    pub fn insert(&mut self, id: usize) {
        if id / 64 < self.bits.len() {
            self.bits[id / 64] |= 1u64 << (id % 64);
        }
    }

    pub fn contains(&self, id: usize) -> bool {
        if self.any {
            return true;
        }
        self.bits
            .get(id / 64)
            .is_some_and(|w| w & (1u64 << (id % 64)) != 0)
    }

    pub fn is_empty(&self) -> bool {
        !self.any && self.bits.iter().all(|w| *w == 0)
    }

    /// Compile exact / regex / alternation / wildcard against `dict` (full match).
    pub fn compile(dict: &[String], pred: &crate::graph::nfa::LabelPred) -> Self {
        match pred {
            crate::graph::nfa::LabelPred::Any => Self::all(dict.len()),
            crate::graph::nfa::LabelPred::Exact(s) => {
                let mut set = Self::empty(dict.len());
                if let Some(id) = dict.iter().position(|d| d == s) {
                    set.insert(id);
                }
                set
            }
            crate::graph::nfa::LabelPred::Alt(labels) => {
                let mut set = Self::empty(dict.len());
                for s in labels {
                    if let Some(id) = dict.iter().position(|d| d == s) {
                        set.insert(id);
                    }
                }
                set
            }
            crate::graph::nfa::LabelPred::Regex(re) => {
                let mut set = Self::empty(dict.len());
                for (id, s) in dict.iter().enumerate() {
                    if full_match(re, s) {
                        set.insert(id);
                    }
                }
                set
            }
        }
    }
}

/// Bitset over a colocated attribute dictionary (id 0 = NONE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrSet {
    bits: Vec<u64>,
}

impl AttrSet {
    pub fn empty(dict_len: usize) -> Self {
        Self {
            bits: vec![0; words(dict_len)],
        }
    }

    pub fn insert(&mut self, id: usize) {
        if id / 64 < self.bits.len() {
            self.bits[id / 64] |= 1u64 << (id % 64);
        }
    }

    pub fn contains(&self, id: usize) -> bool {
        self.bits
            .get(id / 64)
            .is_some_and(|w| w & (1u64 << (id % 64)) != 0)
    }

    pub fn from_exact(dict: &[String], value: &str) -> Self {
        let mut set = Self::empty(dict.len());
        if let Some(id) = dict.iter().position(|d| d == value) {
            set.insert(id);
        }
        set
    }

    pub fn from_regex(dict: &[String], re: &Regex) -> Self {
        let mut set = Self::empty(dict.len());
        for (id, s) in dict.iter().enumerate() {
            if full_match(re, s) {
                set.insert(id);
            }
        }
        set
    }

    pub fn from_wildcard(dict: &[String]) -> Self {
        let mut set = Self::empty(dict.len());
        for id in 0..dict.len() {
            set.insert(id);
        }
        set
    }
}

pub use crate::matching::node_test::{anchor, full_match};

fn words(n: usize) -> usize {
    n.div_ceil(64).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::nfa::LabelPred;

    #[test]
    fn anchored_alternation_is_a_full_match() {
        let re = anchor(&Regex::new("NN|NNS").unwrap());
        assert!(full_match(&re, "NN"));
        assert!(full_match(&re, "NNS"));
        assert!(!full_match(&re, "NNP"));
    }

    #[test]
    fn exact_and_regex_full_match() {
        let dict = vec!["nsubj".into(), "nsubjpass".into(), "dobj".into()];
        let exact = LabelSet::compile(&dict, &LabelPred::Exact("nsubj".into()));
        assert!(exact.contains(0));
        assert!(!exact.contains(1));
        let re = Regex::new("nsubj.*").unwrap();
        let set = LabelSet::compile(&dict, &LabelPred::Regex(re));
        assert!(set.contains(0));
        assert!(set.contains(1));
        assert!(!set.contains(2));
        let unanchored = Regex::new("subj").unwrap();
        let set = LabelSet::compile(&dict, &LabelPred::Regex(unanchored));
        assert!(
            !set.contains(0),
            "full-match: 'subj' must not match 'nsubj'"
        );
    }
}
