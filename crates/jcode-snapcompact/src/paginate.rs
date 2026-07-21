//! Frame geometry and cell-aware pagination.
//!
//! Ported from `snapcompact.ts` (geometry 1403-1411, paginateCells 1318-1339,
//! wrap 1347-1381, docPages 1389-1397, renderedChars 1428-1449). The cell math
//! MUST mirror the native renderer's `is_wide`/`cell_units`/`place_cell` in
//! `render.rs`, or capacity accounting and layout disagree on cell counts.

use crate::serialize::{DIM_OFF, DIM_ON};
use crate::shapes::Shape;

/// Character cells between the two doc columns (research exp14 `GUTTER`).
pub const DOC_GUTTER: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
	/// Characters per row (per-column line width when `columns == 2`).
	pub cols:     usize,
	pub rows:     usize,
	/// Characters that fit one frame.
	pub capacity: usize,
}

/// Frame grid geometry for a shape at a given frame size. Mirrors `geometry()`
/// in snapcompact.ts.
pub fn geometry(shape: &Shape, size: u32) -> Geometry {
	let grid_cols = (size / shape.cell_width) as usize;
	let rows = (size / shape.cell_height / shape.line_repeat) as usize;
	if shape.columns == Some(2) {
		let cols = grid_cols.saturating_sub(DOC_GUTTER) / 2;
		Geometry { cols, rows, capacity: 2 * cols * rows }
	} else {
		Geometry { cols: grid_cols, rows, capacity: grid_cols * rows }
	}
}

/// Geometry at the shape's own frame size.
pub fn geometry_default(shape: &Shape) -> Geometry {
	geometry(shape, shape.frame_size)
}

/// East Asian Wide / Fullwidth code points that occupy two grid cells when a
/// narrow bitmap shape draws them through the Silver fallback. MUST stay in
/// sync with `is_wide` in `render.rs`.
pub fn is_wide_code_point(cp: u32) -> bool {
	matches!(cp,
		0x1100..=0x115F
		| 0x2E80..=0x2EFF
		| 0x2F00..=0x2FDF
		| 0x3000..=0x303E
		| 0x3041..=0x33FF
		| 0x3400..=0x4DBF
		| 0x4E00..=0x9FFF
		| 0xA000..=0xA4CF
		| 0xAC00..=0xD7A3
		| 0xF900..=0xFAFF
		| 0xFE30..=0xFE4F
		| 0xFF00..=0xFF60
		| 0xFFE0..=0xFFE6
		| 0x20000..=0x2FFFD
		| 0x30000..=0x3FFFD
	)
}

/// Cells one character occupies: 0 for the zero-width dim toggles, 2 for wide
/// code points in narrow bitmap shapes, 1 otherwise. Mirrors native `cell_units`.
pub fn char_cells(ch: char, wide_cells: bool) -> usize {
	if ch == DIM_ON || ch == DIM_OFF {
		return 0;
	}
	if wide_cells && is_wide_code_point(ch as u32) {
		2
	} else {
		1
	}
}

/// Wide code points span two cells in every shape except the square-celled
/// Silver shape, which sizes each cell for a full-width glyph already.
pub fn uses_wide_cells(shape: &Shape) -> bool {
	shape.font != "silver"
}

/// Total grid cells a string occupies (ignoring row wrapping/pads).
pub fn cell_length(text: &str, wide_cells: bool) -> usize {
	text.chars().map(|ch| char_cells(ch, wide_cells)).sum()
}

/// Longest prefix of `text` that fits `width` cells (at least one char).
pub fn slice_cells(text: &str, width: usize, wide_cells: bool) -> String {
	let mut cells = 0usize;
	let mut out = String::new();
	let mut placed = false;
	for ch in text.chars() {
		let w = char_cells(ch, wide_cells);
		if placed && cells + w > width {
			break;
		}
		out.push(ch);
		cells += w;
		if w > 0 {
			placed = true;
		}
	}
	out
}

