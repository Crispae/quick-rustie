//! Walk a directory of Odinson JSON documents and flatten them into Quickwit
//! sentence documents, in bounded batches.

use std::path::{Path, PathBuf};

use rustie_schema::flatten_odinson_json;
use serde_json::Value as JsonValue;
use tracing::warn;
use walkdir::WalkDir;

use crate::error::{IndexerError, Result};

/// Cap on failures kept verbatim in [`FlattenedBatch::failures`]; the rest are only counted.
pub const MAX_RECORDED_FAILURES: usize = 100;

/// A file that could not be read or flattened.
#[derive(Debug, Clone)]
pub struct FileFailure {
    pub path: PathBuf,
    pub error: String,
}

/// Result of flattening one group of files.
#[derive(Debug, Default)]
pub struct FlattenedBatch {
    /// One Quickwit JSON document per sentence.
    pub docs: Vec<JsonValue>,
    /// Number of input files in this batch (including failed ones).
    pub num_files: usize,
    pub num_failed_files: usize,
    /// First [`MAX_RECORDED_FAILURES`] failures.
    pub failures: Vec<FileFailure>,
}

/// List `*.json` files under `dir` (recursive, no symlink following), sorted by path so
/// batches — and their content-derived checkpoint partitions — are reproducible across runs.
pub fn list_odinson_files(dir: &Path, limit: Option<usize>) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Err(IndexerError::InvalidConfig(format!(
            "data path `{}` is not a directory",
            dir.display()
        )));
    }
    let mut files = Vec::new();
    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry.map_err(|err| {
            let path = err.path().unwrap_or(dir).to_path_buf();
            IndexerError::io(path, err.into())
        })?;
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            files.push(entry.into_path());
        }
    }
    files.sort();
    if let Some(limit) = limit {
        files.truncate(limit);
    }
    Ok(files)
}

/// Read and flatten `files`. A malformed file is recorded and skipped rather than
/// aborting the whole run: a multi-hour ingest should not die on one bad document.
pub fn flatten_files(files: &[PathBuf]) -> FlattenedBatch {
    let mut batch = FlattenedBatch {
        num_files: files.len(),
        ..Default::default()
    };
    for path in files {
        match flatten_file(path) {
            Ok(mut docs) => batch.docs.append(&mut docs),
            Err(error) => {
                warn!(path = %path.display(), %error, "skipping unreadable Odinson file");
                batch.num_failed_files += 1;
                if batch.failures.len() < MAX_RECORDED_FAILURES {
                    batch.failures.push(FileFailure {
                        path: path.clone(),
                        error,
                    });
                }
            }
        }
    }
    batch
}

fn flatten_file(path: &Path) -> std::result::Result<Vec<JsonValue>, String> {
    let json = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    let sentences = flatten_odinson_json(&json).map_err(|err| err.to_string())?;
    Ok(sentences.iter().map(|s| s.to_quickwit_json()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const FIXTURE: &str = r#"{
      "id": "d1",
      "sentences": [{
        "numTokens": 2,
        "fields": [
          {"name": "word", "$type": "ai.lum.odinson.TokensField", "tokens": ["Hello", "world"]},
          {"name": "dependencies", "$type": "ai.lum.odinson.GraphField",
           "edges": [[1, 0, "amod"]], "roots": [1]}
        ]
      }]
    }"#;

    #[test]
    fn flattens_fixture_and_skips_bad_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.json"), FIXTURE).unwrap();
        std::fs::write(dir.path().join("a.json"), FIXTURE).unwrap();
        std::fs::write(dir.path().join("bad.json"), "{ not json").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let files = list_odinson_files(dir.path(), None).unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["a.json", "b.json", "bad.json"],
            "sorted, .json only"
        );

        let batch = flatten_files(&files);
        assert_eq!(batch.num_files, 3);
        assert_eq!(batch.num_failed_files, 1);
        assert_eq!(batch.failures[0].path.file_name().unwrap(), "bad.json");
        assert_eq!(batch.docs.len(), 2);
        assert_eq!(batch.docs[0]["word"], "Hello|world");
        assert_eq!(batch.docs[0]["doc_id"], "d1");
        assert_eq!(batch.docs[0]["sentence_length"], 2);
    }

    #[test]
    fn limit_applies_after_sorting() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["c.json", "a.json", "b.json"] {
            std::fs::write(dir.path().join(name), FIXTURE).unwrap();
        }
        let files = list_odinson_files(dir.path(), Some(2)).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].file_name().unwrap(), "a.json");
    }

    #[test]
    fn missing_dir_is_a_config_error() {
        let err = list_odinson_files(Path::new("/definitely/not/here"), None).unwrap_err();
        assert!(matches!(err, IndexerError::InvalidConfig(_)));
    }
}
