# Using Bloom in the terminal

## What it is

Bloom is a keyboard-first iMessage client that runs in your terminal. It talks to the local `bloomd` daemon, which talks to the OpenBubbles engine installed on your machine. There is no hosted service in this path.

## Run it

Install from the repository root after setting up Dart and OpenBubbles:

```sh
(cd ext/bloom-engine-sidecar/dart && dart pub get)
export BLOOM_ENGINE_SCRIPT="$PWD/ext/bloom-engine-sidecar/dart/bin/bloom_engine.dart"
cargo install --path apps/bloomd
cargo install --path apps/bloom-tui
bloom
```

The `bloom` command starts `bloomd` if nothing is listening on the socket, opens
the TUI, and stops the daemon it started when you quit. `cargo install` writes to
`~/.cargo/bin`, which rustup normally keeps on `PATH`; if `bloom` is not found,
add `export PATH="$HOME/.cargo/bin:$PATH"` to your shell startup file. A running
daemon is reused, so a second `bloom` shares it while the first owns the engine.
Quitting the owning launcher also disconnects those other clients.

Cargo installs only the Rust binaries, not the Dart sidecar, its package dependencies,
or the external engine. Keep the checkout and persist the absolute
`BLOOM_ENGINE_SCRIPT` setting in your shell. The installed launcher reads exported
environment variables, not the checkout's `.env`. See [setup prerequisites](../README.md#running-it).

From the repository instead, run:

```sh
./scripts/bloom
```

The repository launcher invokes Cargo's incremental build, starts `bloomd` unless a daemon is already answering on the socket, then opens the UI. When you quit, it stops the daemon it started. Diagnostics go to `~/.local/share/bloom/bloomd.log` (or the corresponding state-home directory if configured). Paths are discovered automatically; override them in a trusted `.env` file at the repository root. The file is sourced as shell code, so keep it to path settings and never put secrets there.

## TUI diagnostics

The TUI writes a private diagnostic log to `~/.local/state/bloom/tui.log` (Linux and other Unix systems; `$XDG_STATE_HOME/bloom/tui.log` when configured), `~/Library/Logs/bloom/tui.log` on macOS, or `%LOCALAPPDATA%\\bloom\\tui.log` on Windows. On Unix the directory is mode 0700 and log files mode 0600. The active log is bounded to 256 KiB with one rotated `tui.log.1`; unsafe symlink paths and logging errors are ignored so logging cannot interrupt the UI.

Entries include connection disconnection, request begin/result, an ephemeral attempt number, operation stage, elapsed time, and allowlisted IPC error category. They never include message bodies, recipient/conversation identifiers, credentials, raw IPC frames, or arbitrary error text. Timeout and transport failures are reported as uncertain rather than claiming the message was delivered. If a send looks wrong, inspect this file and share only the relevant sanitized lines.

## One rule that matters

Only one process may own the engine state: it holds your real registration. Close the OpenBubbles app before starting Bloom. To check for a leftover owner, run:

```sh
pgrep -af 'bloomd|bloom_engine'
```

## Keys

The composer starts focused, the way Codex opens: you can type immediately. Tab
switches between the composer and chat list; arrow keys follow whichever region is
focused.

| Key | Action |
| --- | --- |
| Up / Down | Move through wrapped draft lines; at an edge, scroll the transcript. In the chat list, select a chat. |
| PageUp / PageDown | Scroll a screenful |
| Ctrl+Home / End | Jump to the oldest / newest message |
| Tab | Toggle focus between the composer and chat list |
| Enter | Send the message |
| Ctrl+J / Alt+Enter / Shift+Enter | Insert a newline in the message body (Ctrl+J works on every terminal) |
| Terminal paste shortcut | Insert literal text; bracketed-paste slashes never run commands |
| Esc | Dismiss the open popup without losing its draft; otherwise cancel composition |
| `/` | Command menu |
| `?` | Keyboard-shortcut sheet |
| `,` | Settings (also `Ctrl+B` then `s`) |
| `/new` | Start a conversation: recipient first, then the message |
| `/sync` | Refresh conversation-list metadata; does not sync message history |
| Ctrl+W / Alt+Backspace | Delete the previous word |
| Ctrl+U / Ctrl+K | Clear before the cursor / kill to the end |
| Ctrl+R | Recover the oldest saved failed draft into an empty, unoccupied composer; existing-chat drafts require their original chat |
| Ctrl+A / Ctrl+E | Move to the start / end of the draft |
| Ctrl+Left / Ctrl+Right | Move by word |
| Ctrl+B, then `h` / `l` | Previous / next chat |
| Ctrl+B, then `n` / `p` | Next / previous chat |
| Ctrl+P | Previous chat |
| Ctrl+B, then `j` / `k` | Scroll the transcript |
| Ctrl+B, then `f` / `s` / `t` | Filter / settings / theme |
| Ctrl+C | Clear a nonempty input first; quit when it is already empty |
| Settings | Left / Right switch tabs; Up / Down select; Space toggles; Enter changes |

