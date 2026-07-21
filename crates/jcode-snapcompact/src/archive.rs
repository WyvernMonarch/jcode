//! Archive assembly: foveated frame planning, history blocks, and the top-level
//! `compact_to_blocks` entry point.
//!
//! Ports `planArchive` (snapcompact.ts 1750-1834), `historyBlocks` /
//! `imagesWithinBudget` / `omittedFrameNotice` (1618-1704), and the assembly
//! half of `compact` (1849-1976), producing `Vec<ContentBlock>` over jcode's
//! message types. The summary prompt is embedded verbatim and rendered by a
//! small handlebars-subset evaluator (`{{#if}}/{{else}}/{{/if}}` + `{{var}}`).
//!
//! ponytail: jcode's `ContentBlock::Image` has no `detail` field, so the
//! OpenAI-only resolution hint is dropped when building image blocks (it is a
//! provider hint, not content). The `{{files}}` template section is always empty
//! here — stage 1 has no fileOps input — so `file-operations.md` is embedded for
//! provenance but never rendered.

use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use jcode_message_types::ContentBlock;
use regex::Regex;

use crate::paginate::{doc_pages, geometry_default, paginate_cells, uses_wide_cells};
use crate::serialize::{
	DIM_ON, DIM_OFF, dim_stopwords, normalize, serialize_conversation, to_plain_text,
};
use crate::shapes::{Shape, dense_companion, resolve_shape_for_text};

// ============================================================================
// Constants (snapcompact.ts 438-462, 443, 449, 757)
// ============================================================================

/// Default upper bound on archive frames carried per compaction.
pub const MAX_FRAMES_DEFAULT: usize = 80;
/// High-quality frames rendered at each chronological edge of a foveated middle.
pub const HQ_EDGE_FRAMES: usize = 3;
/// Conservative per-frame token estimate used for context budgeting.
pub const FRAME_TOKEN_ESTIMATE: u32 = 5024;
/// Conservative upper bound for one persisted frame's base64 payload.
pub const FRAME_DATA_BYTES_ESTIMATE: usize = 170_000;
/// Maximum snapcompact image base64 carried in every rebuilt provider request.
pub const FRAME_DATA_BYTES_BUDGET: usize = 3_000_000;
/// Plain-text history kept verbatim at each chronological edge (HQ-capacity units).
const TEXT_EDGE_PAGES: usize = 1;

// ============================================================================
// Archive data model
// ============================================================================

/// One developed frame: base64 PNG plus its OpenAI detail hint (dropped when
/// emitted as a jcode image block).
#[derive(Debug, Clone)]
pub struct Frame {
	pub data:   String,
	pub detail: Option<&'static str>,
}

/// Reconstructable archive: rendered frames plus the verbatim text edges.
#[derive(Debug, Clone, Default)]
pub struct Archive {
	pub frames:          Vec<Frame>,
	pub text_head:       String,
	pub text_tail:       String,
	pub truncated_chars: usize,
}

fn text_block(text: String) -> ContentBlock {
	ContentBlock::Text { text, cache_control: None }
}

fn image_block(frame: &Frame) -> ContentBlock {
	ContentBlock::Image { media_type: "image/png".to_string(), data: frame.data.clone() }
}

// ============================================================================
// History blocks
// ============================================================================

fn commas(n: usize) -> String {
	let s = n.to_string();
	let bytes = s.as_bytes();
	let mut out = String::new();
	for (i, b) in bytes.iter().enumerate() {
		if i > 0 && (bytes.len() - i) % 3 == 0 {
			out.push(',');
		}
		out.push(*b as char);
	}
	out
}

fn format_frame_data_bytes(bytes: usize) -> String {
	if bytes >= 1_000_000 {
		format!("{:.1} MB", bytes as f64 / 1_000_000.0)
	} else if bytes >= 1_000 {
		format!("{:.1} KB", bytes as f64 / 1_000.0)
	} else {
		format!("{bytes} B")
	}
}

fn omitted_frame_notice(omitted_frames: usize, omitted_bytes: usize) -> String {
	let plural = if omitted_frames == 1 { "" } else { "s" };
	[
		"-------------- snapcompact image middle omitted".to_string(),
		format!(
			"{} archived image frame{plural} ({} base64) exceeded the per-request snapcompact \
			 payload budget. The compacted summary and visible text edges remain available.",
			commas(omitted_frames),
			format_frame_data_bytes(omitted_bytes)
		),
		"--------------".to_string(),
	]
	.join("\n")
}

