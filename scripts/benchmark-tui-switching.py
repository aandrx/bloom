#!/usr/bin/env python3
"""Opt-in, synthetic actual-PTY switching latency probe (not a CI timing gate).

Build bloom-tui first and run with the Python/pyte environment used by the E2E
suites. Measures input to readable reconstructed terminal state, not compositor
presentation or a real engine. Example stress case: --body-repeat 100.

The very first chat_next_body sample is the cold layout miss: BOB has never
been laid out at startup, so later samples are warm cache hits. That first
sample is also recorded once as chat_first_visit, without changing the
existing series. Returning to ALICE is not a cold case: it was laid out at
startup and stays inside the four-entry cache with only two chats.

Use --binary to compare build profiles with the same fixture; the default is
the debug helper build.
"""
import argparse
import fcntl
import json
import math
import os
from pathlib import Path
import pty
import runpy
import select
import signal
import statistics
import struct
import termios
import time

HERE = Path(__file__).resolve().parent
H = runpy.run_path(str(HERE / 'test-tui-messaging-e2e.py'), run_name='helpers')
F = runpy.run_path(str(HERE / 'stateful-fake-daemon.py'), run_name='helpers')


def benchmark(args):
    fixture = H['MessagingFlows']()
    binary = Path(args.binary)
    fixture.setUp()
    pid = master = None
    try:
        state = F['initial']()
        for chat in state['conversations']:
            who = chat['title'].upper()
            for index in range(args.messages):
                marker = f'{who}_HISTORY_{index:03}'
                at = 1_700_000_000_000 + index + (1000 if who == 'ALICE' else 0)
                state['messages'].append({
                    'id': {'transport': 'openbubbles', 'value': f'{who}-{index}'},
                    'conversation_id': chat['id'], 'remote_id': None,
                    'sender': chat['participants'][1]['handle'],
                    'parts': [{'type': 'text', 'value': (marker + ' synthetic context ') * args.body_repeat}],
                    'created_at': at, 'sent_at': None, 'delivery': 'delivered',
                    'from_me': False, 'edited_at': None, 'reactions': [], 'unsent_at': None,
                })
            chat['last_message_at'] = at
        fixture.store_path.write_text(json.dumps(state))
        fixture.start_daemon()
        with H['isolated_environment']({
            'BLOOM_SOCKET_PATH': str(fixture.socket_path),
            'XDG_CONFIG_HOME': str(fixture.root / 'config'),
            'XDG_STATE_HOME': str(fixture.root / 'state'),
            'XDG_DATA_HOME': str(fixture.root / 'data'),
            'NO_COLOR': '1',
        }):
            pid, master = pty.fork()
            if pid == 0:
                try:
                    os.execv(str(binary), [str(binary)])
                except OSError:
                    os._exit(127)
        screen = H['SHOT']['pyte'].Screen(args.columns, 30)
        stream = H['SHOT']['pyte'].ByteStream(screen)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 30, args.columns, 0, 0))
        os.kill(pid, signal.SIGWINCH)

        def text():
            return '\n'.join(screen.display)

        def until(predicate):
            deadline = time.perf_counter() + 15
            while not predicate():
                remaining = deadline - time.perf_counter()
                if remaining <= 0:
                    raise RuntimeError('Synthetic TUI did not reach the expected state:\n' + text())
                if select.select([master], [], [], min(.003, remaining))[0]:
                    data = os.read(master, 1_048_576)
                    if not data:
                        raise RuntimeError('Synthetic TUI exited before the expected state')
                    stream.feed(data)

        def chat_visible(who):
            other = 'BOB' if who == 'ALICE' else 'ALICE'
            current = text()
            return (who.title() in screen.display[0]
                    and f'{who}_HISTORY_{args.messages - 1:03}' in current
                    and f'{other}_HISTORY_' not in current)

        until(lambda: chat_visible('ALICE'))
        timings = {}

        def measure(label, key, predicate):
            started = time.perf_counter()
            os.write(master, key)
            until(predicate)
            elapsed = (time.perf_counter() - started) * 1000
            timings.setdefault(label, []).append(elapsed)
            return elapsed

        for _ in range(args.samples):
            measure('settings_open', b',', lambda: 'Settings' in text())
            measure('settings_close', b'\x1b', lambda: 'Settings' not in text())
            measure('help_open', b'?', lambda: 'Keyboard shortcuts' in text())
            measure('help_close', b'\x1b', lambda: 'Keyboard shortcuts' not in text())
        if args.columns >= 70:
            for _ in range(args.samples):
                next_elapsed = measure('chat_next_body', b'\x02n', lambda: chat_visible('BOB'))
                if 'chat_first_visit' not in timings:
                    # Only the first BOB layout has no cached dimensions or text.
                    timings['chat_first_visit'] = [next_elapsed]
                measure('chat_previous_body', b'\x02p', lambda: chat_visible('ALICE'))
        else:
            for _ in range(args.samples):
                measure('chats_open', b'\t', lambda: 'Find:' in text())
                measure('chats_close', b'\x1b', lambda: 'Find:' not in text())
        summary = {}
        for label, samples in timings.items():
            summary[label] = {
                'median_ms': round(statistics.median(samples), 2),
                'p95_ms': round(sorted(samples)[math.ceil(len(samples) * .95) - 1], 2),
                'max_ms': round(max(samples), 2),
                'samples_ms': [round(value, 2) for value in samples],
            }
        print(json.dumps({
            'boundary': 'input_to_readable_pyte_state_not_compositor',
            'binary': str(binary), 'columns': args.columns, 'rows': 30,
            'messages_per_chat': args.messages, 'body_repeat': args.body_repeat,
            'timings': summary,
        }, indent=2))
    finally:
        if pid is not None and pid > 0:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
            os.close(master)
        fixture.tearDown()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--messages', type=int, default=200)
    parser.add_argument('--body-repeat', type=int, default=10)
    parser.add_argument('--samples', type=int, default=8)
    parser.add_argument('--columns', type=int, default=100)
    parser.add_argument(
        '--binary',
        default=str(H['BINARY']),
        help='TUI binary to measure (default: the debug helper build)',
    )
    options = parser.parse_args()
    options.binary = str(Path(options.binary).expanduser().resolve())
    if not Path(options.binary).is_file():
        parser.error('--binary must name an existing file')
    if not 2 <= options.messages <= 200:
        parser.error('--messages must be between 2 and the local-history limit of 200')
    if not 1 <= options.body_repeat <= 100:
        parser.error('--body-repeat must be between 1 and 100')
    if not 1 <= options.samples <= 100:
        parser.error('--samples must be between 1 and 100')
    if not 24 <= options.columns <= 200:
        parser.error('--columns must be between 24 and 200')
    benchmark(options)
