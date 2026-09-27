# Using Bloom in the terminal

## What it is

Bloom is a keyboard-first iMessage client that runs in your terminal. It talks to the local `bloomd` daemon, which talks to the OpenBubbles engine installed on your machine. There is no hosted service in this path.

## Run it

From the repository, run:

```sh
./scripts/bloom
```

The launcher rebuilds the binaries when their sources changed, starts `bloomd` unless a daemon is already answering on the socket, then opens the UI. When you quit, it stops the daemon it started. Diagnostics go to `~/.local/share/bloom/bloomd.log` (or the corresponding state-home directory if configured). Paths are discovered automatically; override them in a trusted `.env` file next to the script. The file is sourced as shell code, so keep it to path settings and never put secrets there.

## TUI diagnostics

The TUI writes a private diagnostic log to `~/.local/state/bloom/tui.log` (Linux and other Unix systems; `$XDG_STATE_HOME/bloom/tui.log` when configured), `~/Library/Logs/bloom/tui.log` on macOS, or `%LOCALAPPDATA%\\bloom\\tui.log` on Windows. On Unix the directory is mode 0700 and log files mode 0600. The active log is bounded to 256 KiB with one rotated `tui.log.1`; unsafe symlink paths and logging errors are ignored so logging cannot interrupt the UI.

Entries include connection disconnection, request begin/result, an ephemeral attempt number, operation stage, elapsed time, and allowlisted IPC error category. They never include message bodies, recipient/conversation identifiers, credentials, raw IPC frames, or arbitrary error text. Timeout and transport failures are reported as uncertain rather than claiming the message was delivered. If a send looks wrong, inspect this file and share only the relevant sanitized lines.

## One rule that matters

Only one process may own the engine state: it holds your real registration. Close the OpenBubbles app before starting Bloom. To check for a leftover owner, run:

```sh
pgrep -af 'bloomd|bloom_engine'
```

## Keys

The composer is always focused, the way Codex opens: you can type immediately,
and the arrow keys scroll the transcript rather than moving the selection.

| Key | Action |
| --- | --- |
| Up / Down | Scroll the transcript one row |
| PageUp / PageDown | Scroll a screenful |
| Ctrl+Home / End | Jump to the oldest / newest message |
| Tab | Focus the chat list; Up / Down select, Enter opens |
| Enter | Send the message |
| Esc | Clear the draft, or close the open overlay |
| `/` | Command menu |
| `?` | Keyboard-shortcut sheet |
| `,` | Settings (also `Ctrl+B` then `s`) |
| `n` | Start a conversation: recipient first, then the message |
| Ctrl+W / Alt+Backspace | Delete the previous word |
| Ctrl+U / Ctrl+K | Clear before the cursor / kill to the end |
| Ctrl+A / Ctrl+E | Move to the start / end of the draft |
| Ctrl+Left / Ctrl+Right | Move by word |
| Ctrl+B, then `h` / `l` | Previous / next chat |
| Ctrl+B, then `j` / `k` | Scroll the transcript |
| Ctrl+B, then `n` / `f` / `s` / `t` | New chat / filter / settings / theme |
| Ctrl+C | Quit from anywhere |

Ctrl+B is a leader: press it, then the indicated key. `q` is ordinary text, as in
Codex; Ctrl+C is the quit key.

## Mouse

Mouse reporting is on. The wheel scrolls whatever is under the pointer: the
transcript in the pane, the selection in the chat list. Left click selects a chat,
focuses the composer, runs a command from the popup, toggles a settings row, or
opens the header's `⚙` settings and `?` shortcuts controls. The hitboxes come from
the last frame that was drawn, so a click can only land on something visible.

## Screens

The window has no outer frame; content runs to the edges like Codex.

