//! Golden and behavioral tests for the snapcompact port.

use jcode_message_types::{ContentBlock, Message, Role};
use serde_json::json;

use crate::{
	Archive, DIM_OFF, DIM_ON, Frame, compact_to_blocks, doc_pages, geometry, geometry_default,
	history_blocks, paginate_cells, render_frame, resolve_shape, serialize_conversation, wrap,
};

// ---------------------------------------------------------------------------
// Shape resolution golden values
// ---------------------------------------------------------------------------

#[test]
fn golden_shape_resolution() {
	// glm-5v-turbo (our vision test target) → 8on16-bw.
	let glm = resolve_shape("glm-5v-turbo", None);
	assert_eq!((glm.font, glm.cell_width, glm.cell_height, glm.variant), ("8x13", 8, 16, "bw"));

	// claude → 11on16-bw.
	let claude = resolve_shape("claude-3", None);
	assert_eq!(
		(claude.font, claude.cell_width, claude.cell_height, claude.variant),
		("8x13", 11, 16, "bw")
	);

	// gemini → 8on22-bw at the 2048px frame.
	let gemini = resolve_shape("gemini-3.5-flash", None);
	assert_eq!(
		(gemini.font, gemini.cell_width, gemini.cell_height, gemini.variant),
		("8x13", 8, 22, "bw")
	);
	assert_eq!(gemini.frame_size, 2048);
}

#[test]
fn cjk_heavy_text_falls_back_to_silver() {
	// A transcript dominated by wide CJK glyphs resolves to the Silver grid.
	let cjk = "你好世界 这是一个测试 你好世界 这是一个测试".repeat(3);
	let shape = crate::resolve_shape_for_text(&cjk, "claude-3", None);
	assert_eq!((shape.font, shape.cell_width, shape.cell_height), ("silver", 16, 16));
	// Plain ASCII stays on the model's own (8x13-family) shape.
	let ascii = crate::resolve_shape_for_text("just some english prose here", "claude-3", None);
	assert_eq!(ascii.font, "8x13");
	// An explicit variant is never overridden by the CJK heuristic.
	let forced = crate::resolve_shape_for_text(&cjk, "claude-3", Some("8on22-bw"));
	assert_eq!(forced.font, "8x13");
}

// ---------------------------------------------------------------------------
// Provider billing golden values
// ---------------------------------------------------------------------------

#[test]
fn golden_billing_formulas() {
	// Anthropic 11on16-bw @1568: ceil(min(ceil(1568/28)^2,4784) * 1.05) = 3293.
	assert_eq!(resolve_shape("claude-3-5", None).frame_token_estimate, 3293);
	// Anthropic high-res @1932 (opus 4.8): ceil(min(4761,4784)*1.05) = 5000.
	let opus = resolve_shape("claude-opus-4.8", None);
	assert_eq!(opus.frame_size, 1932);
	assert_eq!(opus.frame_token_estimate, 5000);
	// Google flat media_resolution budget.
	assert_eq!(resolve_shape("gemini-2", None).frame_token_estimate, 1120);
	// OpenAI 8on22-bw @1568: ceil(min(ceil(1568/32)^2,10000) * 1.2) = 2882.
	let gpt = resolve_shape("gpt-5", None);
	assert_eq!(gpt.frame_token_estimate, 2882);
	assert_eq!(gpt.image_detail, Some("original"));
}

// ---------------------------------------------------------------------------
// Serialization golden string
// ---------------------------------------------------------------------------

fn assistant(blocks: Vec<ContentBlock>) -> Message {
	Message { role: Role::Assistant, content: blocks, timestamp: None, tool_duration_ms: None }
}

#[test]
fn golden_serialization() {
	let messages = vec![
		Message::user("Fix the bug"),
		assistant(vec![
			ContentBlock::ReasoningTrace { text: "Let me look".to_string() },
			ContentBlock::Text { text: "I'll read the file".to_string(), cache_control: None },
			ContentBlock::ToolUse {
				id:                "t1".to_string(),
				name:              "read".to_string(),
				input:             json!({ "path": "a.rs", "intent": "inspect" }),
				thought_signature: None,
			},
		]),
		Message::tool_result("t1", "line1\nline2", false),
	];

	let out = serialize_conversation(&messages, None);
	let expected = format!(
		"¶user:Fix the bug\n\n¶think:Let me look\n\n¶ai:I'll read the file\n\n\
		 ¶call:read(path=\"a.rs\")//inspect\n<out>\n{DIM_ON}line1\nline2{DIM_OFF}\n</out>"
	);
	assert_eq!(out, expected);
}

