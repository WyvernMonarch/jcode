//! Snapcompact: bitmap-frame context compression ported from omp.
//!
//! Dropped conversation history is serialized to text, word-wrapped, paginated,
//! and rasterized into dense pixel-font PNG frames that vision models read
//! back — deterministic, local, no LLM call. The rasterizer is adapted from
//! omp's MIT-licensed `pi-natives` implementation (see `reference/`); shape
//! constants are eval-tuned upstream and must be copied verbatim, not re-derived.
//!
//! Top-level entry: [`compact_to_blocks`] serializes a jcode conversation and
//! returns the summary-prompt text block followed by the ordered history blocks
//! (oldest text edge, imaged middle, newest text edge). [`resolve_shape`] /
//! [`resolve_shape_for_text`] pick the provider-aware frame shape, and
//! [`render_frame`] is the low-level `text + Shape -> PNG bytes` path for tests.

mod archive;
mod paginate;
mod render;
mod serialize;
mod shapes;

#[cfg(test)]
mod tests;

pub use archive::{
	Archive, ArchiveLayout, FRAME_DATA_BYTES_BUDGET, FRAME_DATA_BYTES_ESTIMATE,
	FRAME_TOKEN_ESTIMATE, Frame, HQ_EDGE_FRAMES, MAX_FRAMES_DEFAULT, compact_to_blocks,
	history_blocks, plan_archive,
};
pub use paginate::{
	DOC_GUTTER, Geometry, cell_length, char_cells, doc_pages, geometry, geometry_default,
	is_wide_code_point, paginate_cells, rendered_chars, uses_wide_cells, wrap,
};
pub use render::{RenderOptions, render_png, supported_chars};
pub use serialize::{
	DIM_OFF, DIM_ON, NEWLINE_GLYPH, Renderability, SerializeOptions, TOOL_ARG_MAX_CHARS,
	TOOL_CALL_MAX_CHARS, TOOL_RESULT_MAX_CHARS, TRUNCATE_HEAD_RATIO, dim_stopwords,
	is_cjk_heavy_text, normalize, scan_renderability, serialize_conversation, strip_dim_markers,
	to_plain_text,
};
pub use shapes::{
	BillingFamily, IdealShape, SHAPE_VARIANT_NAMES, Shape, ShapeGeometry, billing_family,
	dense_companion, ideal_shape_variant, price_shape, resolve_shape, resolve_shape_for_text,
	shape_variant,
};

/// Low-level `text + Shape -> PNG bytes` path (renders at the shape's own frame
/// size). Deterministic: identical inputs yield byte-identical PNGs.
pub fn render_frame(text: &str, shape: &Shape) -> anyhow::Result<Vec<u8>> {
	shape.render(text, shape.frame_size)
}