- **Sidebar:** conversations are listed newest first. Each chat shows its name with the participant count right-aligned, and underneath, a preview of the newest message Bloom has stored (your own prefixed `You:`), clipped with `…`. The selection highlights both lines. Below 70 columns, or with `/compact`, entries collapse to one line. A direct chat with no title is named after the other participant (their display name, or their handle when the engine has none), and an untitled group lists its people. While filtering, the section label becomes the match count, or “NO MATCHES”.
- **Header:** leads with the selected conversation, with the recipient handle for a direct chat or the participant count for a group (dropped whole when it does not fit). The right side shows the status, the theme control, `⚙`, `?`, and `Bloom`. A status dot and label show the engine session or connection state: green for healthy, yellow for attention, and red for failure. A braille spinner beside Connecting or Disconnected indicates connection/retry activity.
- **Transcript:** consecutive messages from one sender within five minutes share a single byline (a bold, per-speaker colored name and a timestamp), a centered divider marks each new day, and a blank row separates senders. Your messages also show delivery state, such as queued, sent, delivered, read, or failed. An outgoing message still queued for delivery has a spinner beside “queued”.
- **Composer:** a bare `›` prompt, always focused. For a new conversation, the prompt first asks for a recipient and then a message. Sending shows `Sending…`; `Message sent.` appears only after IPC accepts the message. A failure is shown in red. Bloom does not claim Delivered or Read without transport events.
- **Hint row:** shows available shortcuts. While reading older messages, it instead says that you are reading back and that End returns to the newest without moving the draft cursor. If a send fails, a red error line replaces the hints.

The filter prompt appears in the composer area and narrows the sidebar as you type. Its match count is shown in the sidebar label. The command menu opens as a full-width, borderless list above the composer, left-aligned with the chat, and takes priority over the transcript. Enter acts on the highlighted command: commands that open something (`/settings`, `/filter`, `/new`, `/compact`, `/unhide`, `/help`, `/cancel`) run immediately, while `/nickname` and `/theme` fill the composer so you can type an argument. Tab always fills the composer. The shortcut sheet opens the same way.

Right-click a chat in the sidebar for **Rename** and **Close**. Rename sets a local title, which wins over nicknames and the engine's title (an empty name clears it). Close only hides the chat locally; its messages are kept, and `/unhide` restores every hidden chat. Both are stored in `tui.json`.

## Commands and preferences

Available slash commands are `/help`, `/theme [auto|dark|light]`, `/settings`, `/nickname <name|clear>`, `/new`, `/filter`, `/compact`, `/unhide`, and `/cancel`. `/help` lists commands. `/theme` inspects or sets the theme: `auto` follows the detected terminal background, while `dark` and `light` select one explicitly. `/settings` opens local preferences for theme and sidebar density. `/new` starts a chat, `/filter` opens the conversation filter, `/compact` toggles sidebar density, and `/cancel` closes the composer. The `t` shortcut also toggles the theme.

Use `/nickname <name>` to set a nickname for the selected direct chat, or `/nickname clear` to remove it. Nicknames apply only to that direct chat on this device. They are local display labels, not transport identities, and do not change how Bloom identifies or addresses anyone on the messaging transport.

These TUI preferences are persisted in `$XDG_CONFIG_HOME/bloom/tui.json` when `XDG_CONFIG_HOME` is set, or `~/.config/bloom/tui.json` otherwise.

## Accessibility

Setting `NO_COLOR` to any non-empty value disables the UI's palette distinctions. State remains readable through text and symbols, not colour alone: delivery states have labels and symbols, the filter identifies matches in text, and errors are written in the footer; bold and dim styling also distinguish delivery states. At startup the UI queries the terminal background and chooses a light or dark theme based on its brightness. If detection gets no reply, it uses the default theme. Press `t` to toggle themes; your choice takes precedence over the detected theme.

## Troubleshooting

- **A stale socket file:** harmless. The launcher tests whether a daemon answers rather than trusting the file, and the daemon reclaims its own stale socket.
- **`existing store database key is missing`:** the daemon failed closed on purpose. Nothing was deleted or overwritten. Do not remove the store to try to get past the error.
- **`another daemon is already listening`:** a daemon is already running on that socket. Check for the engine-state owner before starting another process.
- **Startup fails:** the launcher prints the last log lines itself. The full daemon log is at `~/.local/share/bloom/bloomd.log` by default.
