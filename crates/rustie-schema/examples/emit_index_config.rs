//! Print the default Quickwit index config (`configs/ie_postings.yaml`) to stdout.
//!
//! ```bash
//! cargo run -p rustie-schema --example emit_index_config > configs/ie_postings.yaml
//! ```

use rustie_schema::{postings_index_config_yaml, IndexConfigOptions};

fn main() {
    print!(
        "{}",
        postings_index_config_yaml(&IndexConfigOptions::default())
    );
}
