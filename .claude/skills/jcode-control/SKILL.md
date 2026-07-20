---
name: jcode-control
description: Programmatically build, run, and drive jcode (this repo) for development and testing — isolated test server, debug-socket CLI, headless runs, z.ai test models. Use when implementing or verifying jcode features without a human at the TUI.
---

# jcode-control: programmatic jcode operation

You are working in the jcode repo (`/Users/yuriy/Desktop/localdev/jcode`, branch `omp-merge`).
This skill tells you how to build the binary, run an ISOLATED test server, and drive it
mechanically. Follow the Safety rules exactly.

## Safety rules (non-negotiable)

1. NEVER touch the user's live server or its sockets (default socket in the OS runtime/temp
   dir, e.g. `/var/folders/.../T/jcode.sock`). Always use your own socket via `JCODE_SOCKET`.
2. NEVER run `git push`. Commit locally on branch `omp-merge` only.
3. LLM calls ONLY via the `zai-test` provider profile (z.ai). Allowed models:
   `glm-5-turbo` (default, fast), `glm-5.2`, `glm-4.7-reap-50`. No Anthropic/OpenAI/etc.
4. Do not run `jcode update`, `scripts/install_release.sh`, or anything that changes the
   user's installed launcher/builds under `~/.jcode/builds` or `~/.local/bin`.
5. Do not edit `~/.jcode/config.toml` beyond what already exists (the `zai-test` profile is
   already configured; its key lives in the app config dir env file).

## Build

```bash
cd /Users/yuriy/Desktop/localdev/jcode
CARGO_BUILD_JOBS=12 cargo check -p <crate-you-touched>     # fast iteration
CARGO_BUILD_JOBS=12 cargo test  -p <crate-you-touched>     # unit tests
CARGO_BUILD_JOBS=12 cargo build -p jcode --bin jcode       # debug binary for e2e testing
```

The e2e binary lands at `target/debug/jcode`. Machine has 16 cores / 128 GiB — parallel
builds are fine, but if several agents build at once, prefer `cargo check -p <crate>`.

## Headless one-shot run (no server needed)

```bash
target/debug/jcode --provider-profile zai-test --model glm-5-turbo run 'your prompt'
# JSON result instead of streamed text:
target/debug/jcode --provider-profile zai-test --model glm-5-turbo run --json 'your prompt'
```

This is the quickest end-to-end check that the binary + a tool works: the model has all
built-in tools available and will use them if the prompt asks.

## Isolated test server + debug socket (full control)

```bash
export JCODE_SOCKET=/tmp/jcode-omp-test.sock       # your private socket; pick a unique name
export JCODE_DEBUG_CONTROL=1                        # enables the debug command router
target/debug/jcode serve &                          # starts server; debug socket appears at
                                                    # /tmp/jcode-omp-test-debug.sock
```

Drive it with the debug CLI (same env vars set, or pass `-s $JCODE_SOCKET`):

```bash
jd() { JCODE_DEBUG_CONTROL=1 target/debug/jcode debug -s "$JCODE_SOCKET" "$@"; }

jd help                        # full command list — consult this first
jd sessions                    # list sessions
jd create_session              # new session (respects -C <cwd>)
jd message 'do X' -S <session-id> -w    # send prompt, wait for the turn to finish
jd history -S <session-id>     # full message history
jd last_response -S <session-id>
jd state                       # server state
jd tools -S <session-id>       # tool registry as the session sees it
jd memory                      # memory subsystem introspection; also: jd trigger_extraction
jd mcp:servers                 # MCP state; mcp:connect:<name> etc.
jd swarm:list                  # swarm state; swarm:info:<dir>, swarm:plan:<dir>,
                               # swarm:broadcast:<text>, swarm:notify:<session> <text>
```

Command grammar: `jcode debug <command> [arg]` is sent as `"command:arg"`; `-S` targets a
session, `-w` waits for completion (use with `message`). Namespaces `client:`/`tester:`
exist for TUI-client introspection.

Sessions started via the debug socket use the server's default provider. To force the test
provider for a session, create it via a headless client instead:

```bash
JCODE_SOCKET=/tmp/jcode-omp-test.sock target/debug/jcode \
  --provider-profile zai-test --model glm-5-turbo run 'seed prompt'
```

then find its session id with `jd sessions` and continue driving it with `jd message`.

When done: `JCODE_SOCKET=... target/debug/jcode server stop` (stops YOUR server only).

## Logs and diagnostics

- Server/agent logs: `~/.jcode/logs/jcode-YYYY-MM-DD.log` (tail during tests).
- `jd agent:memory` / `jd agent:info` — per-agent runtime profile.
- If a `run`/`message` hangs >180s, the provider stream idle timeout may be at play
  (`JCODE_STREAM_IDLE_TIMEOUT_SECS`).

## Conventions for this effort

- One crate/feature per agent; run `cargo check -p <crate>` + your unit tests before
  reporting done. "Done" = code compiles + unit tests pass + (where asked) an e2e probe via
  the isolated server transcript showing the feature working.
- Commit granularly on `omp-merge` with conventional messages (`feat(hashline): ...`).
- Follow existing test convention: co-located `#[cfg(test)]` with sibling `*_tests.rs` files.
