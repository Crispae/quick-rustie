# rustie-graph-store

GPH2, the per-split dependency-graph file (ported from RustIE). Pure bytes in / structs out.

- **Layout:** blocks of 128 documents (columnar heads, labels, colocated attributes, overlay
  edges), then a trailer (block offsets, label and attribute dictionaries) and a 16-byte end
  record. A reader range-reads only the blocks it needs.
- **`SpoolWriter`:** builds a file for a split whose size is unknown up front, in bounded memory
  (records spooled to a temp file while label frequencies are counted, then replayed once).
- **`merge_sources`:** streams several files into one, remapping dictionaries and keeping only
  the alive rows of each source, in the caller's order; sources without a file become empty rows.
