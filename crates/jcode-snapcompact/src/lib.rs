//! Snapcompact: bitmap-frame context compression ported from omp.
//!
//! Dropped conversation history is serialized to text, word-wrapped, paginated,
//! and rasterized into dense pixel-font PNG frames that vision models read
//! back — deterministic, local, no LLM call. The rasterizer is adapted from
//! omp's MIT-licensed `pi-natives` implementation (see `reference/`); shape
//! constants are eval-tuned upstream and must be copied verbatim, not re-derived.
