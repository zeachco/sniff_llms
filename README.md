# sniff-llms

Passive llama.cpp HTTP/SSE inspector using `ngrep` and ratatui. The server does
not need to be restarted. Packet capture alone is elevated through `pkexec`.
The dotfiles setup installs `ngrep` and provides a `sniff_llms` shell alias, so
the inspector can be launched from any directory:

```bash
sniff_llms --interface any
```

The alias runs `cargo run --release --` against this project's manifest and
passes through all additional arguments.

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

A response that continues a previous one after a tool call is composed into
the same tab instead of opening a new one. The primary signal is the tool-call
id: the new request echoes the assistant `tool_calls` that the previous turn
emitted, which works even when the previous request was never captured or the
client rewrites its history between calls (dynamic system prompts). Ids are
unique, so identical-looking parallel sessions stay apart. When no id was
captured, the fallback is a message-for-message prefix match followed by tool
output, then — when the request itself was missed — a response arriving on the
same keep-alive connection as a tool-call finish. Each such response becomes a
new *turn* in the chat, and only the messages it added are rendered. If a
conversation was still split (for example traffic captured before capture was
running), select the later tab and press `m` to merge it into the one on its
left.

Keys: `Left/Right` switch chats, `Up/Down` scroll, `End` resumes following,
`x` closes and forgets a chat, `m` merges the selected chat into the previous
one as a continuation, `d` writes `~/chat-id-{id}.log`, `a` cycles the
auto-clean rule, `r` renames the current chat (`Enter` saves, `Esc` cancels,
blank clears), `R` asks a local model for a short name, and `q` quits.

The left column is a timeline of the selected chat: one entry per generated
run of **thinking**, **answer**, or **tool** call, in generation order. Each
entry shows its estimated token count (from characters) and the wall-clock time
it took to generate. The active entry is marked and bold. Turn boundaries are
shown when a chat has more than one turn.

The sticky statistics panel shows the name (when set), model, completion
state, packet count, flow, token usage, tool-call count, turn count, and
llama.cpp timing data when those fields are present in the streamed JSON.
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

The `R` key sends the first 100 characters of the user's request and the last
1000 characters of the assistant output to `ollama run` (model `gemma4:e2b`,
override with `SNIFF_NAMER`) and applies the returned short name to the chat.
Tool calls are parsed from `delta.tool_calls` and rendered as an orange
`=== TOOL CALLS ===` section in the chat log and in `d` dumps.

Only plain HTTP can be decoded. Start capture before the request if you want its
system prompt and full message list included.
