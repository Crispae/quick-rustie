# rustie-query

Odinson-style query language from RustIE: PEG grammar, AST, and parser.

## Scope

| Included | Not included (yet) |
| --- | --- |
| `query.pest` grammar | Tantivy / Quickwit compilers |
| AST (`Pattern`, `Constraint`, …) | Graph traversal execution |
| `QueryParser::parse_query` | Search engine binding |

## Usage

```rust
use rustie_query::QueryParser;

let pattern = QueryParser::new()
    .parse_query("[word=John] >nsubj [pos=VBZ]")?;
```

## Origin

Ported from RustIE `src/query/{ast,parser,pest_parser}.rs` and `src/query.pest`.
