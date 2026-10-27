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

The repository launcher invokes Cargo's incremental build, starts `bloomd` unless a daemon is already answering on the socket, then opens the UI. Pass `--release` to build and run the optimized profile; debug remains the default. When you quit, it stops the daemon it started. Diagnostics go to `~/.local/share/bloom/bloomd.log` (or the corresponding state-home directory if configured). Paths are discovered automatically; override them in a trusted `.env` file at the repository root. The file is sourced as shell code, so keep it to path settings and never put secrets there.

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
switches between the composer and chat list; below 70 columns it opens a full-screen
Chats navigator instead of squeezing chat identities into a tiny rail. Arrow keys
follow the visible input owner.

| Key | Action |
| --- | --- |
| Up / Down | Move through wrapped draft lines; at an edge, scroll the transcript. In the chat list, select a chat. |
| Ctrl+Z / Ctrl+Y | Undo / redo the composer: a typed run, a deletion run, a paste, or a clear is one step |
| Ctrl+F | Search stored text and reported filenames |
| PageUp / PageDown | Scroll a screenful |
| Ctrl+Home / End | Jump to the oldest / newest message |
| Tab | Toggle composer/chat-list focus; on narrow screens, open or close Chats |
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
| Ctrl+P | Previous chat; inside Search, open the participant picker |
| Ctrl+B, then `j` / `k` | Scroll the transcript |
| Ctrl+B, then `f` / `s` / `t` / `u` | Filter / settings / theme / first local unread message |
| Ctrl+C | Clear a nonempty input first; quit when it is already empty |
| Settings | Left / Right switch tabs; Up / Down select; Space toggles; Enter changes |

Ctrl+B is a leader: press it, then the indicated key. `n` is text while the composer
is focused; use `/new` to start a conversation. Ctrl+C clears the composer or filter
query before quitting on a subsequent press with an empty line. In Chats, Ctrl+C
closes navigation without clearing the parked message body.

## Mouse

Mouse reporting is on. The wheel scrolls whatever is under the pointer: the
transcript in the pane, the selection in the chat list. Left click selects a chat,
focuses the composer, runs a command from the popup, toggles a settings row, or
opens the header's `⚙` settings and `?` shortcuts controls. The hitboxes come from
the last frame that was drawn, so a click can only land on something visible.
Overlays own wheel gestures at their painted location; unsupported panel scrolling
is consumed rather than moving hidden history. Uncovered panes can still scroll.
Click `↓ Latest` while reading back to return without moving the draft cursor.
On narrow screens, click `‹ Chats` to navigate; clicking a candidate opens it.

## Screens

The window has no outer frame; content runs to the edges like Codex.