#[test]
fn serialization_merges_consecutive_scope_and_dims_orphan_result() {
	// Two consecutive user messages merge under one ¶user scope; an orphan tool
	// result (no matching call in-window) renders standalone under ¶call.
	let messages = vec![
		Message::user("first"),
		Message::user("second"),
		Message::tool_result("orphan", "result body", false),
	];
	let out = serialize_conversation(&messages, None);
	let expected = format!(
		"¶user:first\nsecond\n\n¶call:\n<out>\n{DIM_ON}result body{DIM_OFF}\n</out>"
	);
	assert_eq!(out, expected);
}

// ---------------------------------------------------------------------------
// Pagination counts
// ---------------------------------------------------------------------------

#[test]
fn pagination_counts_grid() {
	// kimi → 8on16-bw @1568: cols=196, rows=98, capacity=19208.
	let shape = resolve_shape("kimi", None);
	let geo = geometry(&shape, shape.frame_size);
	assert_eq!((geo.cols, geo.rows, geo.capacity), (196, 98, 19208));

	let wide_cells = true; // bitmap font
	let exact = "x".repeat(geo.capacity);
	assert_eq!(paginate_cells(&exact, geo.capacity, geo.cols, wide_cells).len(), 1);
	let over = "x".repeat(geo.capacity + 1);
	assert_eq!(paginate_cells(&over, geo.capacity, geo.cols, wide_cells).len(), 2);
	let three = "x".repeat(geo.capacity * 2 + 1);
	assert_eq!(paginate_cells(&three, geo.capacity, geo.cols, wide_cells).len(), 3);
}

#[test]
fn pagination_counts_doc() {
	// doc-8on16-bw @1568: gridCols=196, cols=floor((196-3)/2)=96, rows=98.
	let shape = resolve_shape("claude-3", Some("doc-8on16-bw"));
	let geo = geometry(&shape, shape.frame_size);
	assert_eq!((geo.cols, geo.rows, geo.capacity), (96, 98, 2 * 96 * 98));

	// 50-char words: two won't fit one 96-cell line (50+1+50 > 96), so each rides
	// its own line → 300 lines. Pages hold 2*rows = 196 lines → 2 pages.
	let word = "w".repeat(50);
	let text = vec![word; 300].join(" ");
	let lines = wrap(&text, geo.cols, false);
	assert_eq!(lines.len(), 300);
	assert_eq!(doc_pages(&text, geo, false).len(), 2); // ceil(300/196)
}

// ---------------------------------------------------------------------------
// Render determinism + PNG dimensions
// ---------------------------------------------------------------------------

fn ihdr_dims(png: &[u8]) -> (u32, u32) {
	(
		u32::from_be_bytes(png[16..20].try_into().unwrap()),
		u32::from_be_bytes(png[20..24].try_into().unwrap()),
	)
}

#[test]
fn render_is_byte_deterministic() {
	let shape = resolve_shape("glm-5v-turbo", None); // 8on16-bw
	let a = render_frame("Hello world. Again!", &shape).unwrap();
	let b = render_frame("Hello world. Again!", &shape).unwrap();
	assert_eq!(a, b, "same input must produce byte-identical PNG");
	assert!(!a.is_empty());
}

#[test]
fn png_dimensions_match_geometry() {
	// 8on16-bw @1568: width = frame edge, height hugs used rows * cellHeight.
	let shape = resolve_shape("glm-5v-turbo", None);
	let geo = geometry_default(&shape);
	assert_eq!(geo.cols, 196);

	// Exactly one full row of 196 chars → height 16.
	let one_row = render_frame(&"x".repeat(geo.cols), &shape).unwrap();
	assert_eq!(ihdr_dims(&one_row), (1568, 16));

	// One past a full row → two rows → height 32.
	let two_rows = render_frame(&"x".repeat(geo.cols + 1), &shape).unwrap();
	assert_eq!(ihdr_dims(&two_rows), (1568, 32));
}