struct Budgeted {
	images:         Vec<ContentBlock>,
	omitted_frames: usize,
	omitted_bytes:  usize,
}

fn images_within_budget(archive: &Archive, max_frame_data_bytes: Option<usize>) -> Budgeted {
	let Some(budget) = max_frame_data_bytes else {
		return Budgeted {
			images:         archive.frames.iter().map(image_block).collect(),
			omitted_frames: 0,
			omitted_bytes:  0,
		};
	};
	let mut used = 0usize;
	let mut omitted_frames = 0usize;
	let mut omitted_bytes = 0usize;
	let mut kept: Vec<&Frame> = Vec::new();
	for frame in archive.frames.iter().rev() {
		let bytes = frame.data.len();
		if used + bytes > budget {
			omitted_frames += 1;
			omitted_bytes += bytes;
			continue;
		}
		used += bytes;
		kept.push(frame);
	}
	kept.reverse();
	Budgeted {
		images: kept.into_iter().map(image_block).collect(),
		omitted_frames,
		omitted_bytes,
	}
}

/// Ordered archive blocks for a compaction summary message, oldest to newest.
/// Ported from `historyBlocks` (1667-1704).
pub fn history_blocks(archive: &Archive, max_frame_data_bytes: Option<usize>) -> Vec<ContentBlock> {
	let budgeted = images_within_budget(archive, max_frame_data_bytes);
	let has_images = !budgeted.images.is_empty();
	let has_omitted = budgeted.omitted_frames > 0;
	let notice = || omitted_frame_notice(budgeted.omitted_frames, budgeted.omitted_bytes);

	let mut blocks: Vec<ContentBlock> = Vec::new();

	if !archive.text_head.is_empty() {
		let suffix = if has_images {
			"\n-------------- imaged middle below\n".to_string()
		} else if has_omitted {
			format!("\n{}\n", notice())
		} else {
			String::new()
		};
		blocks.push(text_block(to_plain_text(&archive.text_head) + &suffix));
	} else if has_omitted && !has_images {
		blocks.push(text_block(notice()));
	}

	if has_images && has_omitted {
		blocks.push(text_block(notice()));
	}
	blocks.extend(budgeted.images);

	if !archive.text_tail.is_empty() {
		let prefix = if has_images {
			"-------------- imaged middle above\n".to_string()
		} else if archive.truncated_chars > 0 || has_omitted {
			"\n-------------- middle history omitted above\n".to_string()
		} else {
			String::new()
		};
		let tail = prefix + &to_plain_text(&archive.text_tail);
		if let Some(ContentBlock::Text { text, .. }) = blocks.last_mut() {
			text.push_str(&tail);
		} else {
			blocks.push(text_block(tail));
		}
	}

	blocks
}

// ============================================================================
// Foveated frame planning
// ============================================================================

struct PlanFrame {
	text:  String,
	shape: Shape,
}

/// A foveated archive layout. `frames` are oldest→newest for the imaged middle.
pub struct ArchiveLayout {
	frames:              Vec<PlanFrame>,
	pub text_head:       String,
	pub text_tail:       String,
	pub kept_text:       String,
	pub truncated_chars: usize,
}

fn plan_frames(pages: &[String], shape: &Shape) -> Vec<PlanFrame> {
	pages.iter().map(|t| PlanFrame { text: t.clone(), shape: shape.clone() }).collect()
}