/// Split `text` into pages that each fill at most `capacity` grid cells,
/// inserting a one-cell pad before a wide glyph that would straddle the right
/// edge (mirrors native `place_cell`). Pages are contiguous substrings.
pub fn paginate_cells(text: &str, capacity: usize, cols: usize, wide_cells: bool) -> Vec<String> {
	let chars: Vec<char> = text.chars().collect();
	let mut pages: Vec<String> = Vec::new();
	let mut start = 0usize;
	let mut cell = 0usize;
	let mut has_cell = false;
	for i in 0..chars.len() {
		let w = char_cells(chars[i], wide_cells);
		if w == 0 {
			continue;
		}
		let mut at = cell;
		if w == 2 && cols >= 2 && at % cols == cols - 1 {
			at += 1;
		}
		if has_cell && at + w > capacity {
			pages.push(chars[start..i].iter().collect());
			start = i;
			at = 0;
		}
		cell = at + w;
		has_cell = true;
	}
	if has_cell {
		pages.push(chars[start..].iter().collect());
	}
	pages
}

/// Greedy word-wrap, no mid-word breaks (hard split only for width+ words).
/// Ported from `research/exp14_bestgpt.py` `wrap()`.
pub fn wrap(text: &str, width: usize, wide_cells: bool) -> Vec<String> {
	let mut lines: Vec<String> = Vec::new();
	let mut cur = String::new();
	let mut cur_cells = 0usize;
	for token in text.split_whitespace() {
		if token.is_empty() {
			continue;
		}
		let mut word = token.to_string();
		let mut word_cells = cell_length(&word, wide_cells);
		while word_cells > width {
			// Pathological; never hit on prose.
			if !cur.is_empty() {
				lines.push(std::mem::take(&mut cur));
				cur_cells = 0;
			}
			let head = slice_cells(&word, width, wide_cells);
			let head_len = head.len();
			lines.push(head);
			word = word[head_len..].to_string();
			word_cells = cell_length(&word, wide_cells);
		}
		if cur.is_empty() {
			cur = word;
			cur_cells = word_cells;
		} else if cur_cells + 1 + word_cells <= width {
			cur.push(' ');
			cur.push_str(&word);
			cur_cells += 1 + word_cells;
		} else {
			lines.push(std::mem::take(&mut cur));
			cur = word;
			cur_cells = word_cells;
		}
	}
	if !cur.is_empty() {
		lines.push(cur);
	}
	lines
}

/// Paginate already-normalized text for a doc shape: wrap once at the column
/// width, then slice into pages of `2 * rows` lines, each page `\n`-joined.
pub fn doc_pages(normalized: &str, geo: Geometry, wide_cells: bool) -> Vec<String> {
	let lines = wrap(normalized, geo.cols, wide_cells);
	let per_page = 2 * geo.rows;
	if per_page == 0 {
		return Vec::new();
	}
	let mut pages: Vec<String> = Vec::new();
	let mut offset = 0usize;
	while offset < lines.len() {
		let end = (offset + per_page).min(lines.len());
		pages.push(lines[offset..end].join("\n"));
		offset += per_page;
	}
	pages
}

/// Characters actually printed onto one frame (ink toggles/newlines excluded),
/// with wide glyphs taking two cells exactly as the renderer. Mirrors
/// `renderedChars` in snapcompact.ts.
pub fn rendered_chars(text: &str, shape: &Shape, geo: Geometry) -> usize {
	if shape.columns == Some(2) {
		let mut visible = text.chars().count();
		visible -= text.chars().filter(|&c| c == DIM_ON || c == DIM_OFF).count();
		visible -= text.chars().filter(|&c| c == '\n').count();
		return visible.min(geo.capacity);
	}
	let wide_cells = uses_wide_cells(shape);
	let mut cell = 0usize;
	let mut count = 0usize;
	for ch in text.chars() {
		let w = char_cells(ch, wide_cells);
		if w == 0 {
			continue;
		}
		let mut at = cell;
		if w == 2 && geo.cols >= 2 && at % geo.cols == geo.cols - 1 {
			at += 1;
		}
		if at + w > geo.capacity {
			break;
		}
		cell = at + w;
		count += 1;
	}
	count
}
