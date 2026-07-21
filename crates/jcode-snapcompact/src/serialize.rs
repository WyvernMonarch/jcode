//! Conversation serialization and text normalization.
//!
//! Ports `serializeConversation` (snapcompact.ts 770-899), `normalize` /
//! `normalizeWithStats` / `scanRenderability` (1085-1195), `isCjkHeavyText`
//! (388-403), `dimStopwords` (1225-1243), and the text helpers, over jcode's
//! `jcode_message_types::{Message, Role, ContentBlock}`.
//!
//! jcode ↔ omp mapping: omp's `toolResult`-role messages are jcode
//! `ContentBlock::ToolResult` blocks living inside `Role::User` messages; omp's
//! thinking blocks map to jcode `Reasoning` / `ReasoningTrace` /
//! `AnthropicThinking` / `OpenAIReasoning`; `toolCall` → `ToolUse`. jcode has no
//! `useless` result flag, so the useless-pair drop is a no-op here.
//!
//! ponytail: NFKD-based `foldToAscii` is a faithful port, but character indexing
//! uses Unicode scalar values (chars) where omp uses UTF-16 code units — the two
//! agree for every BMP code point, differing only for astral chars, which the
//! normalizer folds or drops anyway. Emoji detection approximates omp's
//! `\p{Extended_Pictographic}` with unicode-properties' emoji status; obscure
//! decorative pictographs may fall through to `?` instead of being dropped.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use jcode_message_types::{ContentBlock, Message, Role};
use regex::{Captures, Regex};
use unicode_normalization::UnicodeNormalization;
use unicode_properties::{GeneralCategory, UnicodeEmoji, UnicodeGeneralCategory};

use crate::paginate::is_wide_code_point;
use crate::render::supported_chars;

// ============================================================================
// Constants
// ============================================================================

/// Default per-tool-result character cap in serialized history.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;
/// Default per-argument-value character cap inside serialized tool calls.
pub const TOOL_ARG_MAX_CHARS: usize = 500;
/// Default character cap across one tool call's full serialized argument list.
pub const TOOL_CALL_MAX_CHARS: usize = 2000;
/// Default fraction of a truncation budget spent on the head.
pub const TRUNCATE_HEAD_RATIO: f64 = 0.6;

/// Zero-width ink toggles understood by the native renderer (shift-out/in).
pub const DIM_ON: char = '\u{000e}';
pub const DIM_OFF: char = '\u{000f}';
/// Printed in place of newline runs; the renderer fills the cell pitch-black.
pub const NEWLINE_GLYPH: char = '\u{2588}';

/// Intent argument key (pi-wire `INTENT_FIELD`).
const INTENT_FIELD: &str = "intent";

/// Char budgets applied while serializing discarded history.
#[derive(Debug, Clone)]
pub struct SerializeOptions {
	pub tool_result_max_chars: usize,
	pub tool_arg_max_chars:    usize,
	pub tool_call_max_chars:   usize,
	pub truncate_head_ratio:   f64,
	pub dim_tool_results:      bool,
}

impl Default for SerializeOptions {
	fn default() -> Self {
		Self {
			tool_result_max_chars: TOOL_RESULT_MAX_CHARS,
			tool_arg_max_chars:    TOOL_ARG_MAX_CHARS,
			tool_call_max_chars:   TOOL_CALL_MAX_CHARS,
			truncate_head_ratio:   TRUNCATE_HEAD_RATIO,
			dim_tool_results:      true,
		}
	}
}

// ============================================================================
// Truncation / dim helpers
// ============================================================================

/// Keep the head and tail of `text`, eliding the middle beyond `max_chars`.
fn truncate_for_summary(text: &str, max_chars: usize, head_ratio: f64) -> String {
	let chars: Vec<char> = text.chars().collect();
	if chars.len() <= max_chars {
		return text.to_string();
	}
	let ratio = head_ratio.clamp(0.0, 1.0);
	let head_chars = (max_chars as f64 * ratio).round() as usize;
	let tail_chars = max_chars - head_chars;
	let elided = chars.len() - max_chars;
	let head: String = chars[..head_chars].iter().collect();
	let tail: String = if tail_chars > 0 {
		chars[chars.len() - tail_chars..].iter().collect()
	} else {
		String::new()
	};
	format!("{head} […{elided}ch elided…] {tail}")
}