Ctrl+B is a leader: press it, then the indicated key. `n` is text while the composer
is focused; use `/new` to start a conversation. Ctrl+C clears the composer or filter
query before quitting on a subsequent press with an empty line.

## Mouse

Mouse reporting is on. The wheel scrolls whatever is under the pointer: the
transcript in the pane, the selection in the chat list. Left click selects a chat,
focuses the composer, runs a command from the popup, toggles a settings row, or
opens the header's `⚙` settings and `?` shortcuts controls. The hitboxes come from
the last frame that was drawn, so a click can only land on something visible.

## Screens

The window has no outer frame; content runs to the edges like Codex.

- **Sidebar:** conversations are listed newest first. Each chat shows its name and, underneath, a preview of the newest message Bloom has stored (your own prefixed `You:`), clipped with `…`. The selection highlights both lines. Below 70 columns, or with `/compact`, entries collapse to one line. A direct chat with no title is named after the other participant (their display name, or their handle when the engine has none), and an untitled group lists its people. While filtering, the section label becomes the match count, or “NO MATCHES”.
- **Header:** leads with the selected conversation, with the recipient handle for a direct chat or the participant count for a group (dropped whole when it does not fit). Identity and status get space before optional controls. The right side can show the theme control, `⚙`, `?`, and `Bloom`; branding, theme, then settings disappear as space runs out. On narrow screens with enough height, status moves to a second row instead of squeezing the title. Long identities use `…`; `/settings`, `/theme`, and `?` remain keyboard-accessible even when their header controls are hidden. A status dot and label show the engine session or connection state: green for healthy, yellow for attention, and red for failure. A braille spinner beside Connecting or Disconnected indicates connection/retry activity. Inbound message/update/delivery/list activity sets an inferred `Syncing` indicator with a spinner; it clears after two quiet seconds. This is not authoritative sync completion or evidence that remote history was imported. Failed, blocked, or disconnected engine states take precedence. Running `/sync` shows the same label until the metadata refresh finishes.
- **Transcript:** consecutive messages from one sender within five minutes share a single byline (a bold, per-speaker colored name and a timestamp), a centered divider marks each new day, and a blank row separates senders. Your messages also show delivery state, such as queued, sent, delivered, read, or failed. An outgoing message still queued for delivery has a spinner beside “queued”. Incoming events update immediately; the selected thread also reconciles against Bloom's stored history every five seconds to recover from missed live events.
- **Composer:** a bare `›` prompt, focused initially. Drafts wrap at grapheme boundaries by default; Up / Down move the cursor between wrapped lines, preserving its visual column, and show `↑ N more` / `↓ N more` when a long draft exceeds the available rows. Toggle wrapping under Settings > Layout; the choice is stored locally, and disabled wrapping keeps the old horizontal scroll behavior. Multiline entry uses Ctrl+J (Alt+Enter and Shift+Enter also work where the terminal reports them) and bracketed paste, so a pasted multi-line body keeps its blank lines. Pasting literal text never runs a slash command: a paste that begins with `/new` stays a message body and is only sent by an explicit Enter. For a new conversation, the prompt first asks for a recipient and then a message. Sending shows `Sending…`; `Message sent.` appears only after IPC accepts the message. A failure is shown in red. Bloom does not claim Delivered or Read without transport events. Drafts are held per recipient for the session. Settings, theme, contacts, and rename use suspended composition; closing a panel restores only the matching recipient's draft, including cursor and literal-paste provenance. New-chat bodies are bound to their entered recipient rather than a shared scratch body. Explicit cancellation may discard the current new-chat draft; closing the TUI discards all session drafts.
- **New-chat completion:** the accepted message ID is correlated with its actual message and conversation metadata before opening the recipient's transcript. Live events, the acknowledgment, and list refreshes may arrive in any order. Newer text, a changed selection, or another active prompt prevents automatic navigation. Without a matching live message, Bloom reports a successful new-chat send but does not guess which thread to open. While the new-chat composer is open the transcript shows the new-message state, not the previously selected chat's history.
- **Send recovery:** rejected sends restore the body and new-chat recipient only when the composing context is unchanged and empty. Otherwise drafts remain queued in memory until explicitly recovered with Ctrl+R; newer text and another recipient prompt are never overwritten. Recovery is disabled while a request is pending or another popup owns input. Saved drafts survive subsequent sends within the same TUI session, but not quitting the TUI. Uncertain delivery outcomes always require checking the conversation before retrying; Bloom never retries automatically.
- **Hint row:** follows the active focus and mode. While reading older messages, it says that you are reading back and that End returns to the newest without moving the draft cursor. If a send fails, a red error line replaces the hints.

