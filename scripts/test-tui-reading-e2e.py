#!/usr/bin/env python3
"""Reading and overlay regressions against the real TUI and private synthetic IPC.

Build bloom-tui first; use a Python interpreter with pyte. No real engine/account.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import runpy
import select
import signal
import struct
import termios
import time
import unittest

HERE = Path(__file__).resolve().parent
H = runpy.run_path(str(HERE / 'test-tui-messaging-e2e.py'), run_name='helpers')
R = runpy.run_path(str(HERE / 'test-stateful-fake-daemon.py'), run_name='helpers')
CID = {'transport': 'openbubbles', 'value': 'alice@example.invalid'}
BOB = {'transport': 'openbubbles', 'value': 'bob@example.invalid'}


class ReadingFlows(unittest.TestCase):
    def setUp(self):
        self.fixture = H['MessagingFlows']()
        self.fixture.setUp()
        self.addCleanup(self.fixture.tearDown)
        self.fixture.start_daemon()
        for index in range(60):
            self.send_message(index)
        self.start_tui()
        self.addCleanup(self.stop_tui)
        self.resize(100, 30)
        self.wait_for(lambda: 'READ_MARKER_059' in self.text())

    def start_tui(self):
        with H['isolated_environment']({
            'BLOOM_SOCKET_PATH': str(self.fixture.socket_path),
            'XDG_CONFIG_HOME': str(self.fixture.root / 'config'),
            'XDG_STATE_HOME': str(self.fixture.root / 'state'),
            'XDG_DATA_HOME': str(self.fixture.root / 'data'),
            'NO_COLOR': '1',
        }):
            self.pid, self.master = pty.fork()
            if self.pid == 0:
                os.execv(str(H['BINARY']), [str(H['BINARY'])])

    def stop_tui(self):
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        os.waitpid(self.pid, 0)
        os.close(self.master)

    def send_message(self, index):
        R['request'](str(self.fixture.socket_path), 'send', conversation_id=CID,
                     body={'parts': [{'type': 'text', 'value': f'READ_MARKER_{index:03d} synthetic context'}]})

    def resize(self, width, height):
        self.width, self.height = width, height
        self.screen = H['SHOT']['pyte'].Screen(width, height)
        self.stream = H['SHOT']['pyte'].ByteStream(self.screen)
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack('HHHH', height, width, 0, 0))
        os.kill(self.pid, signal.SIGWINCH)

    def pump(self, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([self.master], [], [], min(.03, max(0, deadline - time.monotonic())))[0]:
                self.stream.feed(os.read(self.master, 65536))

    def wait_for(self, predicate, timeout=5):
        deadline = time.monotonic() + timeout
        while not predicate() and time.monotonic() < deadline:
            self.pump(.05)
        self.assertTrue(predicate(), self.text())

    def text(self):
        return '\n'.join(self.screen.display)

    def markers(self):
        # Exclude sidebar previews, header and composition. The divider is drawn
        # at the actual sidebar boundary; a full-width narrow layout has none.
        divider = self.screen.display[0].find('│')
        left = divider + 3 if divider >= 0 else 2
        return re.findall(r'(?:READ|OLDER)_MARKER_\d+',
                          '\n'.join(line[left:] for line in self.screen.display[2:-5]))

    def key(self, value):
        os.write(self.master, value)
        self.pump(.2)

    def artifact(self, name):
        root = os.environ.get('BLOOM_E2E_ARTIFACTS')
        if root:
            destination = Path(root) / self._testMethodName
            destination.mkdir(parents=True, exist_ok=True)
            (destination / (name + '.txt')).write_text(self.text() + '\n')
            (destination / (name + '.html')).write_text(H['SHOT']['to_html'](self.screen, self.width, self.height))

    def append_fixture_message(self, conversation_id, text):
        return R['request'](str(self.fixture.socket_path), 'fixture_append_message',
                            conversation_id=conversation_id, text=text)

    def test_unread_badge_open_reads_and_jump_to_first_unread(self):
        # Give Bob enough history that one page-up is a deliberate reading position
        # rather than a no-op on content that already fits.
        for index in range(24):
            self.append_fixture_message(BOB, f'BOB_BASE_MARKER_{index:03d} synthetic history')
        self.wait_for(lambda: 'BOB_BASE_MARKER_023' in self.text(), timeout=15)
        self.key(b'\x02p')  # Ctrl+B p: Bob is newest after the fixture appends.
        self.wait_for(lambda: 'Bob' in self.screen.display[0])
        self.wait_for(lambda: 'BOB_BASE_MARKER_023' in self.text())
        self.key(b'\x1b[5~')  # Read back so arriving messages stay unread.

        for index in range(3):
            self.append_fixture_message(
                BOB, f'UNREAD_MARKER_{index:03d} synthetic unread')
        self.wait_for(lambda: 'UNREAD_MARKER_002' in self.text(), timeout=15)
        unread = [item for item in self.fixture.persisted()['messages']
                  if item['conversation_id'] == BOB
                  and item['parts'][0]['value'].startswith('UNREAD_MARKER_')]
        newest_at = max(item['created_at'] for item in unread)
        newest_ids = {item['id']['value'] for item in unread
                      if item['created_at'] == newest_at}

        self.key(b'\x02n')  # Back to Alice; Bob is no longer selected.
        self.wait_for(lambda: 'Alice' in self.screen.display[0])
        self.wait_for(lambda: 'Bob · 3' in self.text())
        self.artifact('unread-badge-before-open')

        # Opening Bob follows its newest row, which marks the local position read.
        self.key(b'\x02p')
        self.wait_for(lambda: 'Bob' in self.screen.display[0])
        config_path = self.fixture.root / 'config' / 'bloom' / 'tui.json'

        def bob_position():
            try:
                config = json.loads(config_path.read_text())
            except (FileNotFoundError, json.JSONDecodeError):
                return None
            return config.get('read_positions', {}).get('openbubbles|bob@example.invalid')

        self.wait_for(lambda: bob_position() is not None, timeout=10)
        self.wait_for(lambda: (bob_position() or {}).get('created_at') == newest_at,
                      timeout=10)
        self.assertIn(
            bob_position().get('value'), newest_ids,
            f'opening Bob must mark its newest row read: {bob_position()!r}')
        self.wait_for(lambda: 'Bob · 3' not in self.text())
        self.artifact('unread-badge-cleared-after-open')

        # More than a viewport of unread rows makes first-unread navigation
        # distinguishable from clamping to latest. Ignore sidebar previews.
        self.key(b'\x1b[5~')
        for index in range(40):
            self.append_fixture_message(BOB, f'UNREAD2_MARKER_{index:03d} synthetic unread')
        self.wait_for(lambda: 'UNREAD2_MARKER_039' in self.text(), timeout=15)
        self.key(b'/unread\r')

        def unread_transcript_markers():
            divider = self.screen.display[0].find('│')
            left = divider + 3 if divider >= 0 else 2
            return re.findall(r'UNREAD2_MARKER_\d+', '\n'.join(
                line[left:] for line in self.screen.display[2:-5]))

        self.wait_for(lambda: unread_transcript_markers()[:1] == ['UNREAD2_MARKER_000'])
        self.assertNotIn('UNREAD2_MARKER_039', unread_transcript_markers())
        self.artifact('unread-jump-first-unread')

    def test_arrivals_reconciliation_latest_and_literal_draft(self):
        body = '/new\nPrivate literal Alice draft'
        self.key(b'\x1b[200~' + body.encode() + b'\x1b[201~')
        self.key(b'\x1b[5~')
        self.assertTrue(self.markers())
        anchor = self.markers()[0]
        self.artifact('reading-before')
        for index in range(60, 63):
            self.send_message(index)
        self.pump(.6)
        self.artifact('reading-after-arrivals')
        self.assertEqual(self.markers()[0], anchor, 'live arrivals must not abandon the reading anchor')
        missed = 'READ_MARKER_063 stored without a live event'
        R['request'](str(self.fixture.socket_path), 'fixture_append_message',
                     conversation_id=CID, text=missed)
        self.pump(.5)
        self.assertNotIn('READ_MARKER_063', self.text(),
                         'fixture message must not arrive through live fanout')
        self.wait_for(lambda: 'READ_MARKER_063' in self.text(), timeout=15)
        self.assertIn('READ_MARKER_063', self.text(),
                      'periodic stored-history reconciliation must fetch the missed message')
        self.assertEqual(self.markers()[0], anchor, 'stored-history reconciliation must preserve reading')
        self.assertIn('Latest', self.text())
        self.key(b'\x1b[F')
        self.assertNotIn('↓ Latest', self.text())
        self.assertIn('READ_MARKER_063', self.markers(),
                      'End must render the reconciled message in the transcript')
        self.key(b'\x1b[5~')
        row, line = next((row, line) for row, line in enumerate(self.screen.display) if '↓ Latest' in line)
        column = line.index('↓ Latest')
        self.key(f'\x1b[<0;{column + 1};{row + 1}M'.encode())
        self.assertNotIn('↓ Latest', self.text())
        self.assertIn('READ_MARKER_063', self.markers(),
                      'mouse Latest must render the reconciled message in the transcript')
        self.artifact('latest-after-click')
        self.assertEqual(len(self.fixture.persisted()['messages']), 64)
        self.key(b'\r')
        messages = self.fixture.persisted()['messages']
        self.assertEqual(len(messages), 65)
        self.assertEqual(messages[-1]['conversation_id'], CID)
        self.assertEqual(messages[-1]['parts'], [{'type': 'text', 'value': body}])

    def test_older_history_pages_from_the_top_without_moving_the_reader(self):
        # Rebuild the thread with enough older history that the newest 200-message
        # page hides the oldest 80 messages, so one page load is a short page.
        self.stop_tui()
        self.fixture.stop_daemon()
        state = self.fixture.persisted()
        chat = next(item for item in state['conversations'] if item['id'] == CID)
        sender = next(participant['handle'] for participant in chat['participants']
                      if not participant['is_me'])
        baseline = min(item['created_at'] for item in state['messages'])
        seeded = []
        for index in range(220):
            seeded.append({
                'id': {'transport': 'openbubbles', 'value': f'older-{index:03d}'},
                'conversation_id': CID,
                'remote_id': None,
                'sender': sender,
                'parts': [{'type': 'text',
                           'value': f'OLDER_MARKER_{index:03d} synthetic context'}],
                'created_at': baseline - (220 - index) * 10,
                'sent_at': None,
                'delivery': 'delivered',
                'from_me': False,
                'edited_at': None,
                'reactions': [],
                'unsent_at': None,
            })
        state['messages'] = seeded + state['messages']
        self.fixture.store_path.write_text(json.dumps(state))
        self.fixture.start_daemon()
        self.start_tui()
        self.resize(100, 30)
        self.wait_for(lambda: 'READ_MARKER_059' in self.text())
        self.assertNotIn('OLDER_MARKER_079', self.text(),
                         'the newest page must stop before the oldest stored messages')

        # Reaching the oldest loaded row asks for the older page directly.
        anchor = 'OLDER_MARKER_080'
        for _ in range(80):
            self.key(b'\x1b[5~')
            self.pump(.05)
            if self.markers()[:1] == [anchor]:
                break
        self.assertEqual(self.markers()[:1], [anchor],
                         'the newest page ends at its oldest stored message')

        # No further input: the arriving page must not move what is being read.
        # Wait for the request to settle rather than racing a fixed delay.
        self.wait_for(lambda: 'Loading older messages' not in self.text(), timeout=10)
        self.pump(.3)  # Let the settled frame flush before reading the top row.
        self.assertEqual(self.markers()[0], anchor,
                         'loading older history must not move the reader')
        self.artifact('older-anchored')

        # The fetched messages are now reachable above the anchor.
        for _ in range(40):
            if 'OLDER_MARKER_000' in self.markers():
                break
            self.key(b'\x1b[5~')
            self.pump(.05)
        self.assertIn('OLDER_MARKER_000', self.markers(),
                      'the loaded page contributes the previously hidden history')
        self.artifact('older-loaded')

    def test_settings_wheel_cannot_scroll_hidden_history(self):
        before = self.markers()
        self.key(b',')
        self.assertIn('Settings', self.text())
        self.artifact('settings-before-wheel')
        self.key(b'\x1b[<64;65;23M')
        self.key(b'\x1b')
        self.artifact('closed-after-wheel')
        self.assertNotIn('reading back', self.text().lower())
        self.assertNotIn('↓ Latest', self.text())
        self.assertEqual(self.markers(), before)

    def test_narrow_navigation_cancel_click_and_recipient_owned_literal_draft(self):
        self.resize(44, 12)
        self.wait_for(lambda: 'Alice' in self.text())
        self.assertNotIn('│', self.screen.display[0], 'narrow chat should own the full width')
        body = '/new\nPrivate Alice draft'
        self.key(b'\x1b[200~' + body.encode() + b'\x1b[201~')
        self.key(b'\t')
        self.assertIn('Find:', self.text())
        self.key(b'\x1b[200~Bob\n\x1b[201~')
        self.assertIn('Bob', self.text())
        self.artifact('narrow-chats-query')
        self.key(b'\x1b')
        self.assertIn('Alice', self.screen.display[0])
        self.assertIn('Private Alice draft', self.text())
        self.key(b'\t')
        self.key(b'Bob')
        row, line = next((row, line) for row, line in enumerate(self.screen.display) if '› Bob' in line)
        self.key(f'\x1b[<0;{line.index("› Bob") + 1};{row + 1}M'.encode())
        self.assertIn('Bob', self.screen.display[0])
        self.assertNotIn('Private Alice draft', self.text())
        self.key(b'\r')
        self.assertEqual(len(self.fixture.persisted()['messages']), 60)
        self.key(b'\t')
        self.key(b'Alice')
        self.resize(100, 30)
        self.wait_for(lambda: 'Find:' in self.text())
        self.key(b'\r')
        self.assertIn('Alice', self.screen.display[0])
        self.assertIn('Private Alice draft', self.text())
        self.key(b'\r')
        messages = self.fixture.persisted()['messages']
        self.assertEqual(len(messages), 61)
        self.assertEqual(messages[-1]['conversation_id'], CID)
        self.assertEqual(messages[-1]['parts'], [{'type': 'text', 'value': body}])

    def test_drawer_candidate_survives_identity_alias_on_reconnect(self):
        self.resize(44, 12)
        self.wait_for(lambda: 'Alice' in self.text())
        self.key(b'\t')
        self.key(b'\x1b[B')
        self.assertIn('› Bob', self.text())
        self.artifact('candidate-before-reconnect')
        for suffix in ('canonical', 'final'):
            self.fixture.stop_daemon()
            state = self.fixture.persisted()
            bob = next(chat for chat in state['conversations'] if chat['title'].startswith('Bob'))
            bob['aliases'] = [bob['id']]
            bob['id'] = {'transport': 'openbubbles', 'value': f'bob-{suffix}@example.invalid'}
            bob['title'] = f'Bob {suffix} identity'
            self.fixture.store_path.write_text(json.dumps(state))
            self.fixture.start_daemon()
            self.wait_for(lambda: bob['title'] in self.text(), timeout=10)
            self.artifact(f'candidate-after-{suffix}-reconnect')
            self.assertIn(f'› {bob["title"]}', self.text(),
                          'successive identity reconciliation must preserve the logical candidate')
        self.key(b'\r')
        self.assertIn('Bob final identity', self.screen.display[0])
        self.assertEqual(len(self.fixture.persisted()['messages']), 60)

    def test_narrow_resize_preserves_overlay_owner(self):
        self.key(b'\t')
        self.key(b'?')
        self.assertIn('Keyboard shortcuts', self.text())
        self.resize(44, 12)
        self.wait_for(lambda: 'Keyboard shortcuts' in self.text())
        self.assertNotIn('Find:', self.text(),
                         'resize must not replace the help overlay with Chats')

    def test_narrow_mouse_drawer_clears_pending_leader(self):
        self.resize(44, 12)
        self.wait_for(lambda: 'Alice' in self.text())
        self.key(b'\x02')
        row, line = next((row, line) for row, line in enumerate(self.screen.display) if '‹ Chats' in line)
        self.key(f'\x1b[<0;{line.index("‹ Chats") + 1};{row + 1}M'.encode())
        self.assertIn('Find:', self.text())
        self.key(b'\x1b')
        self.key(b'x')
        self.assertTrue(any('›' in line and line.rstrip().endswith('x')
                            for line in self.screen.display),
                        'drawer opening must clear the pending Ctrl+B leader')

    def test_resize_retains_the_message_being_read(self):
        self.key(b'\x1b[5~')
        anchor = self.markers()[0]
        self.resize(66, 18)
        self.wait_for(lambda: bool(self.markers()))
        self.assertEqual(self.markers()[0], anchor)
        self.resize(100, 30)
        self.wait_for(lambda: bool(self.markers()))
        self.assertEqual(self.markers()[0], anchor)
        self.artifact('restored-size')



    def test_search_finds_and_jumps_to_paged_history_and_esc_restores_draft(self):
        # Seed an older page whose oldest message has a marker no other message
        # contains, so a hit can only resolve by paging back through the store.
        self.stop_tui()
        self.fixture.stop_daemon()
        state = self.fixture.persisted()
        chat = next(item for item in state['conversations'] if item['id'] == CID)
        sender = next(participant['handle'] for participant in chat['participants']
                      if not participant['is_me'])
        baseline = min(item['created_at'] for item in state['messages'])
        marker = 'OLDER_MARKER_999'
        seeded = []
        for index in range(220):
            text = (f'{marker} only in the oldest stored message' if index == 0
                    else f'OLDER_SEARCH_CONTEXT_{index:03d} synthetic context')
            seeded.append({
                'id': {'transport': 'openbubbles', 'value': f'search-older-{index:03d}'},
                'conversation_id': CID,
                'remote_id': None,
                'sender': sender,
                'parts': [{'type': 'text', 'value': text}],
                'created_at': baseline - (220 - index) * 10,
                'sent_at': None,
                'delivery': 'delivered',
                'from_me': False,
                'edited_at': None,
                'reactions': [],
                'unsent_at': None,
            })
        state['messages'] = seeded + state['messages']
        self.fixture.store_path.write_text(json.dumps(state))
        self.fixture.start_daemon()
        self.start_tui()
        self.resize(100, 30)
        self.wait_for(lambda: 'READ_MARKER_059' in self.text())
        self.assertNotIn(marker, self.text(),
                         'the oldest stored message must start outside the loaded history')

        draft = 'Private Alice search draft'
        self.key(b'\x1b[200~' + draft.encode() + b'\x1b[201~')
        self.key(b'\x06')  # Ctrl+F
        self.wait_for(lambda: 'Search:' in self.text())
        self.key(b'\x1b')
        self.wait_for(lambda: draft in self.text())
        self.assertIn(draft, self.text(), 'Esc must restore the parked composer draft')

        self.key(b'\x06')
        self.wait_for(lambda: 'Search:' in self.text())
        self.key(marker.encode())
        self.key(b'\r')
        # The query line and the hit snippet both name the marker.
        self.wait_for(lambda: self.text().count(marker) >= 2, timeout=10)
        self.artifact('search-results')
        self.key(b'\r')
        self.wait_for(lambda: marker in self.markers(), timeout=15)
        self.assertEqual(self.markers()[0], marker,
                         'the jump must page back and land on the matched message')
        self.assertIn(draft, self.text(), 'closing search restores the draft')
        self.artifact('search-jumped')

if __name__ == '__main__':
    unittest.main(verbosity=2)