/// Lay out accumulated archive `text` (oldest→newest) with text at both edges
/// and images in the middle, foveating the middle (HQ/LQ/HQ) when it overflows
/// `max_frames`. Ported from `planArchive` (1750-1834).
pub fn plan_archive(text: &str, high: &Shape, low: &Shape, max_frames: usize) -> ArchiveLayout {
	let chars: Vec<char> = text.chars().collect();
	let n = chars.len();
	let slice = |a: usize, b: usize| -> String { chars[a..b].iter().collect() };

	let cap_hi = geometry_default(high).capacity;
	let edge_cap = TEXT_EDGE_PAGES * cap_hi;

	if n <= 2 * edge_cap {
		return ArchiveLayout {
			frames:          Vec::new(),
			text_head:       text.to_string(),
			text_tail:       String::new(),
			kept_text:       text.to_string(),
			truncated_chars: 0,
		};
	}
	if max_frames < 1 {
		let text_head = slice(0, edge_cap);
		let text_tail = slice(n - edge_cap, n);
		let truncated = n - text_head.chars().count() - text_tail.chars().count();
		let kept = format!("{text_head}{text_tail}");
		return ArchiveLayout {
			frames: Vec::new(),
			text_head,
			text_tail,
			kept_text: kept,
			truncated_chars: truncated,
		};
	}

	let text_head = slice(0, edge_cap);
	let text_tail = slice(n - edge_cap, n);
	let image_text = slice(edge_cap, n - edge_cap);
	if image_text.is_empty() {
		return ArchiveLayout {
			frames:          Vec::new(),
			text_head:       text.to_string(),
			text_tail:       String::new(),
			kept_text:       text.to_string(),
			truncated_chars: 0,
		};
	}

	// Doc layouts: one tier, keep newest pages with the session head pinned.
	if high.columns == Some(2) {
		let geo = geometry_default(high);
		let pages = doc_pages(&image_text, geo, uses_wide_cells(high));
		let mut kept = pages.clone();
		let mut truncated_chars = 0usize;
		if pages.len() > max_frames {
			let dropped = &pages[1..pages.len() - (max_frames - 1)];
			truncated_chars = dropped.iter().map(|p| p.chars().count()).sum();
			let mut v = vec![pages[0].clone()];
			v.extend_from_slice(&pages[pages.len() - (max_frames - 1)..]);
			kept = v;
		}
		let flat = kept.iter().map(|p| p.replace('\n', " ")).collect::<Vec<_>>().join(" ");
		return ArchiveLayout {
			frames: plan_frames(&kept, high),
			kept_text: format!("{text_head}{flat}{text_tail}"),
			text_head,
			text_tail,
			truncated_chars,
		};
	}

	// Grid: paginate the imaged region into HQ frames (cell-aware).
	let geo_hi = geometry_default(high);
	let hi_pages = paginate_cells(&image_text, cap_hi, geo_hi.cols, uses_wide_cells(high));
	if hi_pages.len() <= max_frames {
		return ArchiveLayout {
			frames: plan_frames(&hi_pages, high),
			kept_text: format!("{text_head}{image_text}{text_tail}"),
			text_head,
			text_tail,
			truncated_chars: 0,
		};
	}

	// Foveate the imaged middle: HQ edges, dense center, drop the oldest slice.
	let geo_lo = geometry_default(low);
	let cap_lo = geo_lo.capacity;
	let image_edge_frames = HQ_EDGE_FRAMES.min((max_frames - 1) / 2);
	let head_pages = &hi_pages[0..image_edge_frames];
	let tail_pages: &[String] = if image_edge_frames > 0 {
		&hi_pages[hi_pages.len() - image_edge_frames..]
	} else {
		&hi_pages[hi_pages.len()..]
	};
	let image_head: String = head_pages.concat();
	let image_tail: String = tail_pages.concat();
	let img_chars: Vec<char> = image_text.chars().collect();
	let ih_len = image_head.chars().count();
	let it_len = image_tail.chars().count();
	let middle_source: String = img_chars[ih_len..img_chars.len() - it_len].iter().collect();
	let mut middle_pages = paginate_cells(&middle_source, cap_lo, geo_lo.cols, uses_wide_cells(low));
	let middle_budget = max_frames - 2 * image_edge_frames;
	let mut truncated_chars = 0usize;
	let mut middle_text = middle_source.clone();
	if middle_pages.len() > middle_budget {
		let drop_count = middle_pages.len() - middle_budget;
		let dropped: String = middle_pages[0..drop_count].concat();
		let dropped_chars = dropped.chars().count();
		truncated_chars = dropped_chars;
		let ms_chars: Vec<char> = middle_source.chars().collect();
		middle_text = ms_chars[dropped_chars..].iter().collect();
		middle_pages = middle_pages[drop_count..].to_vec();
	}

	let mut frames = plan_frames(head_pages, high);
	frames.extend(plan_frames(&middle_pages, low));
	frames.extend(plan_frames(tail_pages, high));
	ArchiveLayout {
		frames,
		kept_text: format!("{text_head}{image_head}{middle_text}{image_tail}{text_tail}"),
		text_head,
		text_tail,
		truncated_chars,
	}
}

