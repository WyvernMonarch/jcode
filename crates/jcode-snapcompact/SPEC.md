# jcode-snapcompact port spec

Port of omp's snapcompact (MIT). Rasterizer reference: `reference/snapcompact_native.rs`
(adapt: strip napi bindings → pure Rust API; fonts already in `src/fonts/`). Orchestration
source of truth: `/Users/yuriy/.bun/install/global/node_modules/@oh-my-pi/snapcompact/src/snapcompact.ts`
(readable locally — copy constants verbatim, do not re-derive). Prompt texts:
`.../snapcompact/src/prompts/snapcompact-summary.md` and `file-operations.md` (embed verbatim
via include_str! after copying into this crate).

## Extracted orchestration facts (validated 2026-07-21, file:line = snapcompact.ts)

- SHAPE_VARIANTS table at 105-190 (19 variants: 8x8r/8x8u/6x6u/5x8 bw+sent, 6x12-dim,
  8x13-bw, 8on16-bw, 8on22-bw, 11on16-bw, silver16-bw, doc-8on16-*), Shape iface 59-86.
- Family map 261-273: anthropic→11on16-bw, google→8on22-bw, openai→8on22-bw, legacy→5x8-sent.
- billingFamily 206-222; familyBilling 234-247: anthropic ceil(min(ceil(size/28)^2,4784)*1.05);
  google flat 1120; openai ceil(min(ceil(size/32)^2,10000)*1.2) + imageDetail "original".
- MODEL_VARIANTS regex overrides 338-356 (verbatim), incl. `/glm/i` → 8on16-bw (our test
  target glm-5v-turbo hits this). resolveShape 378-386; CJK fallback resolveShapeForText
  411-421 → silver16-bw. Dense companion tier FAMILY_VARIANT_LOW=8on16-bw (314-318).
- Caps: MAX_FRAMES_DEFAULT=80 (438), FRAME_TOKEN_ESTIMATE=5024 (449),
  FRAME_DATA_BYTES_ESTIMATE=170_000, FRAME_DATA_BYTES_BUDGET=3_000_000 (455,462).
- Serialization serializeConversation 770-899: scope prefixes `¶user:` `¶think:` `¶ai:`
  `¶call:` (merge consecutive same-scope); tool calls `name(args)//intent` + `<out>…</out>`
  results wrapped in dim toggles U+000E/U+000F; useless non-error tool results dropped with
  their call; TOOL_RESULT_MAX_CHARS=2000, TOOL_ARG_MAX_CHARS=500, TOOL_CALL_MAX_CHARS=2000,
  TRUNCATE_HEAD_RATIO=0.6, middle elided as `…{n}ch elided…`; only text blocks serialize
  (images/binary silently dropped).
- Wrap/pagination: geometry 1403-1411 (cols=floor(size/cellWidth),
  rows=floor(size/cellHeight/lineRepeat), capacity=cols*rows; halved per column for doc
  shapes). Grid shapes: paginateCells 1318-1339, cell-aware (CJK=2 cells, pad before
  edge-straddle), NO word wrap. Doc shapes: greedy wrap() 1347-1381 then docPages
  1389-1397 (pages of 2*rows lines). No in-image headers/footers.
- Assembly: planArchive 1750-1834 — TEXT_EDGE_PAGES=1 verbatim text page at each edge,
  imaged middle, foveated HQ/LQ/HQ when > maxFrames. historyBlocks 1667-1704 → ordered
  text/image blocks with delimiters "-------------- imaged middle below/above" and an
  omission notice if budget-trimmed. Final message: role user, [text(summary prompt),
  ...blocks], images {base64 png, image/png, optional detail}.
- Trigger (jcode adaptation): omp defaults strategy snapcompact and vision-gates at
  runtime (model.input lacks "image" → warn + fall back to LLM summary; manual invocation
  on text-only model errors instead). Local blockers also fall back: unrenderable glyphs,
  frame budget < 1, base64 payload > FRAME_DATA_BYTES_BUDGET, projected token overflow.
  Token accounting: flat 5024 tokens per frame.

## jcode integration decisions

- Crate depends on jcode-message-types; public API roughly:
  `compact_to_blocks(messages: &[Message], model_id: &str, max_frames: usize)
   -> Result<Vec<ContentBlock>>` plus `resolve_shape(model_id)` and low-level
  `render_frame(text, &Shape) -> Png` for tests.
- jcode gate: NEW config `[compaction] snapcompact = true` (default FALSE — we are guests
  upstream; opt-in) + model-id vision heuristic; fallback = existing summary path.
- Integration point: where app-core builds the summary text block
  (compaction-core `compacted_summary_text_block` / agent compaction flow) — snapcompact
  branch replaces the summary LLM call with deterministic frames; on any local blocker
  fall back to the summary path. Bill frames at IMAGE_TOKEN_COST override 5024.
