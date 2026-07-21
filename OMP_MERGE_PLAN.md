# OMP → jcode merge plan

Goal: merge the best parts of omp (oh-my-pi, TypeScript) into jcode (Rust), keeping jcode
as the primary interface. Executed by parallel Opus agents in rounds; each round is
integration-tested end-to-end by the orchestrator via the isolated-server debug socket
(see `.claude/skills/jcode-control/SKILL.md`) using z.ai `glm-5-turbo`.

## Why these features (from the comparative analysis)

jcode's strengths are runtime primitives (server, sessions, swarm, ambient, memory
injection); omp's strengths are per-turn tool quality. We port the omp side that jcode
lacks:

| # | Feature | From omp | Value |
|---|---------|----------|-------|
| A | `hashline` edit tool | `@oh-my-pi/hashline` | Line-anchored patch language: fewer failed edits than string-replace, cheap validation via whole-file tag |
| B | Agent-authored skills (`create`/`update`/`delete` in `skill_manage`) | `manage_skill`/`learn` | Agent persists its own capabilities; fills jcode's gap |
| C | BM25 skill auto-suggestion | `search_tool_bm25` pattern | Fulfils jcode README's promised (but unimplemented) skill auto-activation, without embeddings |
| D | Secret redaction in memory extraction | (gap in BOTH; jcode has unused `redact_secrets`) | Stops credentials leaking into persistent memory |
| E | Worktree-isolated task delta capture | `task/worktree.ts` synthetic-tree diff | Parallel agents get conflict-freedom by isolation; jcode has NOTHING here today (docs overpromise) |

Deferred to later phases (not this effort): SQLite memory backend (mnemopi port),
snapcompact bitmap compaction, omp-stats dashboard, AST tools (`ast_grep`/`ast_edit`), LSP
tool, DAP debugger, collab web sharing.

## Ground truth (validated 2026-07-21, two independent code audits)

- Tools live one-per-file in `crates/jcode-app-core/src/tool/`, registered in
  `tool/mod.rs::base_tools()` via `insert_tool_timed`. No edit-variant selector exists;
  gating is by name via `[tools]` config (`jcode-base/src/config.rs:536-636`).
- `read` output format: `format!("{:>5}\t{}", line_num, line)`, `MAX_LINE_LEN=2000`
  (truncated display), `DEFAULT_LIMIT=5000` lines. Hash anchors must hash ORIGINAL line
  content, not truncated display.
- `skill_manage` actions today: load/list/reload/reload_all/read
  (`tool/skill.rs:68`). Skill sources incl. `~/.jcode/skills`, project `./.claude/skills`
  (`jcode-base/src/skill.rs:99-343`). No write-path sandbox exists anywhere; `write` to
  `~/.jcode/skills/...` already works.
- BM25 (`bm25_rank`, `jcode-base/src/memory.rs:1991`) and RRF fusion exist; `Skill.search_text`
  is built (`skill.rs:925-928`) but NEVER read — dead field ready to be wired.
- Memory finalization points (no redaction today): `jcode-base/src/memory_agent.rs:203`
  (session-end) and `:1103` (incremental) — both do `MemoryEntry::new(...)` then
  `remember_project`. REUSE existing `crate::message::redact_secrets`
  (`jcode-base/src/message.rs:53-215`, already used for session export).
- NO git-worktree code exists in jcode ("worktree_scope" strings are a selfdev build-lock
  key, unrelated). Feature E is from scratch.
- Workspace: root `Cargo.toml` has an explicit flat `members` list; internal deps are
  `{ path = "../jcode-x" }`. Tests are co-located `#[cfg(test)]` + sibling `*_tests.rs`.
- Test provider: profile `zai-test` (already configured) → `glm-5-turbo` on
  `https://api.z.ai/api/coding/paas/v4`. Isolated server: `JCODE_SOCKET=... JCODE_DEBUG_CONTROL=1`.

## hashline spec (port target, exact)

- File tag: `TAG = uppercase hex4( xxHash32(normalized_text, seed=0) & 0xFFFF )` where
  normalization strips trailing `[ \t\r]` from every line before hashing.
