//! Opaque pagination cursors: "resume after this document address".
//!
//! Quickwit orders hits by (split, segment, doc) when no sort field is given, and supports
//! `search_after` on that address, so resuming costs the same as the first page. (Offset
//! paging would re-scan and re-fetch every earlier candidate.)

use quickwit_proto::search::PartialHit;
use serde::{Deserialize, Serialize};

use crate::error::{Result, SearchError};

#[derive(Serialize, Deserialize)]
struct Cursor {
    /// Fingerprint of the query text: a cursor is only valid for the query that produced it.
    query: u64,
    after: PartialHit,
}

pub fn encode(query: &str, after: &PartialHit) -> String {
    let cursor = Cursor {
        query: fingerprint(query),
        after: after.clone(),
    };
    let json = serde_json::to_vec(&cursor).expect("cursor serializes");
    json.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn decode(query: &str, token: &str) -> Result<PartialHit> {
    let invalid = || SearchError::InvalidQuery("malformed cursor".into());
    if !token.len().is_multiple_of(2) || !token.is_ascii() {
        return Err(invalid());
    }
    let bytes = (0..token.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&token[i..i + 2], 16))
        .collect::<std::result::Result<Vec<u8>, _>>()
        .map_err(|_| invalid())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if cursor.query != fingerprint(query) {
        return Err(SearchError::InvalidQuery(
            "cursor belongs to a different query".into(),
        ));
    }
    Ok(cursor.after)
}

/// FNV-1a: stable across builds (unlike `DefaultHasher`), which matters for cursors that may
/// outlive a server restart.
fn fingerprint(query: &str) -> u64 {
    query.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_binds_to_the_query() {
        let after = PartialHit {
            split_id: "split-1".into(),
            segment_ord: 0,
            doc_id: 42,
            ..Default::default()
        };
        let token = encode("[word=a]", &after);
        assert_eq!(decode("[word=a]", &token).unwrap(), after);
        assert!(decode("[word=b]", &token).is_err());
        assert!(decode("[word=a]", "zz").is_err());
        assert!(decode("[word=a]", "abc").is_err());
    }
}