/// Strip stray ink toggles so raw content cannot forge dim spans.
pub fn strip_dim_markers(text: &str) -> String {
	text.chars().filter(|&c| c != DIM_ON && c != DIM_OFF).collect()
}

/// Normalized archive text → plain text: drop dim toggles, print newline glyphs
/// as real newlines.
pub fn to_plain_text(text: &str) -> String {
	strip_dim_markers(text).replace(NEWLINE_GLYPH, "\n")
}

// ============================================================================
// Serialization
// ============================================================================

fn json_stringify(value: &serde_json::Value) -> String {
	serde_json::to_string(value).unwrap_or_else(|_| "undefined".to_string())
}

fn push_part(
	parts: &mut Vec<String>,
	last_prefix: &mut Option<&'static str>,
	prefix: &'static str,
	content: &str,
) {
	if !parts.is_empty() && *last_prefix == Some(prefix) {
		let last = parts.last_mut().expect("non-empty");
		let sep = if last.ends_with('\n') || content.starts_with('\n') { "" } else { "\n" };
		last.push_str(sep);
		last.push_str(content);
	} else {
		parts.push(format!("{prefix}{content}"));
		*last_prefix = Some(prefix);
	}
}

fn flush_assistant(
	parts: &mut Vec<String>,
	last_prefix: &mut Option<&'static str>,
	pending_thinking: &mut Vec<String>,
	pending_text: &mut Vec<String>,
) {
	if !pending_thinking.is_empty() {
		push_part(parts, last_prefix, "¶think:", &pending_thinking.join("\n"));
		pending_thinking.clear();
	}
	if !pending_text.is_empty() {
		push_part(parts, last_prefix, "¶ai:", &pending_text.join("\n"));
		pending_text.clear();
	}
}

