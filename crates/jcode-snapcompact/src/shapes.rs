//! Frame shapes, provider billing, and shape resolution.
//!
//! Ported from `snapcompact.ts`: SHAPE_VARIANTS (105-190), billingFamily
//! (206-222), familyBilling (234-247), MODEL_VARIANTS (338-356), resolveShape
//! (378-386), resolveShapeForText (411-421), denseCompanion (1714-1719).
//!
//! Note: the SPEC header says "19 variants", but the TypeScript source of truth
//! declares 17 (4 font families × {bw,sent} = 8, plus 6x12-dim, 8x13-bw,
//! 8on16-bw, 8on22-bw, 11on16-bw, silver16-bw, and 3 doc- shapes). We port the
//! 17 that exist. — ponytail: source of truth wins over the SPEC's count typo.

use std::sync::LazyLock;

use regex::Regex;

use crate::paginate::geometry_default;
use crate::render::{RenderOptions, render_png};
use crate::serialize::{is_cjk_heavy_text, scan_renderability};

/// Geometry half of a {@link Shape}: everything except provider billing.
#[derive(Debug, Clone, PartialEq)]
pub struct ShapeGeometry {
	pub font:         &'static str,
	pub cell_width:   u32,
	pub cell_height:  u32,
	pub stretch:      Option<bool>,
	pub variant:      &'static str,
	pub stopword_dim: bool,
	pub columns:      Option<u32>,
	pub line_repeat:  u32,
	pub frame_size:   u32,
}

impl ShapeGeometry {
	/// A base geometry with the common defaults (sent ink, no stretch/dim, one
	/// column, single line). Callers override fields with struct-update syntax.
	const fn base(font: &'static str, cell_width: u32, cell_height: u32, frame_size: u32) -> Self {
		Self {
			font,
			cell_width,
			cell_height,
			stretch: None,
			variant: "sent",
			stopword_dim: false,
			columns: None,
			line_repeat: 1,
			frame_size,
		}
	}
}

/// One priced frame shape: geometry plus provider billing.
#[derive(Debug, Clone, PartialEq)]
pub struct Shape {
	pub font:                 &'static str,
	pub cell_width:           u32,
	pub cell_height:          u32,
	pub stretch:              Option<bool>,
	pub variant:              &'static str,
	pub stopword_dim:         bool,
	pub columns:              Option<u32>,
	pub line_repeat:          u32,
	pub frame_size:           u32,
	pub frame_token_estimate: u32,
	pub image_detail:         Option<&'static str>,
}

impl Shape {
	/// Native renderer options for this shape at `size` px.
	pub fn render_options(&self, size: u32) -> RenderOptions {
		RenderOptions {
			size,
			font: Some(self.font.to_string()),
			cell_width: Some(self.cell_width),
			cell_height: Some(self.cell_height),
			variant: Some(self.variant.to_string()),
			line_repeat: Some(self.line_repeat),
			stretch: self.stretch,
			columns: self.columns,
		}
	}

	/// Rasterize `text` to raw PNG bytes on this shape at `size` px.
	pub fn render(&self, text: &str, size: u32) -> anyhow::Result<Vec<u8>> {
		render_png(text, &self.render_options(size))
	}
}

