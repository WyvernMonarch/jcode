//! Hashline: line-anchored patch language ported from omp (`@oh-my-pi/hashline`).
//!
//! A patch anchors every op to 1-indexed line numbers of the ORIGINAL file and
//! carries a whole-file content tag (`#TAG`) so the applier can cheaply detect
//! that the file drifted since the model read it. See `OMP_MERGE_PLAN.md`
//! "hashline spec" for the exact contract.
//!
//! Public surface:
//! - [`compute_tag`] — whole-file content tag (uppercase hex4 of xxHash32 & 0xFFFF).
//! - [`parse`] — patch text -> `Vec<FileSection>`.
//! - [`apply`] — pure op application on a given text (validates ranges + overlaps).
//! - [`apply_section`] — tag-checked application with snapshot-based stale recovery.
//! - [`SnapshotStore`] — per-path LRU of file snapshots for stale-tag recovery.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// xxHash32 (inline, standard XXH32, seed 0) — verified against the official
// empty-string vector 0x02CC5D05 in tests.
// ---------------------------------------------------------------------------

const PRIME32_1: u32 = 0x9E37_79B1;
const PRIME32_2: u32 = 0x85EB_CA77;
const PRIME32_3: u32 = 0xC2B2_AE3D;
const PRIME32_4: u32 = 0x27D4_EB2F;
const PRIME32_5: u32 = 0x1656_67B1;

#[inline]
fn round32(acc: u32, input: u32) -> u32 {
    acc.wrapping_add(input.wrapping_mul(PRIME32_2))
        .rotate_left(13)
        .wrapping_mul(PRIME32_1)
}

#[inline]
fn read_u32_le(bytes: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
}

/// Standard XXH32 with the given seed.
pub fn xxh32(input: &[u8], seed: u32) -> u32 {
    let len = input.len();
    let mut idx = 0usize;
    let mut h32: u32;

    if len >= 16 {
        let limit = len - 16;
        let mut v1 = seed.wrapping_add(PRIME32_1).wrapping_add(PRIME32_2);
        let mut v2 = seed.wrapping_add(PRIME32_2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(PRIME32_1);
        loop {
            v1 = round32(v1, read_u32_le(input, idx));
            v2 = round32(v2, read_u32_le(input, idx + 4));
            v3 = round32(v3, read_u32_le(input, idx + 8));
            v4 = round32(v4, read_u32_le(input, idx + 12));
            idx += 16;
            if idx > limit {
                break;
            }
        }
        h32 = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
    } else {
        h32 = seed.wrapping_add(PRIME32_5);
    }

    h32 = h32.wrapping_add(len as u32);

    while idx + 4 <= len {
        h32 = h32.wrapping_add(read_u32_le(input, idx).wrapping_mul(PRIME32_3));
        h32 = h32.rotate_left(17).wrapping_mul(PRIME32_4);
        idx += 4;
    }
    while idx < len {
        h32 = h32.wrapping_add((input[idx] as u32).wrapping_mul(PRIME32_5));
        h32 = h32.rotate_left(11).wrapping_mul(PRIME32_1);
        idx += 1;
    }

    h32 ^= h32 >> 15;
    h32 = h32.wrapping_mul(PRIME32_2);
    h32 ^= h32 >> 13;
    h32 = h32.wrapping_mul(PRIME32_3);
    h32 ^= h32 >> 16;
    h32
}

/// Normalize text for hashing: strip trailing `[ \t\r]` from every line, rejoin
/// with `\n`. This makes the tag insensitive to trailing whitespace and CRLF.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut first = true;
    for line in text.split('\n') {
        if !first {
            out.push('\n');
        }
        first = false;
        out.push_str(line.trim_end_matches([' ', '\t', '\r']));
    }
    out
}

/// Whole-file content tag: `uppercase hex4( xxHash32(normalized, seed=0) & 0xFFFF )`.
pub fn compute_tag(text: &str) -> String {
    let h = xxh32(normalize(text).as_bytes(), 0) & 0xFFFF;
    format!("{h:04X}")
}