- **Sidebar:** conversations are listed newest first. Each chat shows its name and, underneath, a preview of the newest message Bloom has stored (your own prefixed `You:`), clipped with `…`. The selection highlights both lines. With `/compact`, entries collapse to one line. Below 70 columns the rail is hidden and the conversation uses the full width. A direct chat with no title is named after the other participant (their display name, or their handle when the engine has none), and an untitled group lists its people. While filtering, the section label becomes the match count, or “NO MATCHES”.
- **Narrow navigation:** `‹ Chats` or Tab opens a full-screen list with readable identities, local previews and an independent `Find:` query. Type or paste a name/address; Up / Down, PageUp / PageDown and Home / End choose a candidate. Enter or a candidate click opens it. Esc, Tab or Ctrl+C returns to the exact previous composer/panel without changing the recipient or draft. Highlighting/searching never selects a send target. The navigator remains modal if the terminal expands. Empty results stay open on Enter so the query can be corrected.
- **Header:** leads with the selected conversation, with the recipient handle for a direct chat or the participant count for a group (dropped whole when it does not fit). Identity and status get space before optional controls. The right side can show the theme control, `⚙`, `?`, and `Bloom`; branding, theme, then settings disappear as space runs out. On narrow screens with enough height, status moves to a second row instead of squeezing the title. Long identities use `…`; `/settings`, `/theme`, and `?` remain keyboard-accessible even when their header controls are hidden. A status dot and label show the engine session or connection state: green for healthy, yellow for attention, and red for failure. A braille spinner beside Connecting or Disconnected indicates connection/retry activity. Inbound message/update/delivery/list activity sets an inferred `Syncing` indicator with a spinner; it clears after two quiet seconds. This is not authoritative sync completion or evidence that remote history was imported. Failed, blocked, or disconnected engine states take precedence. Running `/sync` shows the same label until the metadata refresh finishes.
- **Transcript:** consecutive messages from one sender within five minutes share a single byline (a bold, per-speaker colored name and a timestamp), a centered divider marks each new day, and a blank row separates senders. Your messages also show delivery state, such as queued, sent, delivered, read, or failed. An outgoing message still queued for delivery has a spinner beside “queued”. Incoming events update immediately; the selected thread also reconciles against Bloom's stored history every five seconds to recover from missed live events. Bloom opens the newest 200 stored messages; scrolling back to the oldest one loads the previous page from local storage, shows “Loading older messages…” while it is in flight, and keeps the reading position anchored where it can while never dropping the messages that were on screen. Loaded pages are kept for the session up to a bounded budget and survive the five-second reconciliation; a chat you never paged back keeps replacing wholesale. Older history is not fetched from the engine or the phone. A deliberate reading position stays anchored to its message through arrivals, updates, reconciliation and resize while that message remains in the loaded history. End or `↓ Latest` explicitly resumes following new messages; already-following readers stay at latest. If stored history drops the anchor, Bloom clamps to available content. This is local paging, not authoritative unread tracking.
- **Composer:** a bare `›` prompt, focused initially. Drafts wrap at grapheme boundaries by default; Up / Down move the cursor between wrapped lines, preserving its visual column, and show `↑ N more` / `↓ N more` when a long draft exceeds the available rows. Toggle wrapping under Settings > Layout; the choice is stored locally, and disabled wrapping keeps the old horizontal scroll behavior. Multiline entry uses Ctrl+J (Alt+Enter and Shift+Enter also work where the terminal reports them) and bracketed paste, so a pasted multi-line body keeps its blank lines. Pasting literal text never runs a slash command: a paste that begins with `/new` stays a message body and is only sent by an explicit Enter. For a new conversation, the prompt first asks for a recipient and then a message. Sending shows `Sending…`; `Message sent.` appears only after IPC accepts the message. A failure is shown in red. Bloom does not claim Delivered or Read without transport events. Drafts are held per recipient for the session. Settings, theme, contacts, and rename use suspended composition; closing a panel restores only the matching recipient's draft, including cursor and literal-paste provenance. New-chat bodies are bound to their entered recipient rather than a shared scratch body. Explicit cancellation may discard the current new-chat draft; closing the TUI discards all session drafts.
- **New-chat completion:** the accepted message ID is correlated with its actual message and conversation metadata before opening the recipient's transcript. Live events, the acknowledgment, and list refreshes may arrive in any order. Newer text, a changed selection, or another active prompt prevents automatic navigation. Without a matching live message, Bloom reports a successful new-chat send but does not guess which thread to open. While the new-chat composer is open the transcript shows the new-message state, not the previously selected chat's history.
- **Send recovery:** rejected sends restore the body and new-chat recipient only when the composing context is unchanged and empty. Otherwise drafts remain queued in memory until explicitly recovered with Ctrl+R; newer text and another recipient prompt are never overwritten. Recovery is disabled while a request is pending or another popup owns input. Saved drafts survive subsequent sends within the same TUI session, but not quitting the TUI. Uncertain delivery outcomes always require checking the conversation before retrying; Bloom never retries automatically.
- **Hint row:** follows the active focus and mode. While reading older messages, it says that you are reading back and that End returns to the newest without moving the draft cursor. If a send fails, a red error line replaces the hints.

The filter prompt appears in the composer area and narrows the sidebar as you type. Its popup offers All, Direct, and Groups categories; Left / Right switches categories and Up / Down selects a conversation. Settings, Contacts, Theme, New, Filter, and emoji suggestions use bottom popups above the composer; an unreadable one- or two-row transcript fragment above a tall popup is suppressed. Settings has Appearance and Layout tabs: Appearance cycles the theme through Automatic, Dark, and Light; Layout switches sidebar density and composer wrapping. The choices are stored locally. Bottom popups use roomier option spacing and join the composer without a dark spacer row. The composer stays shaded even when the chat list has keyboard focus; the shortcut sheet is centered on the whole terminal and scrolls with Up / Down or the wheel when it cannot fit. The slash-command menu has the same surface color as outgoing chat lines. Enter runs the highlighted command; bare `/theme` opens its choices on the first Enter, while `/nickname` remains an argument command. On wide screens Tab switches composer/chat-list focus even while a popup is open; on narrow screens it opens Chats above the panel, and cancelling Chats restores that panel. Emoji suggestions appear while typing a `:shortcode` and Enter inserts the selected emoji.