- Patch grammar (per file section):
  ```
  [relative/path.rs#TAG]
  SWAP A.=B:      (replace lines A..=B with the + body)
  SWAP.BLK A:     (replace the indentation block starting at line A)
  DEL A.=B / DEL A / DEL.BLK A
  INS.PRE A: / INS.POST A: / INS.HEAD: / INS.TAIL: / INS.BLK.POST A:
  +body line      (each body row prefixed with '+'; no '-' or context rows)
  ```
  Line numbers are 1-indexed against the ORIGINAL file; multiple ops per section apply
  against original coordinates (compute final result via ordered, non-overlapping op list —
  reject overlaps).
- Stale tag: if TAG doesn't match live content, attempt recovery only when a snapshot with
  that TAG exists: replay edits on snapshot, produce a structured diff (fuzz 0), apply to
  live content; else fail with a clear "re-read the file" error.
- SnapshotStore: per-path LRU — 30 paths max, 4 versions/path, 64 MiB budget; record
  `{path, text, hash, recorded_at}` whenever hashline `read` runs.
- Tool surface (self-contained; do NOT modify the existing `read` tool in round 1):
  `hashline` tool with `action: "read" | "apply"`.
  - `read {path, offset?, limit?}` → `[path#TAG]` header + `N:content` rows (raw content,
    no display truncation for hashing purposes).
  - `apply {patch}` → applies one or more file sections; returns per-file result summary.

## Worktree delta spec (port target, exact)

Baseline capture (at spawn): HEAD sha + three patches: staged (`git diff --cached`),
unstaged (`git diff`), untracked (each file as no-index diff).
Delta capture (at finish) — synthetic-tree diff, drift-proof:
1. temp `GIT_INDEX_FILE`; `git read-tree HEAD_at_baseline`; `git apply --cached` the 3
   baseline patches; `git write-tree` → tree_base.
2. same procedure with the isolation dir's current state → tree_now.
3. `git diff-tree -p tree_base tree_now` → the task's exact delta.
Apply-back: `git apply --binary` the delta in the parent checkout (report conflicts,
do not force). Isolation backend: plain `git worktree add` (skip omp's COW backends).
Cleanup: remove worktree + temp branches.

## Rounds

### Round 1 — parallel implementation (Opus agents, disjoint footprints)

Skeleton crates `jcode-hashline` and `jcode-worktree` plus workspace-member entries and
tool registration stubs are pre-created by the orchestrator so agents never touch shared
files concurrently.

| Task | Agent footprint | Acceptance |
|------|----------------|------------|
| A: hashline | `crates/jcode-hashline/*`, `crates/jcode-app-core/src/tool/hashline.rs` (+ its `hashline_tests.rs`) | Parser/applier/snapshot unit tests incl. tag mismatch + recovery; `cargo check -p jcode-hashline -p jcode-app-core`; tool visible in registry |
| B+C: skills | `tool/skill.rs` (+tests), NEW `jcode-base/src/skill_match.rs`, minimal hooks in `jcode-base/src/skill.rs`, injection in `jcode-app-core/src/agent/prompting.rs` | `skill_manage create/update/delete` writes `~/.jcode/skills/<name>/SKILL.md` with `managed: true` frontmatter, refuses clobbering unmanaged skills; BM25 suggestion: top-k skills over last user message injected as a one-line dynamic-prompt hint when score clears threshold; unit tests both |
| D: redaction | `jcode-base/src/memory_agent.rs` (+tests) | Both finalization points pass content through `redact_secrets`; test: fake `sk-...` key in extracted content is stored redacted |
| E: worktree | `crates/jcode-worktree/*` only | Unit+integration tests on a scratch git repo: baseline capture, synthetic-tree delta with pre-existing dirty state, apply-back, conflict reporting, cleanup; `cargo check -p jcode-worktree` |

Rules for agents: no `git commit`/`push` (orchestrator commits), no touching files outside
the declared footprint, follow `.claude/skills/jcode-control/SKILL.md`, `cargo check` +
unit tests green before reporting done. Report: what changed, test evidence, known gaps.

### Round 2 — integration + fixes

Orchestrator: `cargo check --workspace`, `cargo test -p` touched crates, build
`target/debug/jcode`, commit round-1 state, then e2e via isolated server + `glm-5-turbo`:

1. hashline: session edits a fixture file via `hashline read`→`apply`; verify bytes on disk;
   verify stale-tag rejection after out-of-band edit.
2. skills: session invents a skill via `skill_manage create`, `reload`, then a fresh
   session message matching the skill's description shows the BM25 hint.
3. redaction: seed a fake token in conversation, `jd trigger_extraction`, dump memory JSON,
   assert `[REDACTED`.
4. worktree: run the crate's CLI-less API through a test binary or unit tests (server
   wiring is round 3+).