// ---------------------------------------------------------------------------
// Patch model + parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `SWAP A.=B:` replace lines A..=B with `body`.
    Swap { start: usize, end: usize, body: Vec<String> },
    /// `SWAP.BLK A:` replace the indentation block starting at line A with `body`.
    SwapBlk { start: usize, body: Vec<String> },
    /// `DEL A.=B` / `DEL A` delete lines A..=B (end == start for the single-line form).
    Del { start: usize, end: usize },
    /// `DEL.BLK A` delete the indentation block starting at line A.
    DelBlk { start: usize },
    /// `INS.PRE A:` insert `body` before line A.
    InsPre { line: usize, body: Vec<String> },
    /// `INS.POST A:` insert `body` after line A.
    InsPost { line: usize, body: Vec<String> },
    /// `INS.HEAD:` insert `body` at the top of the file.
    InsHead { body: Vec<String> },
    /// `INS.TAIL:` insert `body` at the end of the file.
    InsTail { body: Vec<String> },
    /// `INS.BLK.POST A:` insert `body` after the indentation block starting at line A.
    InsBlkPost { line: usize, body: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSection {
    /// Path exactly as written in the `[path#TAG]` header.
    pub path: String,
    /// Uppercased content tag from the header.
    pub tag: String,
    pub ops: Vec<Op>,
}

/// Does this op accept a `+body`? (the `:`-terminated forms do; DEL forms do not).
fn op_takes_body(op: &Op) -> bool {
    !matches!(op, Op::Del { .. } | Op::DelBlk { .. })
}

fn parse_num(s: &str) -> Result<usize> {
    let n: usize = s
        .trim()
        .parse()
        .map_err(|_| anyhow!("hashline: invalid line number `{s}`"))?;
    if n == 0 {
        bail!("hashline: line numbers are 1-indexed, got 0");
    }
    Ok(n)
}

/// Parse an `A.=B` range (or a bare `A`, yielding start == end).
fn parse_range(s: &str) -> Result<(usize, usize)> {
    if let Some((a, b)) = s.split_once(".=") {
        let a = parse_num(a)?;
        let b = parse_num(b)?;
        if b < a {
            bail!("hashline: range end {b} is before start {a}");
        }
        Ok((a, b))
    } else {
        let a = parse_num(s)?;
        Ok((a, a))
    }
}

/// Parse an op header line (the line that introduces an op, before its `+body`).
fn parse_op_line(line: &str) -> Result<Op> {
    let line = line.trim_end();
    // `:`-terminated forms carry a body; strip the trailing colon for arg parsing.
    fn strip_colon(s: &str) -> Result<&str> {
        s.strip_suffix(':')
            .ok_or_else(|| anyhow!("hashline: op `{s}` must end with ':'"))
    }

    if let Some(rest) = line.strip_prefix("SWAP.BLK ") {
        let a = parse_num(strip_colon(rest)?)?;
        Ok(Op::SwapBlk { start: a, body: Vec::new() })
    } else if let Some(rest) = line.strip_prefix("SWAP ") {
        let (a, b) = parse_range(strip_colon(rest)?)?;
        Ok(Op::Swap { start: a, end: b, body: Vec::new() })
    } else if let Some(rest) = line.strip_prefix("DEL.BLK ") {
        Ok(Op::DelBlk { start: parse_num(rest)? })
    } else if let Some(rest) = line.strip_prefix("DEL ") {
        let (a, b) = parse_range(rest)?;
        Ok(Op::Del { start: a, end: b })
    } else if let Some(rest) = line.strip_prefix("INS.PRE ") {
        Ok(Op::InsPre { line: parse_num(strip_colon(rest)?)?, body: Vec::new() })
    } else if let Some(rest) = line.strip_prefix("INS.BLK.POST ") {
        Ok(Op::InsBlkPost { line: parse_num(strip_colon(rest)?)?, body: Vec::new() })
    } else if let Some(rest) = line.strip_prefix("INS.POST ") {
        Ok(Op::InsPost { line: parse_num(strip_colon(rest)?)?, body: Vec::new() })
    } else if line == "INS.HEAD:" {
        Ok(Op::InsHead { body: Vec::new() })
    } else if line == "INS.TAIL:" {
        Ok(Op::InsTail { body: Vec::new() })
    } else {
        bail!("hashline: unrecognized op line `{line}`");
    }
}

/// Parse a `[path#TAG]` section header. Returns `(path, TAG)` or `None`.
fn parse_header(line: &str) -> Option<(String, String)> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (path, tag) = inner.rsplit_once('#')?;
    if path.is_empty() || tag.is_empty() {
        return None;
    }
    Some((path.to_string(), tag.to_uppercase()))
}

