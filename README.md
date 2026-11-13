# sniff-llms

Passive llama.cpp HTTP/SSE inspector using `ngrep` and ratatui. The server does
not need to be restarted. Packet capture is the only privileged part, and the
program arranges it itself: `pkexec` where polkit exists, `sudo` otherwise, and
nothing at all when the account may already read the capture device (root, or
the macOS ChmodBPF drop-in). The dotfiles setup installs `ngrep` and provides a
`sniff_llms` shell alias, so the inspector can be launched from any directory:

```bash
sniff_llms                    # interfaces picked for you
sniff_llms --interface lo0    # only the local router
sniff_llms -i en0,utun3       # LAN and VPN, one ngrep each
```

The alias runs `cargo run --release --` against this project's manifest and
passes through all additional arguments.

## Interfaces and capture rights

BSD/macOS libpcap has no `any` pseudo-device: interfaces have to be named and
one `ngrep` runs per interface. macOS therefore defaults to `lo0` (where a local
llama.cpp router usually lives) plus the interface carrying the default route,
and refuses `--interface any` with the list of names the machine actually has.
On Linux `any` still covers every leg in one capture and remains the default.
`--dry-run` prints the interfaces, the `ngrep` that was found, and how it would
be elevated, without starting anything; `--no-elevate` forbids `sudo`/`pkexec`
and reports what the device said instead.

Rights are read from the machine rather than assumed. On Linux capture is
`CAP_NET_RAW`, not a uid, and can be granted invisibly to this program — `setcap`
on the `ngrep` binary, an ambient capability in a unit — so a non-root first
attempt is made unprivileged and only a fast death escalates. `ngrep` is
`brew install ngrep` on macOS, `apt install ngrep` on Debian, `pacman -S ngrep`
(Arch, `extra`); `NGREP=/path/to/ngrep` overrides the search. Helpers are
located by absolute path before `sudo` runs, so a `secure_path` that omits the
Homebrew bin directory does not hide `ngrep` — or the `ollama` used by `R`.

Capture listens on **all local TCP ports**, not a single one. Every packet is
then classified by its HTTP framing (status line vs request line) or, for bare
SSE data, by its JSON shape (`choices`/`completion` ⇒ response, `messages`/
`prompt` ⇒ request). A cheap pre-filter (`/completions` path or LLM JSON keys)
drops non-LLM traffic before any flow is created, and a chat is only opened
when a response object carries both an `id` and a body (`choices`/`completion`/
`prompt`). The LLM server endpoint for each flow is learned from the first
framed packet, so requests and responses on any port pair up correctly. When a
router mirrors the same response ID across multiple port pairs, the first
observed flow owns one conversation tab and chunks from the other legs are
ignored, preventing duplicated streamed output.

Requests are composed into threads, and a response that continues a previous
one is appended to the same tab instead of opening a new one. The evidence, in
order:

1. **Declared thread id.** A request header (`x-session-id`, `x-session-affinity`,
   `session_id`, `x-conversation-id`, `conversation_id`, `x-thread-id`,
   `thread_id`, `x-chat-id`, `chat_id`) or body field (`prompt_cache_key`,
   `conversation_id`, `session_id`, `thread_id`, also under `metadata`). Values
   shorter than 8 characters are ignored as too generic. When a client declares
   one, it is authoritative: same id merges, different ids never merge, even if
   the content looks related. The stats panel shows it as `Thread:`.
2. **Tool-call id.** The new request echoes the assistant `tool_calls` that the
   previous turn emitted. Works when the previous request was never captured or
   the client rewrites its history between calls (dynamic system prompts). Ids
   are unique, so identical-looking parallel sessions stay apart.
3. **History prefix** followed by tool output.
4. **Same keep-alive connection** as a tool-call finish, when the request was
   missed entirely.

Each such response becomes a new *turn* in the chat, and only the messages it
added are rendered. If a conversation was still split (for example traffic
captured before capture was running), select the later tab and press `m` to
merge it into the one on its left.

A **subagent generation** is deliberately *not* merged into its parent: it is a
separate process with its own context and history. It gets its own tab, placed
right after the parent's tabs and prefixed with `↳`, so parent and child stay
one visual group without corrupting either transcript. Detection uses the
parent's unfinished tool call named `subagent` / `spawn_agent` / `task` /
`delegate`: the child request must carry a subagent profile prompt (or the
request was missed, which leaves timing and the open call), and the child
tool-call brief is matched against the parent's arguments when both bodies were
captured. Grouping is bounded to the last 5 minutes of parent activity. To
regroup tabs by hand, press `g` on the parent, select the child, then press `g`
again: the child's subtree moves right after the parent's. `u` ungroups the
selected tab; closing a parent reparents its children instead of
dropping them. When a router backend leg wins a response id, request metadata
from the mirrored client leg (including the declared thread id) is adopted
without appending the stream twice.

Keys: `Left/Right` switch chats, `Up/Down` scroll, `End` resumes following,
`x` closes and forgets a chat, `X` closes every finished chat at once, `m`
merges the selected chat into the previous one as a continuation, `g` marks the
selected tab as a group parent (press it
again on the child to attach it), `u` ungroups it, `d` writes
`~/chat-id-{id}.log`, `a` cycles the auto-clean rule, `r` renames the current
chat (`Enter` saves, `Esc` cancels, blank clears), `R` asks a local model for a
short name, and `q` quits.

The left column is a timeline of the selected chat: one entry per generated
run of **thinking**, **answer**, or **tool** call, in generation order. Each
entry shows its estimated token count (from characters) and the wall-clock time
it took to generate. The active entry is marked and bold. Turn boundaries are
shown when a chat has more than one turn.

The sticky statistics panel shows the name (when set), model, completion
state, packet count, flow, declared thread id, parent id (for a grouped
subagent), token usage, tool-call count, turn count, and llama.cpp timing data
when those fields are present in the streamed JSON.
Conversation tabs use the custom name when set, otherwise middle-elided IDs.
A tab takes the color of the last generated thing: orange for a tool call, gray
for thinking, white for an answer, and green before any output. The selected tab
is underlined. When the tabs are wider than the terminal, the bar windows around
the selected one and shows a `+N` count on either side for the hidden tabs.

New chats never steal the selection; they are appended and shown in the tab
bar, and the status line records their captured ID.

Auto-clean (key `a`) closes a chat as soon as it finishes with a matching
reason. It cycles: off → tool_calls → stop → length → content_filter → off. The
current rule is always shown in the bottom bar.

`X` cleans the tab bar in one press: it closes every chat that has finished and
keeps the running ones. A chat counts as running while its last turn is still
streaming (no `finish_reason` yet), while it ended with `tool_calls` (the client
is executing them, or a subagent is), or while its last chunk arrived less than
30 seconds ago — the silent gap between two turns of a session you are still
using, when nothing is on the wire. Each drop does exactly what `x` does for one
chat: the response ids are forgotten so late chunks cannot resurrect the tab,
the capture buffers of a connection nothing else uses are released, and a
subagent tab whose parent was cleaned becomes a root tab instead of disappearing
with it.

The `R` key sends the first 100 characters of the user's request and the last
1000 characters of the assistant output to `ollama run` (model `gemma4:e2b`,
override with `SNIFF_NAMER`) and applies the returned short name to the chat.
Tool calls are parsed from `delta.tool_calls` and rendered as an orange
`=== TOOL CALLS ===` section in the chat log and in `d` dumps.

Only plain HTTP can be decoded. Start capture before the request if you want its
system prompt and full message list included.
