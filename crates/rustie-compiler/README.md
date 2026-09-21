# rustie-compiler

Compile Odinson/RustIE patterns into **Quickwit-oriented** candidate filters plus
in-memory execution plans. Graph traversal reuses RustIE’s plan/NFA/`evaluate_spans`
algorithm against [`SentenceGraph`](../rustie-schema) JSON — no GPH2 sidecars.

## Scope

| Included | Not included |
| --- | --- |
| `QueryCompiler` → `CompiledQuery::{Surface,Graph}` | Tantivy custom scorers |
| `CandidateFilter` (term / regex / **phrase** / and / or) + `to_quickwit_query()` | Quickwit HTTP search client |
| Graph plan / NFA / sentence eval | GPH2 writer/reader |

## Usage

```rust
use rustie_compiler::QueryCompiler;
use rustie_query::QueryParser;

let pattern = QueryParser::new()
    .parse_query("[word=John] >nsubj [pos=VBZ]")?;
let compiled = QueryCompiler::new().compile_pattern(&pattern)?;
```

## Origin

Ported from RustIE `src/query/compiler`, `src/graph/{plan,nfa,require,eval}`,
and `src/matching/{node_test,span_vm,tokenset}` — adapted for Quickwit postings
fields (`word`, `lemma`, `outgoing_edges`, …).