/// Parse a full patch into one or more file sections.
pub fn parse(patch_text: &str) -> Result<Vec<FileSection>> {
    let mut sections: Vec<FileSection> = Vec::new();
    let mut pending: Option<Op> = None;

    // Commit the in-progress op (with any collected body) to the last section.
    fn flush(pending: &mut Option<Op>, sections: &mut [FileSection]) {
        if let Some(op) = pending.take() {
            if let Some(sec) = sections.last_mut() {
                sec.ops.push(op);
            }
        }
    }

    for raw in patch_text.split('\n') {
        if let Some((path, tag)) = parse_header(raw) {
            flush(&mut pending, &mut sections);
            sections.push(FileSection { path, tag, ops: Vec::new() });
            continue;
        }

        if let Some(body_line) = raw.strip_prefix('+') {
            let op = pending
                .as_mut()
                .ok_or_else(|| anyhow!("hashline: `+body` line with no preceding op"))?;
            if !op_takes_body(op) {
                bail!("hashline: DEL ops take no `+body` line");
            }
            match op {
                Op::Swap { body, .. }
                | Op::SwapBlk { body, .. }
                | Op::InsPre { body, .. }
                | Op::InsPost { body, .. }
                | Op::InsHead { body }
                | Op::InsTail { body }
                | Op::InsBlkPost { body, .. } => body.push(body_line.to_string()),
                _ => unreachable!(),
            }
            continue;
        }

        if raw.trim().is_empty() {
            // Blank separator between ops; a blank body line is written as a bare `+`.
            continue;
        }

        // A new op line: commit the previous op first.
        flush(&mut pending, &mut sections);
        if sections.is_empty() {
            bail!("hashline: op `{}` appears before any `[path#TAG]` header", raw.trim());
        }
        pending = Some(parse_op_line(raw)?);
    }
    flush(&mut pending, &mut sections);

    if sections.is_empty() {
        bail!("hashline: patch has no `[path#TAG]` section header");
    }
    Ok(sections)
}

// ---------------------------------------------------------------------------
// Line splitting / block detection
// ---------------------------------------------------------------------------