// ============================================================================
// Summary prompt template (handlebars subset)
// ============================================================================

const SUMMARY_PROMPT: &str = include_str!("prompts/snapcompact-summary.md");
/// Embedded for provenance; the `{{files}}` section is never rendered here.
#[allow(dead_code)]
const FILE_OPERATIONS_TEMPLATE: &str = include_str!("prompts/file-operations.md");

static TAG_RE: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"\{\{[^}]*\}\}").expect("template tag regex"));

#[derive(Clone)]
enum Tok {
	Text(String),
	Open(String),
	Else,
	Close,
	Var(String),
}

enum Node {
	Text(String),
	Var(String),
	If { name: String, then: Vec<Node>, els: Vec<Node> },
}

#[derive(Clone, Copy)]
enum Val {
	Bool(bool),
	Num(i64),
	Missing,
}

impl Val {
	fn truthy(self) -> bool {
		match self {
			Val::Bool(b) => b,
			Val::Num(n) => n != 0,
			Val::Missing => false,
		}
	}

	fn render(self) -> String {
		match self {
			Val::Num(n) => n.to_string(),
			_ => String::new(),
		}
	}
}

fn classify(content: &str) -> Tok {
	if let Some(rest) = content.strip_prefix("#if ") {
		Tok::Open(rest.trim().to_string())
	} else if content == "else" {
		Tok::Else
	} else if content == "/if" {
		Tok::Close
	} else if content.starts_with('#') || content.starts_with('/') {
		// #xml / /xml and any other block helper: not used by the summary prompt.
		Tok::Text(String::new())
	} else {
		Tok::Var(content.to_string())
	}
}

fn tokenize(tmpl: &str) -> Vec<Tok> {
	let mut toks = Vec::new();
	let mut last = 0;
	for m in TAG_RE.find_iter(tmpl) {
		if m.start() > last {
			toks.push(Tok::Text(tmpl[last..m.start()].to_string()));
		}
		let content = tmpl[m.start() + 2..m.end() - 2].trim();
		toks.push(classify(content));
		last = m.end();
	}
	if last < tmpl.len() {
		toks.push(Tok::Text(tmpl[last..].to_string()));
	}
	toks
}

/// Parse a token run until (and leaving `pos` at) the next `Else`/`Close` at
/// this nesting level, or the end.
fn parse_seq(toks: &[Tok], pos: &mut usize) -> Vec<Node> {
	let mut nodes = Vec::new();
	while *pos < toks.len() {
		match &toks[*pos] {
			Tok::Text(s) => {
				nodes.push(Node::Text(s.clone()));
				*pos += 1;
			},
			Tok::Var(v) => {
				nodes.push(Node::Var(v.clone()));
				*pos += 1;
			},
			Tok::Open(name) => {
				let name = name.clone();
				*pos += 1;
				let then = parse_seq(toks, pos);
				let mut els = Vec::new();
				if matches!(toks.get(*pos), Some(Tok::Else)) {
					*pos += 1;
					els = parse_seq(toks, pos);
				}
				if matches!(toks.get(*pos), Some(Tok::Close)) {
					*pos += 1;
				}
				nodes.push(Node::If { name, then, els });
			},
			Tok::Else | Tok::Close => break,
		}
	}
	nodes
}

fn render_nodes(nodes: &[Node], ctx: &HashMap<&str, Val>, out: &mut String) {
	for node in nodes {
		match node {
			Node::Text(s) => out.push_str(s),
			Node::Var(name) => out.push_str(&ctx.get(name.as_str()).copied().unwrap_or(Val::Missing).render()),
			Node::If { name, then, els } => {
				let branch = if ctx.get(name.as_str()).copied().unwrap_or(Val::Missing).truthy() {
					then
				} else {
					els
				};
				render_nodes(branch, ctx, out);
			},
		}
	}
}