/// Serialize a conversation to the compact `¶`-scoped archive text. Ported from
/// `serializeConversation` (snapcompact.ts 770-899).
pub fn serialize_conversation(messages: &[Message], options: Option<&SerializeOptions>) -> String {
	let default_opts = SerializeOptions::default();
	let o = options.unwrap_or(&default_opts);

	// Pass 1: index tool-result text by call id (jcode has no `useless` flag).
	let mut result_text_by_call_id: HashMap<String, String> = HashMap::new();
	for msg in messages {
		for block in &msg.content {
			if let ContentBlock::ToolResult { tool_use_id, content, .. } = block {
				if !content.is_empty() {
					result_text_by_call_id.insert(tool_use_id.clone(), content.clone());
				}
			}
		}
	}

	let render_result_block = |raw: &str| -> String {
		let body = truncate_for_summary(
			&strip_dim_markers(raw),
			o.tool_result_max_chars,
			o.truncate_head_ratio,
		);
		if o.dim_tool_results {
			format!("<out>\n{DIM_ON}{body}{DIM_OFF}\n</out>")
		} else {
			format!("<out>\n{body}\n</out>")
		}
	};

	let mut parts: Vec<String> = Vec::new();
	let mut last_prefix: Option<&'static str> = None;
	let mut merged_call_ids: HashSet<String> = HashSet::new();

	for msg in messages {
		match msg.role {
			Role::Assistant => {
				let mut pending_thinking: Vec<String> = Vec::new();
				let mut pending_text: Vec<String> = Vec::new();
				for block in &msg.content {
					match block {
						ContentBlock::Text { text, .. } => {
							let t = strip_dim_markers(text);
							if !t.trim().is_empty() {
								pending_text.push(t);
							}
						},
						ContentBlock::Reasoning { text }
						| ContentBlock::ReasoningTrace { text } => {
							let t = strip_dim_markers(text);
							if !t.trim().is_empty() {
								pending_thinking.push(t);
							}
						},
						ContentBlock::AnthropicThinking { thinking, .. } => {
							let t = strip_dim_markers(thinking);
							if !t.trim().is_empty() {
								pending_thinking.push(t);
							}
						},
						ContentBlock::OpenAIReasoning { summary, .. } => {
							let t = strip_dim_markers(&summary.join("\n"));
							if !t.trim().is_empty() {
								pending_thinking.push(t);
							}
						},
						ContentBlock::ToolUse { id, name, input, .. } => {
							flush_assistant(
								&mut parts,
								&mut last_prefix,
								&mut pending_thinking,
								&mut pending_text,
							);
							let raw_intent =
								input.get(INTENT_FIELD).and_then(|v| v.as_str()).unwrap_or("");
							let intent = strip_dim_markers(raw_intent)
								.split_whitespace()
								.collect::<Vec<_>>()
								.join(" ");
							let entries: Vec<String> = match input.as_object() {
								Some(map) => map
									.iter()
									.filter(|(k, _)| k.as_str() != INTENT_FIELD)
									.map(|(k, v)| {
										format!(
											"{k}={}",
											truncate_for_summary(
												&json_stringify(v),
												o.tool_arg_max_chars,
												o.truncate_head_ratio,
											)
										)
									})
									.collect(),
								None => Vec::new(),
							};
							let args_str = truncate_for_summary(
								&entries.join(", "),
								o.tool_call_max_chars,
								o.truncate_head_ratio,
							);
							let mut first_line = format!("{name}({args_str})");
							if !intent.is_empty() {
								first_line.push_str(&format!("//{intent}"));
							}
							let mut lines = vec![first_line];
							if let Some(rt) = result_text_by_call_id.get(id) {
								merged_call_ids.insert(id.clone());
								lines.push(render_result_block(rt));
							}
							push_part(&mut parts, &mut last_prefix, "¶call:", &lines.join("\n"));
						},
						_ => {},
					}
				}
				flush_assistant(
					&mut parts,
					&mut last_prefix,
					&mut pending_thinking,
					&mut pending_text,
				);
			},
			Role::User => {
				let user_text: String = msg
					.content
					.iter()
					.filter_map(|b| match b {
						ContentBlock::Text { text, .. } => Some(text.as_str()),
						_ => None,
					})
					.collect::<Vec<_>>()
					.join("");
				if !user_text.is_empty() {
					push_part(&mut parts, &mut last_prefix, "¶user:", &strip_dim_markers(&user_text));
				}
				for block in &msg.content {
					if let ContentBlock::ToolResult { tool_use_id, .. } = block {
						if merged_call_ids.contains(tool_use_id) {
							continue;
						}
						if let Some(rt) = result_text_by_call_id.get(tool_use_id) {
							push_part(
								&mut parts,
								&mut last_prefix,
								"¶call:",
								&format!("\n{}", render_result_block(rt)),
							);
						}
					}
				}
			},
		}
	}

	parts.join("\n\n")
}

// ============================================================================
// Text normalization
// ============================================================================

const fn is_ascii_or_latin1(cp: u32) -> bool {
	(cp >= 0x20 && cp < 0x7f) || (cp >= 0xa0 && cp <= 0xff)
}

fn is_format(ch: char) -> bool {
	ch.general_category() == GeneralCategory::Format
}

/// \p{M}: any combining/spacing/enclosing mark.
fn is_mark(ch: char) -> bool {
	matches!(
		ch.general_category(),
		GeneralCategory::NonspacingMark | GeneralCategory::SpacingMark | GeneralCategory::EnclosingMark
	)
}

/// UNRENDERABLE = [\p{Cc}\p{Mn}\p{Me}\p{Cs}].
fn is_unrenderable(ch: char) -> bool {
	matches!(
		ch.general_category(),
		GeneralCategory::Control
			| GeneralCategory::NonspacingMark
			| GeneralCategory::EnclosingMark
			| GeneralCategory::Surrogate
	)
}

/// Approximates omp's `\p{Extended_Pictographic}`.
fn is_pictographic(ch: char) -> bool {
	ch.is_emoji_char_or_emoji_component()
}

fn is_collapsible(ch: char) -> bool {
	ch.is_whitespace() || is_format(ch)
}

fn is_line_break(ch: char) -> bool {
	matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

static ANSI: LazyLock<Regex> = LazyLock::new(|| {
	// CSI, OSC (BEL- or ST-terminated), and simple two-char escapes.
	Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]")
		.expect("static ANSI regex")
});

