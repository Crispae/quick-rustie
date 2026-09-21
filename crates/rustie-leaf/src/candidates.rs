//! Candidate documents of one segment: the compiler's [`CandidateFilter`] (a superset of the
//! matching sentences) evaluated on postings.

use rustie_compiler::matching::node_test::anchor;
use rustie_compiler::{CandidateFilter, LeafTest};
use tantivy::schema::{IndexRecordOption, Schema};
use tantivy::{DocId, DocSet, SegmentReader, TERMINATED};

use crate::expand::expand_terms;

/// A set of document ids of one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocBits {
    words: Vec<u64>,
    max_doc: u32,
}

impl DocBits {
    fn empty(max_doc: u32) -> Self {
        Self {
            words: vec![0; (max_doc as usize).div_ceil(64)],
            max_doc,
        }
    }

    fn full(max_doc: u32) -> Self {
        let mut bits = Self::empty(max_doc);
        for doc in 0..max_doc {
            bits.insert(doc);
        }
        bits
    }

    fn insert(&mut self, doc: DocId) {
        self.words[doc as usize / 64] |= 1 << (doc % 64);
    }

    fn and(&mut self, other: &Self) {
        self.words
            .iter_mut()
            .zip(&other.words)
            .for_each(|(a, b)| *a &= b);
    }

    fn or(&mut self, other: &Self) {
        self.words
            .iter_mut()
            .zip(&other.words)
            .for_each(|(a, b)| *a |= b);
    }

    pub(crate) fn docs(&self) -> Vec<DocId> {
        let mut out = Vec::new();
        for (w, &word) in self.words.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let bit = bits.trailing_zeros();
                out.push(w as u32 * 64 + bit);
                bits &= bits - 1;
            }
        }
        out.retain(|&doc| doc < self.max_doc);
        out
    }
}

/// Documents of `reader` that `filter` admits. Warms (and reads) the postings it needs.
pub(crate) fn candidate_docs<'a>(
    filter: &'a CandidateFilter,
    reader: &'a SegmentReader,
    schema: &'a Schema,
) -> futures::future::BoxFuture<'a, anyhow::Result<DocBits>> {
    Box::pin(async move {
        let max_doc = reader.max_doc();
        Ok(match filter {
            CandidateFilter::All => DocBits::full(max_doc),
            CandidateFilter::Term { field, value } => {
                docs_of_test(field, &LeafTest::Exact(value.clone()), reader, schema).await?
            }
            CandidateFilter::Regex { field, pattern } => {
                let regex = regex::Regex::new(pattern)
                    .map_err(|err| anyhow::anyhow!("invalid regex `{pattern}`: {err}"))?;
                docs_of_test(field, &LeafTest::Regex(anchor(&regex)), reader, schema).await?
            }
            // Adjacency is verified by the exact matcher; here every term must be present.
            CandidateFilter::Phrase { field, terms } => {
                let mut acc = DocBits::full(max_doc);
                for value in terms {
                    let docs = docs_of_test(field, &LeafTest::Exact(value.clone()), reader, schema)
                        .await?;
                    acc.and(&docs);
                }
                acc
            }
            CandidateFilter::And(parts) => {
                let mut acc = DocBits::full(max_doc);
                for part in parts {
                    acc.and(&candidate_docs(part, reader, schema).await?);
                }
                acc
            }
            CandidateFilter::Or(parts) => {
                let mut acc = DocBits::empty(max_doc);
                for part in parts {
                    acc.or(&candidate_docs(part, reader, schema).await?);
                }
                acc
            }
        })
    })
}

async fn docs_of_test(
    field_name: &str,
    test: &LeafTest,
    reader: &SegmentReader,
    schema: &Schema,
) -> anyhow::Result<DocBits> {
    let mut bits = DocBits::empty(reader.max_doc());
    // A field the split does not have holds no term: nothing matches.
    let Ok(field) = schema.get_field(field_name) else {
        return Ok(bits);
    };
    if !schema.get_field_entry(field).is_indexed() {
        return Ok(bits);
    }
    let inverted_index = reader.inverted_index(field)?;
    for term in expand_terms(&inverted_index, field, test, false).await? {
        if let Some(mut postings) = inverted_index.read_postings(&term, IndexRecordOption::Basic)? {
            while postings.doc() != TERMINATED {
                bits.insert(postings.doc());
                postings.advance();
            }
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_operations() {
        let mut a = DocBits::empty(130);
        for d in [0, 64, 129] {
            a.insert(d);
        }
        let mut b = DocBits::empty(130);
        for d in [64, 100] {
            b.insert(d);
        }
        let mut and = a.clone();
        and.and(&b);
        assert_eq!(and.docs(), [64]);
        let mut or = a.clone();
        or.or(&b);
        assert_eq!(or.docs(), [0, 64, 100, 129]);
        assert_eq!(DocBits::full(3).docs(), [0, 1, 2]);
    }
}
