# Using Bloom in the terminal

## What it is

Bloom is a keyboard-first iMessage client that runs in your terminal. It talks to the local `bloomd` daemon, which talks to the OpenBubbles engine installed on your machine. There is no hosted service in this path.

## Run it

From the repository, run:

```sh
./scripts/bloom
```

The launcher rebuilds the binaries when their sources changed, starts `bloomd` unless a daemon is already answering on the socket, then opens the UI. When you quit, it stops the daemon it started. Diagnostics go to `~/.local/share/bloom/bloomd.log` (or the corresponding state-home directory if configured). Paths are discovered automatically; override them in a trusted `.env` file next to the script. The file is sourced as shell code, so keep it to path settings and never put secrets there.

## One rule that matters

Only one process may own the engine state: it holds your real registration. Close the OpenBubbles app before starting Bloom. To check for a leftover owner, run:

```sh
pgrep -af 'bloomd|bloom_engine'
```

## Keys

| Key | Action |
| --- | --- |
| Up / Down | Move through conversations |
| Enter | Open the selected conversation, or send the composed message |
| `/` | Open the conversation filter |
| `n` | Start a conversation: enter the recipient first, then the message |
| PageUp / PageDown | Scroll the transcript older / newer |
| Home / End | Jump to the oldest / newest transcript content |
| `t` | Toggle light and dark themes |
| Esc | Cancel the filter or composition |
| `q` | Quit from the conversation list |
| Ctrl+B, then `h` / `l` | Select previous / next chat |
| Ctrl+B, then `j` / `k` | Scroll transcript older / newer |
| Ctrl+B, then `n` / `f` / `t` | Start a chat / filter / change theme |
| Ctrl+C | Quit from anywhere |

Ctrl+B is a leader: press it, then the indicated key. While the filter or composer is open, `q` is ordinary text. That is deliberate: the active prompt owns the keyboard.

## Screens

- **Sidebar:** conversations are listed newest first, with each participant count right-aligned on the same line as its name. While filtering, the section label becomes the match count, or “NO MATCHES”.
- **Header:** shows Bloom and the selected conversation identity and participant count. The theme control shows the current light or dark theme when there is room. A status dot and label show the engine session or connection state: green for healthy, yellow for attention, and red for failure. A braille spinner beside Connecting or Disconnected indicates connection/retry activity.
- **Transcript:** each message has a speaker label and timestamp aligned in columns. Your messages also show delivery state, such as queued, sent, delivered, read, or failed. An outgoing message still queued for delivery has a spinner beside “queued”.
- **Composer:** type a message after opening a conversation. For a new conversation, the prompt first asks for a recipient and then a message. Sending shows `Sending…`; `Message sent.` appears only after IPC accepts the message. A failure is shown in red. Bloom does not claim Delivered or Read without transport events.
- **Hint row:** shows available shortcuts. While reading older messages, it instead says that you are reading back and that End returns to the newest. If a send fails, a red error line replaces the hints.

The filter prompt appears in the composer area and narrows the sidebar as you type. Its match count is shown in the sidebar label.

## Commands and preferences

Available slash commands are `/help`, `/theme [auto|dark|light]`, `/settings`, `/nickname <name|clear>`, `/new`, `/filter`, `/compact`, and `/cancel`. `/help` lists commands. `/theme` inspects or sets the theme: `auto` follows the detected terminal background, while `dark` and `light` select one explicitly. `/settings` opens local preferences for theme and sidebar density. `/new` starts a chat, `/filter` opens the conversation filter, `/compact` toggles sidebar density, and `/cancel` closes the composer. The `t` shortcut also toggles the theme.

Use `/nickname <name>` to set a nickname for the selected direct chat, or `/nickname clear` to remove it. Nicknames apply only to that direct chat on this device. They are local display labels, not transport identities, and do not change how Bloom identifies or addresses anyone on the messaging transport.

These TUI preferences are persisted in `$XDG_CONFIG_HOME/bloom/tui.json` when `XDG_CONFIG_HOME` is set, or `~/.config/bloom/tui.json` otherwise.

## Accessibility

Setting `NO_COLOR` to any non-empty value disables the UI's palette distinctions. State remains readable through text and symbols, not colour alone: delivery states have labels and symbols, the filter identifies matches in text, and errors are written in the footer; bold and dim styling also distinguish delivery states. At startup the UI queries the terminal background and chooses a light or dark theme based on its brightness. If detection gets no reply, it uses the default theme. Press `t` to toggle themes; your choice takes precedence over the detected theme.

## Troubleshooting

- **A stale socket file:** harmless. The launcher tests whether a daemon answers rather than trusting the file, and the daemon reclaims its own stale socket.
- **`existing store database key is missing`:** the daemon failed closed on purpose. Nothing was deleted or overwritten. Do not remove the store to try to get past the error.
- **`another daemon is already listening`:** a daemon is already running on that socket. Check for the engine-state owner before starting another process.
- **Startup fails:** the launcher prints the last log lines itself. The full daemon log is at `~/.local/share/bloom/bloomd.log` by default.
