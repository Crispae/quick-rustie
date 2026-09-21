//! Build a GPH2 file for a split whose size is not known up front, in bounded memory.
//!
//! GPH2 dictionaries are file-global and must be final before the first block is written, but
//! a search index produces sentences one at a time. [`SpoolWriter`] therefore appends each
//! sentence's compact record to an anonymous temporary file while counting label frequencies
//! (which only need the running counters, not the records), and [`finish`](SpoolWriter::finish)
//! replays the spool once to emit the blocks. Memory is the two frequency maps plus one block.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};

use crate::record::SentenceRecord;
use crate::writer::{DictIndex, FreqCounter, Gph2StreamWriter, prepare};

pub struct SpoolWriter {
    spool: BufWriter<File>,
    n_docs: u32,
    /// Colocated attribute names, fixed by the first record that has any.
    colocated: Option<Vec<String>>,
    freq: Option<FreqCounter>,
}

impl SpoolWriter {
    pub fn new() -> Result<Self, String> {
        let file = tempfile::tempfile().map_err(|e| format!("cannot create GPH2 spool: {e}"))?;
        Ok(Self {
            spool: BufWriter::with_capacity(1 << 20, file),
            n_docs: 0,
            colocated: None,
            freq: None,
        })
    }

    /// Number of rows pushed so far (= the document ids covered).
    pub fn len(&self) -> u32 {
        self.n_docs
    }

    pub fn is_empty(&self) -> bool {
        self.n_docs == 0
    }

    /// Append the next row. `None` is a document without a graph (a hole).
    pub fn push(&mut self, record: Option<&SentenceRecord>) -> Result<(), String> {
        match record {
            None => self
                .spool
                .write_all(&0u32.to_le_bytes())
                .map_err(|e| e.to_string())?,
            Some(rec) => {
                if self.colocated.is_none() {
                    let names = rec.attr_names.clone();
                    self.freq = Some(FreqCounter::new(names.len()));
                    self.colocated = Some(names);
                }
                self.freq.as_mut().expect("set above").add(rec);
                let bytes = rec.to_bytes();
                self.spool
                    .write_all(&(bytes.len() as u32).to_le_bytes())
                    .and_then(|()| self.spool.write_all(&bytes))
                    .map_err(|e| e.to_string())?;
            }
        }
        self.n_docs = self
            .n_docs
            .checked_add(1)
            .ok_or("GPH2 spool exceeds u32::MAX rows")?;
        Ok(())
    }

    /// Write the finished GPH2 file for `uuid` to `sink` and return it.
    pub fn finish<W: Write>(mut self, sink: W, uuid: &str) -> Result<W, String> {
        self.spool.flush().map_err(|e| e.to_string())?;
        let mut file = self.spool.into_inner().map_err(|e| e.to_string())?;
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;

        let colocated = self.colocated.unwrap_or_default();
        let freq = self
            .freq
            .unwrap_or_else(|| FreqCounter::new(colocated.len()));
        let (rel_dict, attr_dicts, label_w, attr_w) = freq.dictionaries();
        let rel_index = DictIndex::build(&rel_dict);
        let attr_indexes: Vec<DictIndex> = attr_dicts.iter().map(|d| DictIndex::build(d)).collect();

        let mut writer = Gph2StreamWriter::new(sink, label_w, attr_w);
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut buf = Vec::new();
        for _ in 0..self.n_docs {
            let mut len = [0u8; 4];
            reader.read_exact(&mut len).map_err(|e| e.to_string())?;
            let len = u32::from_le_bytes(len) as usize;
            if len == 0 {
                writer.push_sentence(None)?;
                continue;
            }
            buf.resize(len, 0);
            reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
            let record = SentenceRecord::from_bytes(&buf)?;
            writer.push_sentence(Some(prepare(
                &record,
                &rel_index,
                &attr_indexes,
                &colocated,
            )))?;
        }
        writer.finish(uuid, colocated, rel_dict, attr_dicts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::Gph2File;
    use crate::view::SentenceScratch;
    use crate::writer::Gph2Writer;

    fn record(n: u32, edges: &[(u32, u32, &str)], tags: &[&str]) -> SentenceRecord {
        let mut r = SentenceRecord::new(n);
        r.set_edges(
            edges
                .iter()
                .map(|&(g, d, l)| (g, d, l.to_string()))
                .collect(),
        );
        r.set_attr("tag", tags.iter().map(|t| t.to_string()).collect());
        r
    }

    #[test]
    fn spool_output_equals_in_memory_encode() {
        let docs: Vec<Option<SentenceRecord>> = (0..300)
            .map(|i| {
                if i % 7 == 3 {
                    None
                } else {
                    Some(record(
                        3,
                        &[
                            (1, 0, "nsubj"),
                            (1, 2, if i % 2 == 0 { "dobj" } else { "advmod" }),
                        ],
                        &["NN", "VB", "NN"],
                    ))
                }
            })
            .collect();
        let expected = Gph2Writer::encode("abc", &docs).unwrap();

        let mut spool = SpoolWriter::new().unwrap();
        for d in &docs {
            spool.push(d.as_ref()).unwrap();
        }
        assert_eq!(spool.len(), 300);
        let got = spool.finish(Vec::new(), "abc").unwrap();
        assert_eq!(got, expected, "spooled encoding must be byte-identical");

        // Spot-check a decoded row.
        let file = Gph2File::open(&got).unwrap();
        let mut scratch = SentenceScratch::default();
        assert_eq!(
            file.sentence(3, &mut scratch).unwrap().n_tokens,
            0,
            "hole stays a hole"
        );
        assert_eq!(file.sentence(4, &mut scratch).unwrap().n_tokens, 3);
    }

    #[test]
    fn empty_spool_is_a_valid_file() {
        let bytes = SpoolWriter::new().unwrap().finish(Vec::new(), "u").unwrap();
        assert_eq!(Gph2File::open(&bytes).unwrap().trailer.max_doc, 0);
    }
}