Right-click a chat in the sidebar for **Rename** and **Close**. Rename sets a local title, which wins over nicknames and the engine's title (an empty name clears it). Close only hides the chat locally; its messages are kept, and `/unhide` restores every hidden chat. Both are stored in `tui.json`.

## Commands and preferences

Available slash commands are `/help`, `/search [query]`, `/attachment`, `/attach <path>`, `/open`, `/unread`, `/sync`, `/theme [auto|dark|light]`, `/settings`, `/contacts`, `/nickname <name|clear>`, `/new`, `/filter`, `/compact`, `/unhide`, and `/cancel`. `/search` or Ctrl+F opens a message search over Bloom's stored text and reported attachment filenames; type a query, press Enter for results, use Up / Down to choose, and press Enter again to open that message. A hit already in loaded history anchors there directly. Otherwise Bloom opens a separate Search context view with the exact message and up to 200 nearby stored messages, even beyond the normal paging budget. This fixed preview stays anchored through arrivals and reconciliation. End or the clickable Latest control returns to normal latest history without changing the draft. Missing targets and failed or oversized context requests show a notice rather than silently opening another message. Search reads stored text and explicit filename metadata. Matching text takes priority within each message; otherwise the first matching filename appears as an escaped `Filename: ` excerpt. Search does not query the engine or phone, read file contents, infer missing names, or open attachments. `/attachment` opens a chooser over every fetchable attachment in the open chat: Up / Down move, Enter saves the choice into the daemon's private cache and shows the local path, `o` saves it and launches the desktop opener, and Esc cancels. Only attachments that still carry a live sidecar fetch token are listed, newest first; a restarted sidecar generation cannot be recovered. `/open` skips the chooser and fetches plus opens the newest attachment. `/attach <absolute path>` uploads and sends one local file to the open chat. The path must be absolute, regular, not a symlink, free of `.` and `..`, and at most 25 MiB; only the file name appears in notices, never the source path. A second send is refused while one is in flight, and a timeout is reported as an uncertain outcome rather than retried. Remote delivery of a sent attachment is not yet live-verified. Both inbound attachment commands are explicit: nothing downloads or opens on its own. The opener is `xdg-open`, or the program named by `BLOOM_OPENER`; a path is refused unless it is absolute, regular and non-empty, and it is passed as one argument with no shell. Bloom reports that the opener started, not that a viewer displayed anything. Search refuses to open while a new message is being composed. `/sync` forces a conversation-list metadata refresh and reports whether it completed, was limited, or failed; it does not fetch remote message history. After it finishes, the open thread is refreshed from Bloom's local store. Messages absent from that store cannot be recovered by `/sync`. Ctrl+Z undoes one composer gesture at a time (a typed run, a deletion run, a paste, or an input clear), Ctrl+Y redoes it, and the history is dropped when the draft changes owner or is sent or cancelled. Search shows a result count, and marks a list that hit the search limit. `?` or `/help` opens the shortcut sheet, and typing `/` lists commands. Bare `/theme` opens Automatic, Dark, and Light choices; an explicit `auto`, `dark`, or `light` argument still applies that theme directly. `/settings` opens local preferences for theme, sidebar density, and composer wrapping; these choices are stored locally. `/contacts` has All and Nicknamed tabs; type to search contact numbers or local nicknames case-insensitively, use Backspace or Delete to edit or clear the search, Left / Right to switch tabs, Up / Down or the mouse to select, and Enter to edit a local nickname. Blank or `clear` removes a nickname. `/new` recommends recent known contacts, with an All tab and address entry for new recipients. `/filter` opens All, Direct, and Groups conversation options. `/compact` toggles sidebar density, and `/cancel` closes the composer. The `t` shortcut also toggles the theme.

Use `/nickname <name>` to set a nickname for the selected direct chat, or `/nickname clear` to remove it. The `/contacts` view edits the same local-only nickname for any participant Bloom already knows from a chat. Nicknames apply on this device and are display labels, not transport identities; they do not change how Bloom identifies or addresses anyone on the messaging transport.