/// Trim leading/trailing spaces or newline glyphs (EDGE_RUNS).
fn strip_edge_runs(s: &str) -> String {
	s.trim_matches(|c| c == ' ' || c == NEWLINE_GLYPH).to_string()
}

/// Strip ANSI, collapse whitespace/format runs, drop edge runs. Ported from
/// `normalizedInputChars` (snapcompact.ts 1085-1093).
fn normalized_input_chars(text: &str) -> String {
	let stripped: std::borrow::Cow<str> =
		if text.contains('\u{1b}') { ANSI.replace_all(text, "") } else { text.into() };
	let cs: Vec<char> = stripped.chars().collect();
	let mut out = String::new();
	let mut i = 0;
	while i < cs.len() {
		if is_collapsible(cs[i]) {
			let start = i;
			while i < cs.len() && is_collapsible(cs[i]) {
				i += 1;
			}
			let run = &cs[start..i];
			if run.iter().copied().any(is_line_break) {
				out.push(NEWLINE_GLYPH);
			} else if run.iter().any(|&c| !is_format(c)) {
				out.push(' ');
			}
			// pure format run collapses to nothing
		} else {
			out.push(cs[i]);
			i += 1;
		}
	}
	strip_edge_runs(&out)
}

static CHAR_FOLD: LazyLock<HashMap<char, &'static str>> = LazyLock::new(|| {
	HashMap::from([
		('\u{2018}', "'"),
		('\u{2019}', "'"),
		('\u{201a}', "'"),
		('\u{201b}', "'"),
		('\u{201c}', "\""),
		('\u{201d}', "\""),
		('\u{201e}', "\""),
		('\u{2032}', "'"),
		('\u{2033}', "\""),
		('\u{2035}', "'"),
		('\u{2036}', "\""),
		('\u{2039}', "<"),
		('\u{203a}', ">"),
		('\u{2010}', "-"),
		('\u{2011}', "-"),
		('\u{2012}', "-"),
		('\u{2013}', "-"),
		('\u{2014}', "-"),
		('\u{2015}', "-"),
		('\u{2212}', "-"),
		('\u{2044}', "/"),
		('\u{2024}', "."),
		('\u{2025}', ".."),
		('\u{2026}', "..."),
		('\u{22ef}', "..."),
		('\u{2022}', "*"),
		('\u{2023}', "*"),
		('\u{2043}', "-"),
		('\u{2219}', "*"),
		('\u{25cf}', "*"),
		('\u{25a0}', "*"),
		('\u{25aa}', "*"),
		('\u{2190}', "<-"),
		('\u{2191}', "^"),
		('\u{2192}', "->"),
		('\u{2193}', "v"),
		('\u{2194}', "<->"),
		('\u{21d0}', "<="),
		('\u{21d2}', "=>"),
		('\u{21d4}', "<=>"),
		('\u{2713}', "v"),
		('\u{2714}', "v"),
		('\u{2717}', "x"),
		('\u{2718}', "x"),
	])
});

static EMOJI_FOLD: LazyLock<HashMap<char, &'static str>> = LazyLock::new(|| {
	HashMap::from([
		('\u{2705}', "[OK]"),
		('\u{2611}', "[OK]"),
		('\u{2714}', "[OK]"),
		('\u{274c}', "[FAIL]"),
		('\u{274e}', "[FAIL]"),
		('\u{2716}', "[FAIL]"),
		('\u{26a0}', "[WARN]"),
		('\u{1f6a8}', "[ALERT]"),
		('\u{2139}', "[INFO]"),
		('\u{1f41b}', "[BUG]"),
		('\u{1f4a5}', "[CRASH]"),
		('\u{1f525}', "[HOT]"),
		('\u{1f512}', "[LOCK]"),
		('\u{1f513}', "[UNLOCK]"),
		('\u{1f4c1}', "[DIR]"),
		('\u{1f4c2}', "[DIR]"),
		('\u{1f4c4}', "[FILE]"),
		('\u{1f4dd}', "[NOTE]"),
		('\u{1f9ea}', "[TEST]"),
		('\u{23f3}', "[WAIT]"),
		('\u{231b}', "[WAIT]"),
		('\u{1f680}', "[RUN]"),
	])
});