/// The 17 eval-validated frame geometries, by research name. Verbatim from
/// `SHAPE_VARIANTS` in snapcompact.ts (105-190).
pub fn shape_variant(name: &str) -> Option<ShapeGeometry> {
	let g = match name {
		"8x8r-bw" => ShapeGeometry {
			variant: "bw",
			line_repeat: 2,
			..ShapeGeometry::base("8x8", 8, 8, 1568)
		},
		"8x8r-sent" => ShapeGeometry {
			line_repeat: 2,
			..ShapeGeometry::base("8x8", 8, 8, 1568)
		},
		"8x8u-bw" => ShapeGeometry {
			variant: "bw",
			..ShapeGeometry::base("8x8", 8, 8, 1568)
		},
		"8x8u-sent" => ShapeGeometry::base("8x8", 8, 8, 1568),
		"6x6u-bw" => ShapeGeometry {
			variant: "bw",
			..ShapeGeometry::base("8x8", 6, 6, 1568)
		},
		"6x6u-sent" => ShapeGeometry::base("8x8", 6, 6, 1568),
		"5x8-bw" => ShapeGeometry {
			variant: "bw",
			..ShapeGeometry::base("5x8", 5, 8, 2576)
		},
		"5x8-sent" => ShapeGeometry::base("5x8", 5, 8, 2576),
		"6x12-dim" => ShapeGeometry {
			variant: "bw",
			stopword_dim: true,
			..ShapeGeometry::base("6x12", 6, 12, 1568)
		},
		"8x13-bw" => ShapeGeometry {
			variant: "bw",
			..ShapeGeometry::base("8x13", 8, 13, 1568)
		},
		"8on16-bw" => ShapeGeometry {
			stretch: Some(false),
			variant: "bw",
			..ShapeGeometry::base("8x13", 8, 16, 1568)
		},
		"8on22-bw" => ShapeGeometry {
			stretch: Some(false),
			variant: "bw",
			..ShapeGeometry::base("8x13", 8, 22, 1568)
		},
		"11on16-bw" => ShapeGeometry {
			stretch: Some(false),
			variant: "bw",
			..ShapeGeometry::base("8x13", 11, 16, 1568)
		},
		"silver16-bw" => ShapeGeometry {
			variant: "bw",
			..ShapeGeometry::base("silver", 16, 16, 1568)
		},
		"doc-8on16-bw" => ShapeGeometry {
			stretch: Some(false),
			variant: "bw",
			columns: Some(2),
			..ShapeGeometry::base("8x13", 8, 16, 1568)
		},
		"doc-8on16-sent" => ShapeGeometry {
			stretch: Some(false),
			columns: Some(2),
			..ShapeGeometry::base("8x13", 8, 16, 1568)
		},
		"doc-8on16-sent-dim" => ShapeGeometry {
			stretch: Some(false),
			stopword_dim: true,
			columns: Some(2),
			..ShapeGeometry::base("8x13", 8, 16, 1568)
		},
		_ => return None,
	};
	Some(g)
}

/// All variant names, in declaration order.
pub const SHAPE_VARIANT_NAMES: [&str; 17] = [
	"8x8r-bw",
	"8x8r-sent",
	"8x8u-bw",
	"8x8u-sent",
	"6x6u-bw",
	"6x6u-sent",
	"5x8-bw",
	"5x8-sent",
	"6x12-dim",
	"8x13-bw",
	"8on16-bw",
	"8on22-bw",
	"11on16-bw",
	"silver16-bw",
	"doc-8on16-bw",
	"doc-8on16-sent",
	"doc-8on16-sent-dim",
];

/// Provider families with distinct image billing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingFamily {
	Anthropic,
	Google,
	OpenAI,
}

/// Billing family for a model id. omp keys this off the wire API; jcode has
/// only the model id at the shape-resolution boundary, so we derive it from the
/// id (gemini→google, gpt/codex→openai, everything else including claude/glm/
/// kimi/unknown→anthropic, the safe pixel-area ceiling).
pub fn billing_family(model_id: &str) -> BillingFamily {
	let m = model_id.to_ascii_lowercase();
	if m.contains("gemini") {
		BillingFamily::Google
	} else if m.contains("gpt") || m.contains("codex") {
		BillingFamily::OpenAI
	} else {
		BillingFamily::Anthropic
	}
}

/// Per-frame billing for a square frame of edge `frame_size`, by family.
/// Verbatim formulas from `familyBilling` in snapcompact.ts (234-247).
fn family_billing(family: BillingFamily, frame_size: u32) -> (u32, Option<&'static str>) {
	match family {
		BillingFamily::Google => (1120, None),
		BillingFamily::OpenAI => {
			let patches = frame_size.div_ceil(32).pow(2).min(10_000);
			(((patches as f64) * 1.2).ceil() as u32, Some("original"))
		},
		BillingFamily::Anthropic => {
			let patches = frame_size.div_ceil(28).pow(2).min(4784);
			(((patches as f64) * 1.05).ceil() as u32, None)
		},
	}
}

/// Attach a provider family's billing to a variant geometry (`priceShape`).
pub fn price_shape(base: &ShapeGeometry, family: BillingFamily) -> Shape {
	let (frame_token_estimate, image_detail) = family_billing(family, base.frame_size);
	Shape {
		font: base.font,
		cell_width: base.cell_width,
		cell_height: base.cell_height,
		stretch: base.stretch,
		variant: base.variant,
		stopword_dim: base.stopword_dim,
		columns: base.columns,
		line_repeat: base.line_repeat,
		frame_size: base.frame_size,
		frame_token_estimate,
		image_detail,
	}
}

/// Eval-winning variant per provider family (billing fallback when the model id
/// matches no known reader line).
fn family_variant(family: BillingFamily) -> &'static str {
	match family {
		BillingFamily::Anthropic => "11on16-bw",
		BillingFamily::Google => "8on22-bw",
		BillingFamily::OpenAI => "8on22-bw",
	}
}

/// Denser companion variant per family for the foveated archive middle.
fn family_variant_low(_family: BillingFamily) -> &'static str {
	"8on16-bw"
}