struct SummaryCtx {
	frame_count:               usize,
	doc_columns:               bool,
	cols:                      usize,
	rows:                      usize,
	sentence_ink:              bool,
	stopword_dimmed:           bool,
	line_repeated:             bool,
	truncated_chars:           usize,
	included_previous_summary: bool,
}

fn render_summary_prompt(c: &SummaryCtx) -> String {
	let ctx: HashMap<&str, Val> = HashMap::from([
		("frameCount", Val::Num(c.frame_count as i64)),
		("docColumns", Val::Bool(c.doc_columns)),
		("cols", Val::Num(c.cols as i64)),
		("rows", Val::Num(c.rows as i64)),
		("sentenceInk", Val::Bool(c.sentence_ink)),
		("stopwordDimmed", Val::Bool(c.stopword_dimmed)),
		("lineRepeated", Val::Bool(c.line_repeated)),
		("truncatedChars", Val::Num(c.truncated_chars as i64)),
		("includedPreviousSummary", Val::Bool(c.included_previous_summary)),
		("files", Val::Missing),
	]);
	let toks = tokenize(SUMMARY_PROMPT);
	let mut pos = 0;
	let nodes = parse_seq(&toks, &mut pos);
	let mut out = String::new();
	render_nodes(&nodes, &ctx, &mut out);
	out
}

// ============================================================================
// Entry point
// ============================================================================

fn last_dim_open(s: &str) -> bool {
	match (s.rfind(DIM_ON), s.rfind(DIM_OFF)) {
		(Some(a), Some(b)) => a > b,
		(Some(_), None) => true,
		_ => false,
	}
}

/// Compact a conversation to snapcompact image + text blocks. Fully local and
/// deterministic: serialize → normalize → foveate → render → assemble. Prepends
/// the summary prompt text block, then the ordered history blocks (oldest text
/// edge, imaged middle, newest text edge). No LLM call.
pub fn compact_to_blocks(
	messages: &[jcode_message_types::Message],
	model_id: &str,
	max_frames: usize,
) -> Result<Vec<ContentBlock>> {
	let serialized = serialize_conversation(messages, None);
	let high = resolve_shape_for_text(&serialized, model_id, None);
	let low = dense_companion(&high, model_id);
	let geo = geometry_default(&high);
	let max_frames = max_frames.min(MAX_FRAMES_DEFAULT).max(1);

	let archive_text = normalize(&serialized, Some(high.font));
	let layout = plan_archive(&archive_text, &high, &low, max_frames);

	// Render planned frames, carrying any open dim span across page boundaries.
	let mut dim_open = last_dim_open(&layout.text_head);
	let mut frames: Vec<Frame> = Vec::with_capacity(layout.frames.len());
	for pf in &layout.frames {
		let mut page = if dim_open { format!("{DIM_ON}{}", pf.text) } else { pf.text.clone() };
		dim_open = last_dim_open(&page);
		if pf.shape.stopword_dim {
			page = dim_stopwords(&page);
		}
		let png = pf.shape.render(&page, pf.shape.frame_size)?;
		frames.push(Frame { data: BASE64.encode(&png), detail: pf.shape.image_detail });
	}

	let text_head = layout.text_head.clone();
	let text_tail = if layout.text_tail.is_empty() {
		String::new()
	} else if dim_open {
		format!("{DIM_ON}{}", layout.text_tail)
	} else {
		layout.text_tail.clone()
	};

	let archive = Archive {
		frames,
		text_head: text_head.clone(),
		text_tail: text_tail.clone(),
		truncated_chars: layout.truncated_chars,
	};

	if archive.frames.is_empty() && text_head.is_empty() && text_tail.is_empty() {
		return Ok(vec![text_block("No prior history.".to_string())]);
	}

	let prompt = render_summary_prompt(&SummaryCtx {
		frame_count:               archive.frames.len(),
		doc_columns:               high.columns == Some(2),
		cols:                      geo.cols,
		rows:                      geo.rows,
		sentence_ink:              high.variant == "sent",
		stopword_dimmed:           high.stopword_dim,
		line_repeated:             high.line_repeat > 1,
		truncated_chars:           layout.truncated_chars,
		included_previous_summary: false,
	});

	let mut blocks = vec![text_block(prompt)];
	blocks.extend(history_blocks(&archive, None));
	Ok(blocks)
}
