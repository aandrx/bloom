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
        # A recorder instead of a desktop opener: the PTY test must prove the real
        # command path without launching a viewer on the test machine.
        self.opener_record = self.fixture.root / 'opened-path.txt'
        self.opener_script = self.fixture.root / 'opener.sh'
        self.opener_script.write_text(
            '#!/bin/sh\nprintf "%s" "$1" > "$BLOOM_OPENER_RECORD"\n',
            encoding='utf-8')
        self.opener_script.chmod(0o755)
        self.start_tui()
        self.addCleanup(self.stop_tui)
        self.resize(100, 30)
        self.wait_for(lambda: 'READ_MARKER_059' in self.text())

    def start_tui(self, opener=None):
        with H['isolated_environment']({
            'BLOOM_SOCKET_PATH': str(self.fixture.socket_path),
            'XDG_CONFIG_HOME': str(self.fixture.root / 'config'),
            'XDG_STATE_HOME': str(self.fixture.root / 'state'),
            'XDG_DATA_HOME': str(self.fixture.root / 'data'),
            'NO_COLOR': '1',
            'BLOOM_OPENER': opener or str(self.opener_script),
            'BLOOM_OPENER_RECORD': str(self.opener_record),
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



    def test_participant_search_filters_before_the_hit_cap_and_preserves_draft(self):
        query = 'from:MEMBER_NEEDLE'  # Plain text, never sender-filter syntax.
        self.append_fixture_message(CID, query + ' ALICE_SCOPED_HIT')
        for index in range(61):
            self.append_fixture_message(BOB, f'{query} Bob scope hit {index:03d}')
        self.wait_for(lambda: 'Alice' in self.screen.display[0])
        before = len(self.fixture.persisted()['messages'])
        draft = '/new\nParticipant search draft for Alice'
        self.key(b'\x1b[200~' + draft.encode() + b'\x1b[201~')
        self.key(b'\x06')
        self.wait_for(lambda: 'Search:' in self.text())
        self.key(query.encode())
        self.key(b'\r')
        self.wait_for(lambda: '50 results' in self.text(), timeout=10)

        # Cancelling the inner picker preserves both the literal body query and
        # the completed unfiltered results. Pasted names never become addresses.
        self.key(b'\x10')
        self.wait_for(lambda: 'Find:' in self.text() and 'Chats with' in self.text())
        self.key(b'\x1b[200~Bob\n\x1b[201~')
        self.key(b'\x1b')
        self.wait_for(lambda: f'Search: {query}' in self.text())
        self.assertIn('50 results', self.text())

        def choose_alice(mouse=False):
            self.key(b'\x10')
            self.wait_for(lambda: 'Find:' in self.text())
            self.key(b'Alice')
            if mouse:
                row, line = next((row, line) for row, line in enumerate(self.screen.display)
                                 if 'email:alice@example.invalid' in line)
                column = line.index('alice@example.invalid') + 1
                self.key(f'\x1b[<0;{column};{row + 1}M'.encode())
            else:
                self.key(b'\x1b[B')
                self.key(b'\r')
            self.wait_for(lambda: f'Search: {query}' in self.text())
            self.key(b'\r')
            self.wait_for(lambda: re.search(r'\b1 results\b', self.text()) is not None, timeout=10)

        choose_alice()
        self.artifact('participant-scoped-results')
        # Explicit All chats restores the original global result cap without
        # editing the text query or committing a recipient change.
        self.key(b'\x10')
        self.wait_for(lambda: 'Find:' in self.text())
        self.key(b'\x1b[H')
        self.key(b'\r')
        self.wait_for(lambda: f'Search: {query}' in self.text())
        self.key(b'\r')
        self.wait_for(lambda: '50 results' in self.text(), timeout=10)
        choose_alice(mouse=True)
        self.assertEqual(len(self.fixture.persisted()['messages']), before,
                         'picker interaction must never send or change recipients')
        self.key(b'\r')

        def transcript_text():
            divider = self.screen.display[0].find('│')
            left = divider + 3 if divider >= 0 else 2
            return '\n'.join(line[left:] for line in self.screen.display[2:-5])

        self.wait_for(lambda: 'ALICE_SCOPED_HIT' in transcript_text())
        self.assertIn('Alice', self.screen.display[0])
        for line in draft.splitlines():
            self.assertIn(line, self.text())
        self.artifact('participant-result-opened-with-draft')
        self.key(b'\r')

        def sent_draft():
            return [message for message in self.fixture.persisted()['messages']
                    if message.get('from_me')
                    and message['parts'] == [{'type': 'text', 'value': draft}]]

        self.wait_for(lambda: bool(sent_draft()), timeout=10)
        self.assertEqual(len(sent_draft()), 1)
        self.assertEqual(sent_draft()[0]['conversation_id'], CID)

    def test_narrow_participant_picker_wraps_distinct_handles_before_mouse_selection(self):
        self.stop_tui()
        self.fixture.stop_daemon()
        state = self.fixture.persisted()
        values = ['long.name.one@example.invalid', 'long.name.two@example.invalid']
        for cid, value in zip([CID, BOB], values):
            chat = next(chat for chat in state['conversations'] if chat['id'] == cid)
            participant = next(person for person in chat['participants'] if not person['is_me'])
            participant['handle'] = {'kind': 'email', 'value': value}
            participant['display_name'] = 'Duplicate'
        latest = max(message['created_at'] for message in state['messages'])
        for index, cid in enumerate([CID, BOB]):
            message = json.loads(json.dumps(state['messages'][0]))
            message['id']['value'] = f'narrow-member-{index}'
            message['conversation_id'] = cid
            message['created_at'] = latest + index + 1
            message['parts'] = [{'type': 'text', 'value': f'NARROW_MEMBER_HIT {index}'}]
            state['messages'].append(message)
        self.fixture.store_path.write_text(json.dumps(state))
        self.fixture.start_daemon()
        self.start_tui()
        self.resize(24, 12)
        self.wait_for(lambda: 'Alice' in self.screen.display[0])
        self.key(b'\x06')
        self.wait_for(lambda: 'Search:' in self.text())
        self.key(b'\x10')
        self.wait_for(lambda: 'Chats with' in self.text() and 'Find:' in self.text())
        compact = ''.join(character for character in self.text() if not character.isspace())
        for value in values:
            self.assertIn(value, compact, 'narrow identities must wrap, not lose the distinguishing text')
        self.artifact('narrow-wrapped-identities')
        row, line = next((row, line) for row, line in enumerate(self.screen.display)
                         if 'openbubbles' in line)
        column = line.index('openbubbles') + 1
        self.key(f'\x1b[<0;{column};{row + 1}M'.encode())
        self.wait_for(lambda: 'Search:' in self.text() and 'Find:' not in self.text())
        self.key(b'NARROW_MEMBER_HIT\r')
        self.wait_for(lambda: re.search(r'\b1 results\b', self.text()) is not None, timeout=10)
        self.key(b'\r')
        self.wait_for(lambda: 'Search:' not in self.text())
        self.assertIn('Alice', self.screen.display[0], 'the wrapped row click must select the exact first handle')

    def test_search_opens_out_of_window_hit_and_esc_restores_draft(self):
        self.search_navigation_case(220, expected_context=True)

    def test_search_anchors_an_already_loaded_hit_without_context(self):
        self.search_navigation_case(20, expected_context=False)

    def test_deep_search_context_survives_arrivals_and_reconciliation_then_latest(self):
        self.search_navigation_case(1420, expected_context=True, check_latest=True)

    def search_navigation_case(self, older_count, expected_context, check_latest=False):
        # Only the oldest seeded message has the marker. The deep case exceeds
        # the normal retained budget; the small case is already in newest history.
        self.stop_tui()
        self.fixture.stop_daemon()
        state = self.fixture.persisted()
        chat = next(item for item in state['conversations'] if item['id'] == CID)
        sender = next(participant['handle'] for participant in chat['participants']
                      if not participant['is_me'])
        baseline = min(item['created_at'] for item in state['messages'])
        marker = 'OLDER_MARKER_999'
        seeded = []
        for index in range(older_count):
            text = (f'{marker} only in the oldest stored message' if index == 0
                    else f'OLDER_SEARCH_CONTEXT_{index:03d} synthetic context')
            seeded.append({
                'id': {'transport': 'openbubbles', 'value': f'search-older-{index:03d}'},
                'conversation_id': CID,
                'remote_id': None,
                'sender': sender,
                'parts': [{'type': 'text', 'value': text}],
                'created_at': baseline - (older_count - index) * 10,
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

        draft = ('/new\nPrivate Alice search draft' if check_latest
                 else 'Private Alice search draft')
        self.key(b'\x1b[200~' + draft.encode() + b'\x1b[201~')
        self.key(b'\x06')  # Ctrl+F
        self.wait_for(lambda: 'Search:' in self.text())
        self.key(b'\x1b')
        self.wait_for(lambda: all(line in self.text() for line in draft.splitlines()))
        for line in draft.splitlines():
            self.assertIn(line, self.text(), 'Esc must restore the parked composer draft')

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
                         'the jump must land on the exact matched message')
        for line in draft.splitlines():
            self.assertIn(line, self.text(), 'closing search restores the draft')
        self.assertEqual('Search context' in self.text(), expected_context)
        self.artifact('search-jumped')
        if not check_latest:
            return

        self.append_fixture_message(CID, 'READ_MARKER_999 synthetic newest arrival')
        self.wait_for(lambda: 'READ_MARKER_999' in self.text(), timeout=10)
        # The sidebar can update, but an arrival and the five-second local
        # reconciliation must not replace the context transcript.
        self.pump(5.6)
        self.assertEqual(self.markers()[0], marker)
        self.assertNotIn('READ_MARKER_999', self.markers())
        self.assertIn('Search context', self.text())
        self.artifact('context-after-reconciliation')

        self.key(b'\x1b[F')  # End is an explicit return to normal latest history.
        self.wait_for(lambda: 'READ_MARKER_999' in self.markers())
        # A post-paint read marker needs its own redraw, not another key or the
        # next five-second reconciliation before its unread badge disappears.
        self.wait_for(lambda: 'Alice · 1' not in self.text(), timeout=3)
        self.assertNotIn('Search context', self.text())
        self.assertIn('Alice', self.screen.display[0])
        for line in draft.splitlines():
            self.assertIn(line, self.text())
        self.artifact('context-returned-to-latest')
        self.key(b'\r')

        def restored_sends():
            return [message for message in self.fixture.persisted()['messages']
                    if message.get('from_me')
                    and message['parts'] == [{'type': 'text', 'value': draft}]]

        self.wait_for(lambda: bool(restored_sends()), timeout=10)
        self.assertEqual(len(restored_sends()), 1)
        self.assertEqual(restored_sends()[0]['conversation_id'], CID,
                         'Latest must preserve literal-paste provenance and recipient')

    def test_filename_hit_outside_window_opens_full_metadata_and_restores_alice_draft(self):
        filename = "FILE_ONLY_NEEDLE界'\u202e\u001b\u0085report.bin"
        escaped = "FILE_ONLY_NEEDLE界\\'\\u{202e}\\u{1b}\\u{85}report.bin"
        anchor = 'FILE_TARGET_CONTEXT anchored body has no query'

        # Rebuild Bob with 220 older rows. Only the oldest row carries the reported
        # filename, so it falls outside the newest 200 rows that a chat switch loads.
        self.stop_tui()
        self.fixture.stop_daemon()
        state = self.fixture.persisted()
        chat = next(item for item in state['conversations'] if item['id'] == BOB)
        sender = next(participant['handle'] for participant in chat['participants']
                      if not participant['is_me'])
        baseline = min(item['created_at'] for item in state['messages'])
        seeded = []
        for index in range(220):
            parts = [{'type': 'text',
                      'value': f'BOB_OLDER_CONTEXT_{index:03d} synthetic context'}]
            if index == 0:
                parts = [
                    {'type': 'text', 'value': anchor},
                    {'type': 'attachment', 'value': {
                        'id': 'synthetic-filename-only-target',
                        'mime_type': None,
                        'file_name': filename,
                        'byte_len': 0,
                    }},
                ]
            seeded.append({
                'id': {'transport': BOB['transport'],
                       'value': f'bob-filename-older-{index:03d}'},
                'conversation_id': dict(BOB),
                'remote_id': None,
                'sender': sender,
                'parts': parts,
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
        self.assertNotIn('FILE_ONLY_NEEDLE', self.text(),
                         'the filename hit must start outside the loaded newest rows')

        def transcript_text():
            divider = self.screen.display[0].find('│')
            left = divider + 3 if divider >= 0 else 2
            return '\n'.join(line[left:] for line in self.screen.display[2:-5])

        def projected():
            return re.sub(r'\s+', ' ', transcript_text())

        def compact():
            return re.sub(r'\s+', '', transcript_text())

        outgoing_before = {
            (message['id']['transport'], message['id']['value'])
            for message in state['messages'] if message.get('from_me')
        }
        draft = '/new\nPrivate Alice filename draft'
        self.key(b'\x1b[200~' + draft.encode() + b'\x1b[201~')
        for _ in range(5):
            self.key(b'\x1b[D')
        parked_cursor = (self.screen.cursor.x, self.screen.cursor.y)
        self.artifact('filename-parked-draft')

        self.key(b'\x06')
        self.wait_for(lambda: 'Search:' in self.text())
        self.key(b'FILE_ONLY_NEEDLE')
        self.key(b'\r')
        self.wait_for(lambda: re.search(r'\b1 results\b', self.text()) is not None,
                      timeout=10)
        self.assertIn('Filename: ' + escaped, self.text(),
                      'the single hit must show the escaped filename excerpt')
        self.artifact('filename-search-result')
        self.key(b'\r')
        self.wait_for(lambda: anchor in projected() and escaped in compact(), timeout=15)
        self.assertIn('Bob', self.screen.display[0])
        self.assertIn('Search context', self.text(), 'the old target must use exact context')
        self.assertNotIn('PrivateAlicefilenamedraft', re.sub(r'\s+', '', self.text()),
                         "Alice's parked body must not appear in Bob's composer")
        self.assertIn(anchor, projected())
        self.assertIn(escaped, compact(),
                      'the exact context must show the full escaped filename')
        self.assertNotIn('\u202e', transcript_text(), 'raw RLO must never reach the screen')
        self.artifact('filename-context-100')

        # Keep enough transcript height to inspect the entire wrapped metadata.
        # A 12-row terminal cannot show the anchor and long filename together.
        for width, height in ((44, 30), (24, 30)):
            self.resize(width, height)
            self.wait_for(lambda: anchor in projected() and escaped in compact(),
                          timeout=10)
            self.assertIn(anchor, projected())
            self.assertIn(escaped, compact())
            self.assertNotIn('\u202e', transcript_text())
            if width == 24:
                self.assertIn('↓ Latest', self.text(),
                              'the narrow search context must keep its Latest exit visible')
            self.artifact(f'filename-context-{width}')

        self.resize(100, 30)
        self.wait_for(lambda: anchor in projected() and escaped in compact(), timeout=10)
        self.assertIn(anchor, projected())
        self.assertIn(escaped, compact())
        self.assertNotIn('\u202e', transcript_text())
        self.assertIn('Search context', self.text(), 'resizing must retain the exact context')
        self.artifact('filename-context-restored-100')

        self.key(b'\x02p')  # Alice remains first; Bob is second in the stable chat order.
        self.wait_for(lambda: 'Alice' in self.screen.display[0])
        self.wait_for(lambda: all(line in self.text() for line in draft.splitlines()))
        self.wait_for(lambda: (self.screen.cursor.x, self.screen.cursor.y) == parked_cursor)
        self.assertEqual((self.screen.cursor.x, self.screen.cursor.y), parked_cursor)
        self.artifact('filename-alice-draft-restored')

        self.key(b'X')
        expected = draft[:-5] + 'X' + draft[-5:]
        self.wait_for(lambda: 'Private Alice filename Xdraft' in self.text())
        self.key(b'\r')

        def sent_rows():
            return [message for message in self.fixture.persisted()['messages']
                    if message.get('from_me')
                    and message['parts'] == [{'type': 'text', 'value': expected}]]

        self.wait_for(lambda: bool(sent_rows()), timeout=10)
        self.assertEqual(len(sent_rows()), 1)
        self.assertEqual(sent_rows()[0]['conversation_id'], CID,
                         'the restored cursor, literal draft and recipient must send to Alice')
        self.assertNotEqual(sent_rows()[0]['conversation_id'], BOB)
        self.assertEqual(sent_rows()[0]['parts'], [{'type': 'text', 'value': expected}])
        new_outgoing = [
            message for message in self.fixture.persisted()['messages']
            if message.get('from_me')
            and (message['id']['transport'], message['id']['value']) not in outgoing_before
        ]
        self.assertEqual(new_outgoing, sent_rows(),
                         'metadata navigation must not create any additional outbound message')
        self.artifact('filename-alice-sent')


    def test_reaction_summary_renders_at_three_widths(self):
        self.stop_tui()
        self.fixture.stop_daemon()
        state = self.fixture.persisted()
        chat = next(item for item in state['conversations'] if item['id'] == CID)
        sender = next(participant['handle'] for participant in chat['participants']
                      if not participant['is_me'])
        newest = max(item['created_at'] for item in state['messages'])
        reaction = lambda kind, part: {
            'sender': sender, 'kind': kind, 'remote_id': None,
            'part': part, 'timestamp': None,
        }
        state['messages'].append({
            'id': {'transport': 'openbubbles', 'value': 'reaction-pty-0001'},
            'conversation_id': CID,
            'remote_id': None,
            'sender': sender,
            'parts': [{'type': 'text', 'value': 'REACTION_PTY_MARKER synthetic body'}],
            'created_at': newest + 10,
            'sent_at': None,
            'delivery': 'delivered',
            'from_me': False,
            'edited_at': None,
            'reactions': [reaction('heart', 0), reaction('like', None),
                          reaction('heart', None)],
            'unsent_at': None,
        })
        self.fixture.store_path.write_text(json.dumps(state))
        self.fixture.start_daemon()
        self.start_tui()
        def transcript_text():
            divider = self.screen.display[0].find('│')
            left = divider + 3 if divider >= 0 else 2
            return '\n'.join(line[left:] for line in self.screen.display[2:-5])

        for width in (100, 44, 24):
            self.resize(width, 30)
            self.wait_for(lambda: 'REACTION_PTY_MARKER' in transcript_text(), timeout=15)
            compact = re.sub(r'\s+', '', transcript_text())
            self.assertIn('·heart2·like1', compact,
                          f'the sorted aggregate must render at {width} columns')
            self.artifact(f'reaction-summary-{width}')
        self.assertIn(
            '· heart 2 · like 1',
            re.sub(r'\s+', ' ', transcript_text()),
            'wide layout keeps spaced separators',
        )

    def test_edit_and_unsend_notes_render_with_retained_content(self):
        self.stop_tui()
        self.fixture.stop_daemon()
        state = self.fixture.persisted()
        chat = next(item for item in state['conversations'] if item['id'] == CID)
        sender = next(participant['handle'] for participant in chat['participants']
                      if not participant['is_me'])
        newest = max(item['created_at'] for item in state['messages'])

        def seeded(value, body, created_at, edited_at, unsent_at):
            return {
                'id': {'transport': 'openbubbles', 'value': value},
                'conversation_id': CID,
                'remote_id': None,
                'sender': sender,
                'parts': [{'type': 'text', 'value': body}],
                'created_at': created_at,
                'sent_at': None,
                'delivery': 'delivered',
                'from_me': False,
                'edited_at': edited_at,
                'reactions': [],
                'unsent_at': unsent_at,
            }

        state['messages'].append(seeded(
            'edit-pty-0001', 'EDITED_PTY_MARKER retained revised body',
            newest + 10, newest + 20, None))
        state['messages'].append(seeded(
            'unsent-pty-0001', 'UNSENT_PTY_MARKER retained audit body',
            newest + 11, None, newest + 21))
        self.fixture.store_path.write_text(json.dumps(state))
        self.fixture.start_daemon()
        self.start_tui()

        def transcript_text():
            divider = self.screen.display[0].find('│')
            left = divider + 3 if divider >= 0 else 2
            return '\n'.join(line[left:] for line in self.screen.display[2:-5])

        for width in (100, 44, 24):
            self.resize(width, 30)
            self.wait_for(
                lambda: 'EDITED_PTY_MARKER' in transcript_text()
                and 'UNSENT_PTY_MARKER' in transcript_text(),
                timeout=15,
            )
            compact = re.sub(r'\s+', '', transcript_text())
            self.assertIn('Edited', compact,
                          f'the edit note must render at {width} columns')
            self.assertIn('Unsent·retainedlocally', compact,
                          f'the unsent note must render at {width} columns')
            self.assertIn('retainedauditbody', compact,
                          f'unsettled content must stay visible at {width}')
            self.assertNotIn('deleted', compact.lower(),
                             f'unsend must not imply deletion at {width}')
            self.artifact(f'edit-unsend-notes-{width}')

    def seed_attachment(self, extra=None):
        self.stop_tui()
        self.fixture.stop_daemon()
        state = self.fixture.persisted()
        chat = next(item for item in state['conversations'] if item['id'] == CID)
        sender = next(participant['handle'] for participant in chat['participants']
                      if not participant['is_me'])
        newest = max(item['created_at'] for item in state['messages'])
        token = 'abcdef0123456789abcdef0123456789'
        state['messages'].append({
            'id': {'transport': 'openbubbles', 'value': 'attachment-pty-0001'},
            'conversation_id': CID,
            'remote_id': None,
            'sender': sender,
            'parts': [{'type': 'attachment', 'value': {
                'id': 'local-metadata:attachment-pty',
                'mime_type': 'image/jpeg',
                'file_name': 'photo.jpg',
                'byte_len': 42,
                'fetch_token': token,
            }}],
            'created_at': newest + 10,
            'sent_at': None,
            'delivery': 'delivered',
            'from_me': False,
            'edited_at': None,
            'reactions': [],
            'unsent_at': None,
        })
        if extra is not None:
            extra_token, extra_name = extra
            state['messages'].append({
                'id': {'transport': 'openbubbles', 'value': 'attachment-pty-0002'},
                'conversation_id': CID,
                'remote_id': None,
                'sender': sender,
                'parts': [{'type': 'attachment', 'value': {
                    'id': 'local-metadata:attachment-pty-extra',
                    'mime_type': 'application/pdf',
                    'file_name': extra_name,
                    'byte_len': 42,
                    'fetch_token': extra_token,
                }}],
                'created_at': newest + 20,
                'sent_at': None,
                'delivery': 'delivered',
                'from_me': False,
                'edited_at': None,
                'reactions': [],
                'unsent_at': None,
            })
        self.fixture.store_path.write_text(json.dumps(state))
        self.fixture.start_daemon()
        self.start_tui()
        self.resize(100, 30)
        self.wait_for(lambda: 'photo.jpg' in self.text())
        return token

    def test_attachment_command_saves_the_choice_from_the_picker(self):
        self.seed_attachment()
        self.key(b'/attachment')
        self.key(b'\r')
        self.wait_for(lambda: 'Enter save' in self.text())
        self.assertIn('photo.jpg', self.text(), 'the chooser lists its name')
        self.key(b'\r')
        self.wait_for(lambda: 'Attachment saved to' in self.text())
        self.assertIn('attachments/', self.text(),
                      'the saved cache location must be visible')
        # Settle first: a wrongly spawned opener could write its record after the
        # notice appears, so an immediate check would not be a real negative proof.
        self.pump(0.5)
        self.assertFalse(self.opener_record.exists(),
                         'saving must not launch an external opener')
        self.artifact('attachment-saved')

    def test_attachment_picker_selects_the_older_loaded_attachment(self):
        older = self.seed_attachment(
            extra=('fedcba9876543210fedcba9876543210', 'report.pdf'))
        self.key(b'/attachment')
        self.key(b'\r')
        self.wait_for(lambda: 'Enter save' in self.text())
        self.assertIn('report.pdf', self.text(), 'newest choice is listed')
        self.assertIn('photo.jpg', self.text(), 'older choice is listed')
        self.key(b'\x1b[B')  # Down
        self.resize(200, 30)
        self.key(b'\r')
        self.wait_for(lambda: 'Attachment saved to' in self.text())
        self.wait_for(lambda: older in self.text())
        self.assertIn(older, self.text(),
                      'the fetched path belongs to the chosen older attachment')
        self.assertNotIn('fedcba9876543210fedcba9876543210', self.text())
        self.artifact('attachment-picker-older')

    def test_open_command_fetches_then_launches_the_external_opener(self):
        token = self.seed_attachment()
        self.key(b'/open')
        self.key(b'\r')
        self.wait_for(lambda: 'Opened' in self.text())
        expected = str(self.fixture.root / 'attachments' /
                       f'bloom-fixture-{token}.bin')
        self.wait_for(lambda: self.opener_record.exists() and
                      self.opener_record.read_text(encoding='utf-8') == expected)
        self.assertEqual(self.opener_record.read_text(encoding='utf-8'), expected)
        self.assertIn('attachments/', self.text(),
                      'the opened cache location must be visible')
        self.artifact('attachment-opened')

    def test_open_command_reports_a_missing_opener(self):
        self.seed_attachment()
        self.stop_tui()
        self.start_tui(opener='/nonexistent/bloom-opener')
        self.resize(100, 30)
        self.wait_for(lambda: 'photo.jpg' in self.text())
        self.key(b'/open')
        self.key(b'\r')
        self.wait_for(lambda: 'No file opener is available' in self.text())
        self.assertFalse(self.opener_record.exists())
        self.artifact('attachment-open-missing-opener')



if __name__ == '__main__':
    unittest.main(verbosity=2)
