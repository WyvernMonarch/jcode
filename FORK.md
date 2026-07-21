# Fork features & workflow

This is `WyvernMonarch/jcode` — a managed fork of [`1jehuang/jcode`](https://github.com/1jehuang/jcode)
carrying features ported from [oh-my-pi](https://github.com/can1357/oh-my-pi) plus our own
additions. One branch per feature, `omp-merge` integrates them all on top of upstream master.

Plan and e2e test log: `OMP_MERGE_PLAN.md`. Programmatic driving of jcode for
testing: `.claude/skills/jcode-control/SKILL.md`.

---

## Features

### 1. hashline — line-tag patch language (`feat/hashline`)

Crate `jcode-hashline` + a `hashline` tool. Every line gets an xxHash32 tag;
patches address lines by `[path#TAG]` sections instead of fragile string
matching, with whole-file snapshots backing stale-tag recovery.

**Use (model-facing tool):**
- `hashline {action: "read", path, offset?, limit?}` — hashed, line-numbered
  view (`[path#TAG]` header + `N:content` rows, cap 5000), records a snapshot.
- `hashline {action: "apply", patch}` — applies one or more `[path#TAG]`
  sections; a stale tag recovers via the stored snapshot.

**Config:**
```toml
[tools]
read_hashline_tags = true   # plain `read` also emits [path#TAG] headers
                            # + snapshots (off by default, zero change when off)
```

### 2. Skills overhaul (`feat/skills`)

Four related pieces:

- **Managed skills** — `skill_manage` gains `create` / `update` / `delete`
  (plus upstream `load`/`list`/`reload`/`reload_all`/`read`). Agent-authored
  skills carry `managed: true` frontmatter; hand-written skills are never
  clobbered.
- **BM25 skill hints** — instead of relying only on the always-on skills list,
  each turn the latest user message is BM25-matched against skill
  name+description+body; top-3 hits above a score floor are injected as a
  `# Skill hints` block in the *dynamic* (uncached) prompt part. Junk queries
  inject nothing.
- **`disable-model-invocation` respected** — skills whose SKILL.md frontmatter
  sets `disable-model-invocation: true` stay out of the model-facing surfaces
  (Available Skills list, BM25 hints). `/name` slash invocation still works.
  Upstream ignored this field and injected every description (measured: 38
  skills ≈ 3.4k tokens ≈ 87% of the system prompt on a plugin-heavy machine).
- **`[skills]` config** — load-time control, glob patterns, no hardcoding:

```toml
[skills]
plugin_import = false            # don't scan ~/.claude/plugins for skills
exclude = ["figma-*"]            # don't load at all (no prompt, no slash)
user_invoked_only = ["review"]   # keep /review working, hide from the model
```

Filters apply to global sources (plugins, `~/.jcode/skills`,
`~/.agents/skills`); project-local `./.jcode|.agents|.claude/skills` load
unfiltered.

**`/skills-setup`** — interactive TUI checkbox picker over every global skill:
`↵` toggles on/off, `Esc` saves (unchecked → `[skills].exclude`; hand-written
glob patterns are preserved). Only works in a session with **0 messages** —
after that the skill list is baked into the cached prompt prefix, so the
command refuses and suggests `/clear`. Saving reloads the registry live: in
process for local sessions, via the new `reload_skills` wire request for
remote ones.

### 3. Memory redaction (`feat/memory-redaction`)

Memory-agent extraction passes conversation content through
`message::redact_secrets` before anything is persisted, so API keys/tokens
never land in long-term memory. Automatic, no knobs.

### 4. Worktree-isolated swarm agents (`feat/worktree-isolation`)

Crate `jcode-worktree` + swarm integration. `swarm {action: "spawn",
isolated: true, ...}` runs the member in a disposable git worktree seeded from
the parent checkout; its edits are held aside until `action: "collect"`
(with `target_session`) applies the delta back to the coordinator tree.
Conflicting deltas are never forced — the conflict is reported and the patch
saved to a file for manual apply. Requires the working dir to be inside a git
repo (falls back to a plain spawn with a note otherwise).

### 5. snapcompact — bitmap-frame compaction (`feat/snapcompact`)

Crate `jcode-snapcompact`. Opt-in replacement for LLM-summary compaction:
dropped history is deterministically rendered into pixel-font PNG frames (no
model call to compact). Model-aware shape selection (8on16-bw for GLM/Kimi,
8on22 for GPT/Gemini, 11on16 for Claude), foveated pagination with verbatim
HQ edges (3 frames each side), head+tail tool-result truncation, max 80
frames.

```toml
[compaction]
snapcompact = true
```

Requires a vision-capable **current** model (checked via
`Provider::supports_image_input`, heuristic fallback on the model id);
non-vision models silently fall back to the normal summary path, so enabling
globally is safe.

### 6. Tool control — `/tools-setup` + per-family routing (`feat/tool-control`)

Stacked on `feat/skills` (reuses the checkbox-picker infrastructure).

**`/tools-setup`** — interactive checkbox picker over every registered tool
(name + first description line): `↵` toggles, `Esc` saves unchecked tools into
`[tools].disabled` and restarts the (empty) session so the rebuilt Agent
re-reads the config. Gated to sessions with **0 messages** — the tool list is
locked per session for prompt-cache stability.

**`[tools.families]`** — per-model-family tool routing. Keys are
case-insensitive substrings matched against the active model id; matching
entries' `disabled` lists are subtracted from the tool set when the list locks
(first turn). No families ship by default — pure config:

```toml
[tools.families.gpt]
disabled = ["edit", "multiedit", "patch"]   # codex models patch via apply_patch
[tools.families.claude]
disabled = ["apply_patch", "patch"]         # claude models edit via str_replace
[tools.families.glm]
disabled = ["apply_patch", "patch"]
```

Rationale: upstream sends all 5 editing tools (~16.5k tokens of schemas for 34
tools) to every model; opencode ships the same idea hardcoded
(`registry.ts`: GPT-5 → apply_patch, everyone else → edit+write). A
mid-session model switch keeps the locked list — cache stability wins.

### 7. `fix/config-env-keys`

One-liner: `JCODE_TOOL_CALL_DETAILS` added to `CONFIG_ENV_KEYS` — the env
fingerprint test was red on upstream master. Prime upstream-PR candidate.

---

## Branch layout

```
master                     mirror of upstream/master  (never commit here)
fix/config-env-keys        ┐
feat/hashline              │  one branch per feature,
feat/skills                │  each compiles standalone
feat/memory-redaction      │  off master
feat/worktree-isolation    │
feat/snapcompact           ┘
feat/tool-control          stacked on feat/skills (shares the picker infra)
omp-merge                  integration = master + merge of all of the above + docs
```

Old pre-split linear history: tag `omp-merge-legacy-linear`.

Remotes: `origin` = WyvernMonarch/jcode (fork), `upstream` = 1jehuang/jcode
(push disabled on purpose).

## Workflows

**Change a feature**
```sh
git checkout feat/<x>        # commit the change here
git checkout omp-merge && git merge feat/<x>
cargo build --release --bin jcode
```

**Sync with upstream**
```sh
git fetch upstream
git checkout master && git merge upstream/master
git checkout omp-merge && git merge master     # fix conflicts once, here
```
Rebase individual `feat/*` branches onto fresh master only when you actually
touch them (or when preparing an upstream PR).

**Build & install** — `~/.local/bin/jcode` is a symlink to this repo's
`target/release/jcode`; `cargo build --release --bin jcode` makes the new
binary live (running server daemons pick it up via auto_server_reload).
Rollback to the stock release channel:
```sh
ln -sf ~/.jcode/builds/stable/jcode ~/.local/bin/jcode
```

**Upstream a feature** — each `feat/*` branch is a self-contained PR:
`gh pr create --repo 1jehuang/jcode --head WyvernMonarch:feat/<x>`.

**Drop a feature** — rebuild `omp-merge` from master merging every branch
except the dropped one (the layout exists precisely so this is cheap).
