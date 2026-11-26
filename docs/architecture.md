# KimiChatCLI architecture

## Runtime ownership

`KimiChatCLI.exe` owns the temporary Kimi TOML and MCP configuration
transactions, the OpenAI-compatible loopback relay, a separate headless Brave
process, and the lifecycle of its own MCP server. When native Kimi swarm mode
is active, Kimi CLI owns the `AgentSwarm` child sessions and their progress UI.
For the MCP fallback, the executable runs its embedded Python worker-pool
source through the installed Python runtime and owns that helper's process
tree. It prefers relay port `8140` and CDP port `9227`, and picks free loopback
ports when either is taken.

The isolated browser profile is:

```text
%LOCALAPPDATA%\KimiChatCLI\state\brave-headless-profile-clean\
```

On first activation it is seeded from the last-used profile of the interactive
Brave user-data directory: cookies, local and session storage, IndexedDB,
preferences and `Local State`. The interactive Brave process and its windows
are never controlled; the clone is the only profile handed to the headless
process. `KIMICHATCLI_BRAVE_USER_DATA_DIR` overrides discovery.

The headless browser runs inside a kill-on-close job object, and swarm helpers
are tracked by PID; timed-out children are terminated as process trees.
Closing the controller therefore stops the relay, every child session and the
browser before the original `.kimi-code/config.toml` and `.kimi-code/mcp.json`
are restored byte-for-byte.

## Configuration transaction

Activation writes `config.toml.original`, `mcp.json.original` and an
`active-session.json` marker (both config paths, whether `mcp.json` existed,
the relay address) into the state directory before it edits anything. It then
injects:

- `providers.chatmode`: an `openai` provider whose `base_url` is the live
  relay address;
- `models."chatmode/<id>"` for each browser model, and `default_model =
  "chatmode/instant"`;
- `mcpServers.kimichatcli-swarm`: the relay's `/mcp` endpoint with a
  per-session `X-KimiChatCLI-Token` header.

Deactivation, and every launch before anything else, replays the marker:
backups are written back to the recorded paths, an `mcp.json` that did not
exist before is removed, and the marker is deleted last. A launch whose
restore fails refuses to start and keeps the backups.

## Request flow

```text
Kimi CLI
   |
   | POST /v1/chat/completions
   v
Rust relay :8140
   |
   | up to four requests at once, one page each
   v
Dedicated Brave CDP :9227
   |
   | fresh kimi.ai new-chat page
   | model selection + Input.insertText
   v
Kimi browser chat
   |
   | reasoning_content + final content
   v
Rust relay -> Kimi CLI renderer

Kimi CLI
   |
   | MCP initialize / tools/list / tools/call
   v
Rust relay `/mcp`
   |
   | native Kimi `AgentSwarm` when `/swarm` is active
   | Rust -> embedded Python -> bounded `kimi --agent ...` fallback otherwise
   v
Kimi indexed swarm progress or structured fallback report
```

The relay keeps a pre-warmed page pool keyed by model name, so a request
usually takes a page that already has the right model selected; when none is
pooled, a cold path prepares one on demand. MutationObserver-based composer
discovery replaces fixed sleeps during warm-up. Prompts are typed with Chrome
DevTools `Input.insertText`, which updates Kimi's Lexical editor correctly.
The rendered thinking panel is returned as `reasoning_content` and the answer
as `content`. Streaming requests poll the rendered nodes every 150 ms, emit
reasoning deltas immediately, then answer deltas, and finish with the standard
SSE `[DONE]` marker; an answer counts as finished only after two identical
completed polls, and delta rewrite detection prevents double-sending when Kimi
rewrites partial content. Every request starts on a fresh new-chat page, so
earlier page state cannot bleed into the next turn.

The CLI times out and retries long requests with an identical body. Retries
join the browser turn already running instead of starting a second one, and a
result that finished moments earlier is replayed. Short repeated fragments
("The\n\nThe\n\nThe") are treated as kimi.ai capacity failures rather than
answers.

## Browser prompt

Kimi `system` messages stay in the outer CLI request but are not typed into
the chat page; user, assistant and tool messages are. Each prompt starts with
a framing paragraph and a tool-calling contract (a leaner pair for the instant
model). `KIMICHATCLI_INSTRUCTIONS_FILE` replaces the framing, never the
contract, and is re-read on every request.

## MCP tool bridge

At activation the relay calls `initialize` and `tools/list` on every enabled
HTTP-transport server in the original `mcp.json`. Tools whose names match
filesystem or shell work are kept; UI automation, memory, registry, network,
browser and security tools are dropped so the catalog stays small enough for
the browser model. The catalog (full schemas up to 80 tools, a compact list
above that) and a worked example built from a real catalog tool are appended
to every prompt.

A `<kimi_tool_call>` marker in the answer is parsed with Windows-path-tolerant
JSON handling, executed relay-side, and its result typed back into the same
page; the loop allows ten tool rounds per request. A markerless refusal, or an
answer that used kimi.ai's own sandbox tools instead of the bridge, gets one
corrective turn. Calls naming tools the relay does not serve, such as the swarm
tools or stdio servers, pass through to Kimi CLI as ordinary OpenAI tool calls.
`KIMICHATCLI_TOOL_BRIDGE=0` skips discovery entirely.

## Swarm

When Kimi's system messages show native swarm mode, explicit spawn intent is
translated into the built-in `AgentSwarm` call with distinct indexed items, and
Kimi owns the child lifecycle and its progress cells. Without native mode,
explicit capability or spawn intent becomes a qualified MCP tool call and the
structured fallback result is returned. Tool-result turns are never routed back
into the same swarm tool automatically. Swarm child processes carry
environment guards that stop them from re-entering the browser relay.

## Recovery

The browser restarts every 50 requests to bound memory growth. If a sign-in
wall appears, the seed marker is removed, the profile is re-seeded from the
interactive Brave profile, the browser restarts, and the request is retried
once. WebSocket reads to CDP carry a 30-second timeout so a hung page cannot
pin a relay thread. Page warm-up checks the expected selectors and logs when
kimi.ai's DOM no longer matches them.

## Source boundaries

- `src/main.rs`: window, configuration transaction, HTTP relay, CDP browser
  driver, MCP tool bridge.
- `src/swarm.rs`: Streamable HTTP MCP server, embedded Python handoff and
  helper process-tree lifecycle.
- `python/swarm_backend.py`: bounded `ThreadPoolExecutor` fan-out over real
  local `kimi --agent ...` child sessions.
- `build.rs`: draws the icon and embeds it as a Windows resource.
- Runtime state: `%LOCALAPPDATA%\KimiChatCLI\state\` by default; an older
  `state\` directory next to the executable is still used while it holds an
  unfinished session.

## Verification

```powershell
cargo fmt -- --check
cargo clippy --release --all-targets -- -D warnings
cargo test --release
cargo build --release
kimi -m chatmode/instant -p "Reply with exactly VERIFY" --output-format text
```

The final CLI check should show a thinking block followed by `VERIFY`.