fn is_box_drawing(cp: u32) -> bool {
	(0x2500..=0x257f).contains(&cp)
}

/// NFKD compatibility fold to the ASCII/Latin-1 skeleton. Ported from
/// `foldToAscii` (1057-1072).
fn fold_to_ascii(ch: char) -> Option<String> {
	let src = ch.to_string();
	let decomposed: String = src.nfkd().filter(|c| !is_mark(*c)).collect();
	if decomposed == src {
		return None;
	}
	let mut out = String::new();
	for part in decomposed.chars() {
		if is_ascii_or_latin1(part as u32) {
			out.push(part);
			continue;
		}
		match CHAR_FOLD.get(&part) {
			Some(f) => out.push_str(f),
			None => return None,
		}
	}
	Some(out)
}

/// Unique non-ASCII chars needing a font-support check (mirrors
/// `candidateUnicodeChars`, 1095-1115).
fn candidate_chars(chars: &str) -> Vec<char> {
	let mut seen: HashSet<char> = HashSet::new();
	let mut unique: Vec<char> = Vec::new();
	for ch in chars.chars() {
		let cp = ch as u32;
		if is_ascii_or_latin1(cp) || ch == DIM_ON || ch == DIM_OFF || ch == NEWLINE_GLYPH {
			continue;
		}
		if CHAR_FOLD.contains_key(&ch)
			|| is_box_drawing(cp)
			|| EMOJI_FOLD.contains_key(&ch)
			|| is_pictographic(ch)
			|| fold_to_ascii(ch).is_some()
			|| is_unrenderable(ch)
		{
			continue;
		}
		if seen.insert(ch) {
			unique.push(ch);
		}
	}
	unique
}

/// Chars from `candidates` the named font (or the Silver fallback) can render.
fn renderable_unicode_chars(candidates: &[char], font: Option<&str>) -> HashSet<char> {
	if candidates.is_empty() {
		return HashSet::new();
	}
	let text: String = candidates.iter().collect();
	let primary = font.unwrap_or("5x8");
	let mut supported: HashSet<char> =
		supported_chars(primary, &text).unwrap_or_default().chars().collect();
	if primary != "silver" {
		for ch in supported_chars("silver", &text).unwrap_or_default().chars() {
			supported.insert(ch);
		}
	}
	supported
}

/// Collapse runs of spaces to one (`/ +/g → " "`).
fn collapse_spaces(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	let mut prev_space = false;
	for ch in s.chars() {
		if ch == ' ' {
			if !prev_space {
				out.push(' ');
			}
			prev_space = true;
		} else {
			out.push(ch);
			prev_space = false;
		}
	}
	out
}

struct NormalizedText {
	text:           String,
	total_graphics: usize,
	fallback_count: usize,
}

/// Ported from `normalizeWithStats` (1117-1172).
fn normalize_with_stats(text: &str, font: Option<&str>) -> NormalizedText {
	let chars = normalized_input_chars(text);
	let supported = renderable_unicode_chars(&candidate_chars(&chars), font);
	let mut out = String::new();
	let mut total_graphics = 0usize;
	let mut fallback_count = 0usize;

	for ch in chars.chars() {
		let cp = ch as u32;
		if is_ascii_or_latin1(cp) {
			out.push(ch);
			total_graphics += 1;
			continue;
		}
		if ch == DIM_ON || ch == DIM_OFF || ch == NEWLINE_GLYPH {
			out.push(ch);
			continue;
		}
		if let Some(e) = EMOJI_FOLD.get(&ch) {
			out.push_str(e);
			total_graphics += 1;
			continue;
		}
		if let Some(f) = CHAR_FOLD.get(&ch) {
			out.push_str(f);
			total_graphics += 1;
			continue;
		}
		if is_box_drawing(cp) {
			out.push(if cp == 0x2502 || cp == 0x2503 {
				'|'
			} else if cp == 0x2500 || cp == 0x2501 {
				'-'
			} else {
				'+'
			});
			total_graphics += 1;
			continue;
		}
		if !is_pictographic(ch) && supported.contains(&ch) {
			out.push(ch);
			total_graphics += 1;
			continue;
		}
		if let Some(folded) = fold_to_ascii(ch) {
			out.push_str(&folded);
			total_graphics += 1;
		} else if is_pictographic(ch) {
			// decorative pictograph: drop
		} else if !is_unrenderable(ch) {
			out.push('?');
			total_graphics += 1;
			fallback_count += 1;
		}
	}

	let text = strip_edge_runs(&collapse_spaces(&out));
	NormalizedText { text, total_graphics, fallback_count }
}