The filter prompt appears in the composer area and narrows the sidebar as you type. Its popup offers All, Direct, and Groups categories; Left / Right switches categories and Up / Down selects a conversation. Settings, Contacts, Theme, New, Filter, and emoji suggestions use bottom popups above the composer; an unreadable one- or two-row transcript fragment above a tall popup is suppressed. Settings has Appearance and Layout tabs: Appearance cycles the theme through Automatic, Dark, and Light; Layout switches sidebar density and composer wrapping. The choices are stored locally. Bottom popups use roomier option spacing and join the composer without a dark spacer row. The composer stays shaded even when the chat list has keyboard focus; the shortcut sheet is centered on the whole terminal and scrolls with Up / Down or the wheel when it cannot fit. The slash-command menu has the same surface color as outgoing chat lines. Enter runs the highlighted command; bare `/theme` opens its choices on the first Enter, while `/nickname` remains an argument command. Tab switches focus between the composer and chat list even while a popup is open. Emoji suggestions appear while typing a `:shortcode` and Enter inserts the selected emoji.

Right-click a chat in the sidebar for **Rename** and **Close**. Rename sets a local title, which wins over nicknames and the engine's title (an empty name clears it). Close only hides the chat locally; its messages are kept, and `/unhide` restores every hidden chat. Both are stored in `tui.json`.

## Commands and preferences

Available slash commands are `/help`, `/sync`, `/theme [auto|dark|light]`, `/settings`, `/contacts`, `/nickname <name|clear>`, `/new`, `/filter`, `/compact`, `/unhide`, and `/cancel`. `/sync` forces a conversation-list metadata refresh and reports whether it completed, was limited, or failed; it does not fetch remote message history. After it finishes, the open thread is refreshed from Bloom's local store. Messages absent from that store cannot be recovered by `/sync`. `/help` lists commands. Bare `/theme` opens Automatic, Dark, and Light choices; an explicit `auto`, `dark`, or `light` argument still applies that theme directly. `/settings` opens local preferences for theme, sidebar density, and composer wrapping; these choices are stored locally. `/contacts` has All and Nicknamed tabs; type to search contact numbers or local nicknames case-insensitively, use Backspace or Delete to edit or clear the search, Left / Right to switch tabs, Up / Down or the mouse to select, and Enter to edit a local nickname. Blank or `clear` removes a nickname. `/new` recommends recent known contacts, with an All tab and address entry for new recipients. `/filter` opens All, Direct, and Groups conversation options. `/compact` toggles sidebar density, and `/cancel` closes the composer. The `t` shortcut also toggles the theme.

Use `/nickname <name>` to set a nickname for the selected direct chat, or `/nickname clear` to remove it. The `/contacts` view edits the same local-only nickname for any participant Bloom already knows from a chat. Nicknames apply on this device and are display labels, not transport identities; they do not change how Bloom identifies or addresses anyone on the messaging transport.

When an engine GUID arrives for a direct chat already represented by the exact same normalized participant handle, Bloom links the GUID as an identity alias. The chat keeps one stable local row, combines both histories, and sends through the engine GUID. Original encrypted conversation rows and messages are retained. Groups, different handle types, and ambiguous or conflicting identities are not merged.

These TUI preferences are persisted in `$XDG_CONFIG_HOME/bloom/tui.json` when `XDG_CONFIG_HOME` is set, or `~/.config/bloom/tui.json` otherwise.

## Accessibility

Setting `NO_COLOR` to any non-empty value disables the UI's palette distinctions. State remains readable through text and symbols, not colour alone: delivery states have labels and symbols, the filter identifies matches in text, and errors are written in the footer; bold and dim styling also distinguish delivery states. At startup the UI queries the terminal background and chooses a light or dark theme based on its brightness. If detection gets no reply, it uses the default theme. Press `t` to toggle themes; your choice takes precedence over the detected theme.

## Troubleshooting

- **A stale socket file:** harmless. The launcher tests whether a daemon answers rather than trusting the file, and the daemon reclaims its own stale socket.
- **`existing store database key is missing`:** the daemon failed closed on purpose. Nothing was deleted or overwritten. Do not remove the store to try to get past the error.
- **`another daemon is already listening`:** a daemon is already running on that socket. Check for the engine-state owner before starting another process.
- **Startup fails:** the installed launcher reports the daemon log path; the repository script also prints a short log tail. The full daemon log is at `~/.local/share/bloom/bloomd.log` by default. TUI send and connection diagnostics are in `~/.local/state/bloom/tui.log` and use allowlisted categories without message text. Review daemon logs before sharing: external sidecar/engine diagnostics are not covered by the TUI's allowlist.