/// Split into lines preserving content exactly. Returns (lines, trailing_newline).
/// A CRLF line keeps its `\r` (unchanged lines round-trip byte-for-byte); new
/// body lines are LF-only. ponytail: LF-normalizing new lines is fine for source.
fn split_lines(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let trailing = text.ends_with('\n');
    let body = if trailing { &text[..text.len() - 1] } else { text };
    (body.split('\n').collect(), trailing)
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn indent_width(line: &str) -> usize {
    // ponytail: tab counted as one column; good enough for block detection.
    line.chars().take_while(|c| *c == ' ' || *c == '\t').count()
}

/// 1-indexed inclusive end of the indentation block that starts at 1-indexed
/// line `start`. The block is `start` plus following more-indented lines;
/// blank lines belong to the block only when a deeper line follows them.
fn block_end(lines: &[&str], start: usize) -> usize {
    let base = indent_width(lines[start - 1]);
    let mut end = start; // block always includes its own start line
    let mut i = start; // 0-indexed position of line `start+1`
    while i < lines.len() {
        let line = lines[i];
        if is_blank(line) {
            i += 1;
            continue; // tentatively part of the block; committed only by a deeper line
        }
        if indent_width(line) > base {
            end = i + 1;
            i += 1;
        } else {
            break;
        }
    }
    end
}

// ---------------------------------------------------------------------------
// Pure application: apply ops against a text (validates ranges + overlaps)
// ---------------------------------------------------------------------------

/// One op reduced to concrete original-line coordinates.
enum Effect {
    /// Delete original lines `a..=b` (1-indexed); optionally insert `body` at the
    /// boundary just before line `a`.
    Cut { a: usize, b: usize, body: Option<Vec<String>> },
    /// Insert `body` at `boundary` (0 = before line 1, N = after last line).
    Insert { boundary: usize, body: Vec<String> },
}

fn resolve_effect(op: &Op, lines: &[&str]) -> Result<Effect> {
    let n = lines.len();
    let check = |a: usize| -> Result<()> {
        if a > n {
            bail!("hashline: line {a} is out of range (file has {n} lines)");
        }
        Ok(())
    };
    Ok(match op {
        Op::Swap { start, end, body } => {
            check(*start)?;
            check(*end)?;
            Effect::Cut { a: *start, b: *end, body: Some(body.clone()) }
        }
        Op::SwapBlk { start, body } => {
            check(*start)?;
            let end = block_end(lines, *start);
            Effect::Cut { a: *start, b: end, body: Some(body.clone()) }
        }
        Op::Del { start, end } => {
            check(*start)?;
            check(*end)?;
            Effect::Cut { a: *start, b: *end, body: None }
        }
        Op::DelBlk { start } => {
            check(*start)?;
            let end = block_end(lines, *start);
            Effect::Cut { a: *start, b: end, body: None }
        }
        Op::InsPre { line, body } => {
            check(*line)?;
            Effect::Insert { boundary: *line - 1, body: body.clone() }
        }
        Op::InsPost { line, body } => {
            check(*line)?;
            Effect::Insert { boundary: *line, body: body.clone() }
        }
        Op::InsBlkPost { line, body } => {
            check(*line)?;
            Effect::Insert { boundary: block_end(lines, *line), body: body.clone() }
        }
        Op::InsHead { body } => Effect::Insert { boundary: 0, body: body.clone() },
        Op::InsTail { body } => Effect::Insert { boundary: n, body: body.clone() },
    })
}

/// Apply a section's ops to `original`. Does NOT check the tag — callers use
/// [`apply_section`] for tag-checked application with stale recovery. Rejects
/// overlapping ops and out-of-range line numbers.
pub fn apply(original: &str, section: &FileSection) -> Result<String> {
    let (lines, trailing_nl) = split_lines(original);
    let n = lines.len();

    let effects: Vec<Effect> = section
        .ops
        .iter()
        .map(|op| resolve_effect(op, &lines))
        .collect::<Result<_>>()?;

    // Overlap detection.
    // - Cut intervals [a,b] must not intersect each other.
    // - A pure Insert boundary must not fall strictly inside a Cut interval.
    // - No two ops may target the same insertion boundary (ambiguous ordering);
    //   a Cut's body counts as an insertion at boundary a-1.
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut ins_boundaries: Vec<usize> = Vec::new();
    for e in &effects {
        match e {
            Effect::Cut { a, b, body } => {
                cuts.push((*a, *b));
                if body.is_some() {
                    ins_boundaries.push(*a - 1);
                }
            }
            Effect::Insert { boundary, .. } => ins_boundaries.push(*boundary),
        }
    }
    for i in 0..cuts.len() {
        for j in (i + 1)..cuts.len() {
            let (a1, b1) = cuts[i];
            let (a2, b2) = cuts[j];
            if a1 <= b2 && a2 <= b1 {
                bail!(
                    "hashline: overlapping ops on lines {a1}..={b1} and {a2}..={b2}"
                );
            }
        }
    }
    for e in &effects {
        if let Effect::Insert { boundary, .. } = e {
            for &(a, b) in &cuts {
                if a <= *boundary && *boundary <= b - 1 {
                    bail!(
                        "hashline: insertion at line boundary {boundary} falls inside deleted range {a}..={b}"
                    );
                }
            }
        }
    }
    {
        let mut sorted = ins_boundaries.clone();
        sorted.sort_unstable();
        for w in sorted.windows(2) {
            if w[0] == w[1] {
                bail!("hashline: two ops target the same insertion point (boundary {})", w[0]);
            }
        }
    }

    // Build result.
    let mut deleted = vec![false; n];
    let mut ins_at: HashMap<usize, Vec<String>> = HashMap::new();
    for e in effects {
        match e {
            Effect::Cut { a, b, body } => {
                for line in &mut deleted[a - 1..b] {
                    *line = true;
                }
                if let Some(body) = body {
                    ins_at.insert(a - 1, body);
                }
            }
            Effect::Insert { boundary, body } => {
                ins_at.insert(boundary, body);
            }
        }
    }

    let mut out: Vec<&str> = Vec::with_capacity(n + 8);
    if let Some(body) = ins_at.get(&0) {
        out.extend(body.iter().map(|s| s.as_str()));
    }
    for i in 1..=n {
        if !deleted[i - 1] {
            out.push(lines[i - 1]);
        }
        if let Some(body) = ins_at.get(&i) {
            out.extend(body.iter().map(|s| s.as_str()));
        }
    }

    let mut result = out.join("\n");
    if trailing_nl && !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Stale-tag application + recovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Applied {
    pub content: String,
    /// True when the live tag differed and the result was recovered via a snapshot.
    pub recovered: bool,
}

/// Apply a section to `live` content, checking the tag. When the live tag
/// differs and a `snapshot` matching the section tag is supplied, replay the ops
/// on the snapshot and port the resulting diff onto `live` with zero fuzz.
pub fn apply_section(
    section: &FileSection,
    live: &str,
    snapshot: Option<&str>,
) -> Result<Applied> {
    if compute_tag(live) == section.tag {
        return Ok(Applied { content: apply(live, section)?, recovered: false });
    }
    match snapshot {
        Some(snap) => {
            let snap_result = apply(snap, section)?;
            let content = port_diff(snap, &snap_result, live)?;
            Ok(Applied { content, recovered: true })
        }
        None => bail!(
            "hashline: `{}` now hashes to #{} but the patch targets #{}. The file changed \
             since you read it and no snapshot is available. Re-read it with `hashline read` \
             and rebuild the patch against the fresh tag.",
            section.path,
            compute_tag(live),
            section.tag
        ),
    }
}

/// Port the `snap -> snap_result` change onto `live` by matching each diff hunk's
/// original block (with context) in `live` and splicing the new block. Zero fuzz:
/// a hunk that can't be located exactly aborts the whole recovery.
fn port_diff(snap: &str, snap_result: &str, live: &str) -> Result<String> {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(snap, snap_result);
    let old_slices = diff.old_slices();
    let new_slices = diff.new_slices();

    let mut out = String::with_capacity(live.len());
    let mut cursor = 0usize; // byte offset into `live`
    for group in diff.grouped_ops(3) {
        let first = &group[0];
        let last = group.last().unwrap();
        let old_range = first.old_range().start..last.old_range().end;
        let new_range = first.new_range().start..last.new_range().end;
        let old_block: String = old_slices[old_range].concat();
        let new_block: String = new_slices[new_range].concat();

        let pos = live[cursor..].find(&old_block).ok_or_else(|| {
            anyhow!(
                "hashline: stale-tag recovery failed — the region being edited no longer \
                 matches the live file. Re-read the file and rebuild the patch."
            )
        })?;
        let abs = cursor + pos;
        out.push_str(&live[cursor..abs]);
        out.push_str(&new_block);
        cursor = abs + old_block.len();
    }
    out.push_str(&live[cursor..]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Snapshot store: per-path LRU for stale-tag recovery
// ---------------------------------------------------------------------------

const MAX_PATHS: usize = 30;
const MAX_VERSIONS_PER_PATH: usize = 4;
const MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct Snapshot {
    tag: String,
    text: String,
    seq: u64, // monotonic recency stamp
}

struct Inner {
    /// path -> versions, oldest first, newest last.
    map: HashMap<String, Vec<Snapshot>>,
    /// path keys, least-recent first, most-recent last.
    order: Vec<String>,
    total_bytes: usize,
    seq: u64,
}

/// Records whole-file snapshots so a later `apply` can recover from a stale tag.
/// LRU across paths (30 paths), bounded versions per path (4), and a global byte
/// budget (64 MiB). Thread-safe.
pub struct SnapshotStore {
    inner: std::sync::Mutex<Inner>,
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(Inner {
                map: HashMap::new(),
                order: Vec::new(),
                total_bytes: 0,
                seq: 0,
            }),
        }
    }

    fn touch_order(inner: &mut Inner, path: &str) {
        if let Some(pos) = inner.order.iter().position(|p| p == path) {
            inner.order.remove(pos);
        }
        inner.order.push(path.to_string());
    }

    /// Record a whole-file snapshot for `path`. Same-tag re-reads refresh recency
    /// instead of duplicating.
    pub fn record(&self, path: &str, text: &str) {
        let tag = compute_tag(text);
        let mut inner = self.inner.lock().unwrap();
        inner.seq += 1;
        let seq = inner.seq;

        let versions = inner.map.entry(path.to_string()).or_default();
        let mut added_bytes = 0usize;
        let mut freed_bytes = 0usize;
        if let Some(existing) = versions.iter_mut().find(|s| s.tag == tag) {
            existing.seq = seq;
        } else {
            added_bytes = text.len();
            versions.push(Snapshot { tag, text: text.to_string(), seq });
            if versions.len() > MAX_VERSIONS_PER_PATH {
                freed_bytes = versions.remove(0).text.len();
            }
        }
        inner.total_bytes = inner.total_bytes + added_bytes - freed_bytes;
        Self::touch_order(&mut inner, path);
        Self::evict(&mut inner);
    }

    /// Look up the snapshot text for `(path, tag)`, refreshing recency.
    pub fn lookup(&self, path: &str, tag: &str) -> Option<String> {
        let mut inner = self.inner.lock().unwrap();
        let text = inner
            .map
            .get(path)?
            .iter()
            .rev()
            .find(|s| s.tag == tag)
            .map(|s| s.text.clone());
        if text.is_some() {
            Self::touch_order(&mut inner, path);
        }
        text
    }

    fn evict(inner: &mut Inner) {
        // Evict least-recent whole paths until within the path cap and byte budget.
        while inner.order.len() > MAX_PATHS || inner.total_bytes > MAX_BYTES {
            if inner.order.len() <= 1 {
                break; // never evict the only (just-touched) path
            }
            let victim = inner.order.remove(0);
            if let Some(versions) = inner.map.remove(&victim) {
                let freed: usize = versions.iter().map(|s| s.text.len()).sum();
                inner.total_bytes = inner.total_bytes.saturating_sub(freed);
            }
        }
    }
}

#[cfg(test)]
mod tests;