Defects → correction Opus agents (one per defect cluster) → re-test. Repeat until green.

### Round 3+ (as time allows)

- Wire `jcode-worktree` into subagent/swarm spawn as `isolated: true`.
- Stamp `[path#TAG]` headers into the main `read` tool behind a config flag.
- Auto-`learn` nudge after tool-heavy turns (omp's autolearn controller pattern).

## Definition of done (whole effort)

- `cargo check --workspace` green; all new unit tests green.
- All four e2e probes above pass against `target/debug/jcode` with `glm-5-turbo`.
- Work committed incrementally on `omp-merge`; nothing pushed; user's live server untouched.

## Results (2026-07-21)

All rounds complete. 48 new unit/integration tests, `cargo check --workspace` green
(only pre-existing upstream failures, verified failing on clean master: intent-schema x2,
bash stdin x2, batch schema, swarm legacy snapshot).

Live e2e — isolated `JCODE_HOME`+`JCODE_RUNTIME_DIR` server, all LLM calls `glm-5-turbo`
via `zai-test`:

| Probe | Result |
|---|---|
| hashline read→apply edits file, tag advances | PASS |
| stale tag rejected with both-tags error; model re-reads and retries OK | PASS |
| `skill_manage create` writes managed SKILL.md, immediately loadable | PASS |
| BM25 `# Skill hints` in dynamic prompt (deterministic: score 23.2, model quotes it) | PASS |
| `tools.read_hashline_tags` → `[path#TAG]` header from main read | PASS |
| memory extraction with seeded `sk-live-FAKE...` → zero secret bytes in store | PASS |
| coordinator spawns `isolated:true` worker; `collect` applies 163-byte delta to parent | PASS |

Isolation recipe for e2e (also in the skill): `JCODE_HOME=<scratch> JCODE_RUNTIME_DIR=/tmp/<short>`
(socket path must clear SUN_LEN), `JCODE_DEBUG_CONTROL=1`, drive via `jcode debug -s
<rt>/jcode.sock` (`create_session:<path>`, `message -S <sid> -w`, `trigger_extraction`).
Known cosmetic upstream bug: openai-compatible profiles print "Using OpenRouter" at startup.

Deferred (round 3+ leftovers): auto-apply-on-completion for isolated members (explicit
`collect` implemented instead), autolearn nudge, SQLite memory backend,
AST/LSP/DAP tools, omp-stats dashboard.

## snapcompact results (2026-07-21, round 4)

Landed: `jcode-snapcompact` crate (MIT rasterizer adapted from omp pi-natives: BDF/hex/TTF
fonts, 17 eval-tuned shapes, ¶-scope serialization, cell pagination, foveated archive —
29 tests) + opt-in integration (`[compaction] snapcompact = true`, vision gate via
`Provider::supports_image_input()` or model-id regex, deterministic artifact replaces the
summary-LLM call, flat 5024 tok/frame, fallback to summary on any blocker — 6 tests).

Live e2e (isolated server, glm-5v-turbo): 320KB seeded across 8 user messages → manual
compact → artifact applied in <20s with no LLM run (previous 39KB attempt correctly fell
back to summary: both text edges [~19.2k chars each at 8on16-bw] swallowed the whole
fixture, imaged middle empty). Recall of facts living ONLY in the imaged middle:
exact hit ("November 19th"), near-miss on a 4-digit code (7351 vs 7391 — pixel-level
misread), one hallucinated rare word ('launchdarkly' vs 'lampyrid'). Frames confirmed in
provider requests (321KB request json = base64 PNGs).

Findings worth acting on later: (1) glm-5v-turbo fidelity at 8on16-bw is mixed for codes/
rare words — consider a `[compaction] snapcompact_shape` override (crate already supports
explicit variants, e.g. silver16-bw/11on16-bw) and an omp-style shape eval for GLM;
(2) gotchas for testers: config is read at server start (restart after toggling the flag),
manual compact needs >10 messages, tool results serialize truncated to 2000 chars — feed
long fixtures as user messages; (3) session takeover + client disconnect drops
debug-created sessions — keep one wire connection for seed/compact/recall.