// ---------------------------------------------------------------------------
// history_blocks ordering + omission notice (no rendering)
// ---------------------------------------------------------------------------

fn frame(data: &str) -> Frame {
	Frame { data: data.to_string(), detail: None }
}

fn text_of(block: &ContentBlock) -> &str {
	match block {
		ContentBlock::Text { text, .. } => text,
		_ => panic!("expected text block"),
	}
}

#[test]
fn history_blocks_order_and_delimiters() {
	let archive = Archive {
		frames:          vec![frame("AAAA"), frame("BBBB")],
		text_head:       "HEAD".to_string(),
		text_tail:       "TAIL".to_string(),
		truncated_chars: 0,
	};
	let blocks = history_blocks(&archive, None);
	assert_eq!(blocks.len(), 4);
	assert!(text_of(&blocks[0]).starts_with("HEAD"));
	assert!(text_of(&blocks[0]).contains("imaged middle below"));
	assert!(matches!(blocks[1], ContentBlock::Image { .. }));
	assert!(matches!(blocks[2], ContentBlock::Image { .. }));
	assert!(text_of(&blocks[3]).contains("imaged middle above"));
	assert!(text_of(&blocks[3]).ends_with("TAIL"));
}

#[test]
fn history_blocks_emit_omission_notice_when_over_byte_budget() {
	let archive = Archive {
		frames:          vec![frame("AAAA"), frame("BBBB")],
		text_head:       "HEAD".to_string(),
		text_tail:       "TAIL".to_string(),
		truncated_chars: 0,
	};
	// Budget fits only the newest 4-byte frame; the oldest is omitted.
	let blocks = history_blocks(&archive, Some(4));
	let image_count = blocks.iter().filter(|b| matches!(b, ContentBlock::Image { .. })).count();
	assert_eq!(image_count, 1);
	let notice = blocks
		.iter()
		.filter_map(|b| match b {
			ContentBlock::Text { text, .. } => Some(text.as_str()),
			_ => None,
		})
		.find(|t| t.contains("exceeded the per-request"));
	assert!(notice.is_some(), "omission notice must be present");
}

// ---------------------------------------------------------------------------
// compact_to_blocks end-to-end
// ---------------------------------------------------------------------------

#[test]
fn compact_to_blocks_produces_prompt_edges_and_images() {
	// Enough serialized text to overflow both text edges (2 * capHi ≈ 27,832
	// chars for anthropic 11on16-bw) and yield a small imaged middle.
	let big = "lorem ipsum dolor sit amet ".repeat(2400); // ~64,800 chars
	let messages = vec![Message::user(&big)];

	let blocks = compact_to_blocks(&messages, "claude-3", 80).unwrap();

	// [0] = summary prompt.
	assert!(text_of(&blocks[0]).contains("HISTORY"));
	assert!(text_of(&blocks[0]).contains("compact scopes"));

	// Image blocks in the middle.
	let images: Vec<&ContentBlock> =
		blocks.iter().filter(|b| matches!(b, ContentBlock::Image { .. })).collect();
	assert!(!images.is_empty(), "middle must be imaged");
	for img in &images {
		match img {
			ContentBlock::Image { media_type, data } => {
				assert_eq!(media_type, "image/png");
				assert!(!data.is_empty());
			},
			_ => unreachable!(),
		}
	}

	// Delimiters surround the images: an edge text before with "below", and an
	// edge text after with "above".
	let all_text: String = blocks
		.iter()
		.filter_map(|b| match b {
			ContentBlock::Text { text, .. } => Some(text.clone()),
			_ => None,
		})
		.collect::<Vec<_>>()
		.join("\u{1}");
	assert!(all_text.contains("imaged middle below"));
	assert!(all_text.contains("imaged middle above"));

	// Block ordering: first image is preceded by the "below" delimiter text,
	// and followed later by the "above" delimiter text.
	let first_img = blocks.iter().position(|b| matches!(b, ContentBlock::Image { .. })).unwrap();
	let last_img = blocks.iter().rposition(|b| matches!(b, ContentBlock::Image { .. })).unwrap();
	assert!(text_of(&blocks[first_img - 1]).contains("imaged middle below"));
	assert!(text_of(&blocks[last_img + 1]).contains("imaged middle above"));
}
