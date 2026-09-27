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
| Ctrl+C | Quit from anywhere |

While the filter or composer is open, `q` is ordinary text. That is deliberate: the active prompt owns the keyboard.

## Screens

- **Sidebar:** conversations are listed newest first, with each participant count shown as “person” or “people”. While filtering, the section label becomes the match count, or “NO MATCHES”.
- **Header:** shows Bloom and the selected conversation identity and participant count. The theme control shows the current light or dark theme when there is room; status appears when it needs attention.
- **Transcript:** each message has a speaker label and timestamp aligned in columns. Your messages also show delivery state, such as queued, sent, delivered, read, or failed.
- **Composer:** type a message after opening a conversation. For a new conversation, the prompt first asks for a recipient and then a message.
- **Hint row:** shows available shortcuts. While reading older messages, it instead says that you are reading back and that End returns to the newest. If a send fails, a red error line replaces the hints.

The filter prompt appears in the composer area and narrows the sidebar as you type. Its match count is shown in the sidebar label.

## Accessibility

Setting `NO_COLOR` to any non-empty value disables the UI's palette distinctions. State is also expressed with text and symbols, not colour alone: delivery states have labels and symbols, the filter identifies matches in text, and errors are written in the footer. Choose light or dark with `t`; the UI does not automatically detect the terminal theme.

## Troubleshooting

- **A stale socket file:** harmless. The launcher tests whether a daemon answers rather than trusting the file, and the daemon reclaims its own stale socket.
- **`existing store database key is missing`:** the daemon failed closed on purpose. Nothing was deleted or overwritten. Do not remove the store to try to get past the error.
- **`another daemon is already listening`:** a daemon is already running on that socket. Check for the engine-state owner before starting another process.
- **Startup fails:** the launcher prints the last log lines itself. The full daemon log is at `~/.local/share/bloom/bloomd.log` by default.