/// One model line's ideal format: variant plus an optional frame-size override.
#[derive(Debug, Clone, Copy)]
pub struct IdealShape {
	pub variant:    &'static str,
	pub frame_size: Option<u32>,
}

/// Eval-winning format per model line, matched against the model id (first
/// match wins). Verbatim from `MODEL_VARIANTS` in snapcompact.ts (338-356).
static MODEL_VARIANTS: LazyLock<Vec<(Regex, IdealShape)>> = LazyLock::new(|| {
	let re = |p: &str| Regex::new(p).expect("static MODEL_VARIANTS regex");
	vec![
		(re(r"(?i)claude.*(fable|mythos)"), IdealShape {
			variant:    "11on16-bw",
			frame_size: Some(1932),
		}),
		(re(r"(?i)claude-?opus-?4[.-][7-9]"), IdealShape {
			variant:    "11on16-bw",
			frame_size: Some(1932),
		}),
		(re(r"(?i)claude"), IdealShape { variant: "11on16-bw", frame_size: None }),
		(re(r"(?i)gemini"), IdealShape {
			variant:    "8on22-bw",
			frame_size: Some(2048),
		}),
		(re(r"(?i)gpt|codex"), IdealShape { variant: "8on22-bw", frame_size: None }),
		(re(r"(?i)kimi"), IdealShape { variant: "8on16-bw", frame_size: None }),
		(re(r"(?i)glm"), IdealShape { variant: "8on16-bw", frame_size: None }),
	]
});

/// Eval-ideal format for a model id, or None when unmeasured.
pub fn ideal_shape_variant(model_id: &str) -> Option<IdealShape> {
	MODEL_VARIANTS
		.iter()
		.find(|(pattern, _)| pattern.is_match(model_id))
		.map(|(_, ideal)| *ideal)
}

/// Pick the frame shape for a reader. An explicit `variant` (anything but
/// `Some("auto")`) forces that geometry; otherwise the model id selects the
/// eval-winning shape and frame size, falling back to the API family's winner.
/// Mirrors `resolveShape` in snapcompact.ts (378-386).
pub fn resolve_shape(model_id: &str, variant: Option<&str>) -> Shape {
	let family = billing_family(model_id);
	if let Some(v) = variant {
		if v != "auto" {
			let base = shape_variant(v).unwrap_or_else(|| panic!("unknown shape variant {v:?}"));
			return price_shape(&base, family);
		}
	}
	let ideal = ideal_shape_variant(model_id);
	let name = ideal.map(|i| i.variant).unwrap_or_else(|| family_variant(family));
	let ideal_frame = ideal.and_then(|i| i.frame_size);
	if name == family_variant(family) && ideal_frame.is_none() {
		return price_shape(&shape_variant(name).expect("family variant exists"), family);
	}
	let mut base = shape_variant(name).expect("model variant exists");
	if let Some(fs) = ideal_frame {
		base.frame_size = fs;
	}
	price_shape(&base, family)
}

/// Pick the frame shape for `text`. Explicit variants remain forced. Auto first
/// resolves the model default, then selects the Silver CJK grid when the default
/// font cannot safely render the text or wide CJK glyphs dominate. Mirrors
/// `resolveShapeForText` (411-421).
pub fn resolve_shape_for_text(text: &str, model_id: &str, variant: Option<&str>) -> Shape {
	let shape = resolve_shape(model_id, variant);
	if let Some(v) = variant {
		if v != "auto" {
			return shape;
		}
	}
	let silver = resolve_shape(model_id, Some("silver16-bw"));
	if !scan_renderability(text, shape.font).is_safe {
		return if scan_renderability(text, silver.font).is_safe {
			silver
		} else {
			shape
		};
	}
	if shape.font != "silver"
		&& is_cjk_heavy_text(text)
		&& scan_renderability(text, silver.font).is_safe
	{
		silver
	} else {
		shape
	}
}

/// Denser companion of `high` for the foveated archive middle: same family and
/// frame size (identical per-frame bill) but a tighter cell. Returns `high`
/// unchanged for doc layouts, TrueType shapes, or when no denser variant packs
/// more chars. Mirrors `denseCompanion` (1714-1719).
pub fn dense_companion(high: &Shape, model_id: &str) -> Shape {
	if high.columns == Some(2) || high.font == "silver" {
		return high.clone();
	}
	let family = billing_family(model_id);
	let mut base = shape_variant(family_variant_low(family)).expect("low variant exists");
	base.frame_size = high.frame_size;
	let low = price_shape(&base, family);
	if geometry_default(&low).capacity > geometry_default(high).capacity {
		low
	} else {
		high.clone()
	}
}