When an engine GUID arrives for a direct chat already represented by the exact same normalized participant handle, Bloom links the GUID as an identity alias. The chat keeps one stable local row, combines both histories, and sends through the engine GUID. Original encrypted conversation rows and messages are retained. Groups, different handle types, and ambiguous or conflicting identities are not merged.

### Search chats with a participant

Inside Search, press Ctrl+P to open **Chats with**. Type or paste a name, nickname, handle, or transport to narrow the picker. Use arrows and Enter, or click a painted row, to select an exact handle and transport. Names are labels, not unique identities. Below 70 columns the picker uses the full frame, and identities wrap rather than silently hiding distinguishing text. Resize if a full identity cannot fit. The first row, **All chats**, clears the scope without clearing the text query. Typing resets the highlight to All chats; use Down to choose a person before pressing Enter. Esc, Ctrl+P, or Ctrl+C cancels only the inner picker; Esc from Search restores the parked draft.

After selecting scope, press Enter to run the existing text query. Changing scope clears old results. The query remains literal: `from:alice` is text, not sender-filter syntax. Results include messages in chats whose current locally stored participant list contains the selected identity, even if somebody else sent the message. This is not historical group membership.

Filtering occurs before the result limit, within the newest 20,000 stored messages globally. It is not an exhaustive archive search. Participant choice never changes the send recipient; only explicitly opening a search hit navigates to its conversation.

### Local unread navigation

A badge such as `Bob · 3` counts incoming loaded messages newer than that chat's saved read position. Your own messages do not count. `200+` is a conservative lower bound when the entire loaded page is newer than the marker. A chat without a marker or loaded history has unknown unread state and shows no badge.

Following latest in a successfully displayed transcript advances the marker. Reading back, open overlays, and frames without a transcript do not. `/unread` or Ctrl+B then `u` jumps to the first unread loaded message. It cannot locate unloaded unread history. These are local navigation markers, not remote read receipts.

These TUI preferences are persisted in `$XDG_CONFIG_HOME/bloom/tui.json` when `XDG_CONFIG_HOME` is set, or `~/.config/bloom/tui.json` otherwise. Read markers add conversation IDs, message IDs, and message timestamps to this private but unencrypted file. No message bodies or drafts are stored there. Markers accumulate automatically and have no automatic expiry.

## Accessibility

Setting `NO_COLOR` to any non-empty value disables the UI's palette distinctions. State remains readable through text and symbols, not colour alone: delivery states have labels and symbols, the filter identifies matches in text, and errors are written in the footer; bold and dim styling also distinguish delivery states. At startup the UI queries the terminal background and chooses a light or dark theme based on its brightness. If detection gets no reply, it uses the default theme. Press `t` to toggle themes; your choice takes precedence over the detected theme.

## Troubleshooting

- **Lag switching panels/chats:** transcript layouts are cached for four recent identity/width/palette variants and only visible lines are painted. Cold or changed large histories and IPC refreshes can still cost more than a warm local switch. Use `scripts/benchmark-tui-switching.py` for an isolated synthetic latency probe, not a live-account benchmark. See [performance evidence and limits](tui-switching-performance.md). After updating/building, reopen the TUI to load the new executable; preserve unsent drafts first because they are session-only.

- **After an IPC upgrade:** client and daemon must use the same protocol version, currently 6. A newly built client rejects a reused older daemon with "unsupported IPC protocol version". Restart the daemon through its owning launcher after preserving session drafts; do not start another engine owner.
- **A stale socket file:** harmless. The launcher tests whether a daemon answers rather than trusting the file, and the daemon reclaims its own stale socket.
- **`existing store database key is missing`:** the daemon failed closed on purpose. Nothing was deleted or overwritten. Do not remove the store to try to get past the error.
- **`another daemon is already listening`:** a daemon is already running on that socket. Check for the engine-state owner before starting another process.
- **Startup fails:** the installed launcher reports the daemon log path; the repository script also prints a short log tail. The full daemon log is at `~/.local/share/bloom/bloomd.log` by default. TUI send and connection diagnostics are in `~/.local/state/bloom/tui.log` and use allowlisted categories without message text. Review daemon logs before sharing: external sidecar/engine diagnostics are not covered by the TUI's allowlist.