/// Prepare text for printing (font-aware). `font` is the shape's font name.
pub fn normalize(text: &str, font: Option<&str>) -> String {
	normalize_with_stats(text, font).text
}

/// Result of a font-aware renderability scan.
#[derive(Debug, Clone, Copy)]
pub struct Renderability {
	pub is_safe:            bool,
	pub unrenderable_ratio: f64,
}

/// Scan text with the same font-aware path as `normalize`; unsafe means more
/// than 5% of graphic characters would hit the `?` fallback.
pub fn scan_renderability(text: &str, font: &str) -> Renderability {
	let n = normalize_with_stats(text, Some(font));
	let ratio =
		if n.total_graphics > 0 { n.fallback_count as f64 / n.total_graphics as f64 } else { 0.0 };
	Renderability { is_safe: ratio <= 0.05, unrenderable_ratio: ratio }
}

const CJK_HEAVY_MIN_WIDE_CHARS: usize = 8;
const CJK_HEAVY_WIDE_RATIO: f64 = 0.25;

/// Ported from `isCjkHeavyText` (391-403).
pub fn is_cjk_heavy_text(text: &str) -> bool {
	let chars = normalized_input_chars(text);
	let mut graphic = 0usize;
	let mut wide = 0usize;
	for ch in chars.chars() {
		if ch == ' ' || ch == DIM_ON || ch == DIM_OFF || ch == NEWLINE_GLYPH {
			continue;
		}
		if is_unrenderable(ch) {
			continue;
		}
		graphic += 1;
		if is_wide_code_point(ch as u32) {
			wide += 1;
		}
	}
	graphic > 0
		&& wide >= CJK_HEAVY_MIN_WIDE_CHARS
		&& (wide as f64 / graphic as f64) >= CJK_HEAVY_WIDE_RATIO
}

// ============================================================================
// Stopword dimming
// ============================================================================

static STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
	("the a an and or of to in on at as is are was were be been by for with that this it its from had \
	  has have not but he she his her they their them which also who whom when where while will would \
	  could should there then than into over under about after before between during each such these \
	  those some most more other only same so")
		.split(' ')
		.collect()
});

static ALPHA_RUN: LazyLock<Regex> =
	LazyLock::new(|| Regex::new(r"[a-zA-Z\x{C0}-\x{D6}\x{D8}-\x{F6}\x{F8}-\x{FF}]+").expect("alpha run"));

fn wrap_stopwords(seg: &str) -> String {
	ALPHA_RUN
		.replace_all(seg, |caps: &Captures| {
			let w = &caps[0];
			if STOPWORDS.contains(w.to_lowercase().as_str()) {
				format!("{DIM_ON}{w}{DIM_OFF}")
			} else {
				w.to_string()
			}
		})
		.into_owned()
}

/// Wrap each stopword alphabetic run in dim toggles. Spans already dim pass
/// through untouched. Ported from `dimStopwords` (1225-1243).
pub fn dim_stopwords(text: &str) -> String {
	let mut out = String::new();
	let mut dim = false;
	let mut seg = String::new();
	for ch in text.chars() {
		if ch == DIM_ON || ch == DIM_OFF {
			out.push_str(&if dim { std::mem::take(&mut seg) } else { wrap_stopwords(&std::mem::take(&mut seg)) });
			out.push(ch);
			dim = ch == DIM_ON;
		} else {
			seg.push(ch);
		}
	}
	out.push_str(&if dim { seg } else { wrap_stopwords(&seg) });
	out
}
