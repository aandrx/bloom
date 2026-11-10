#!/usr/bin/env python3
"""Real-terminal input and recipient-safety regressions with synthetic IPC only.

Run with a Python interpreter containing pyte after building bloom-tui.
No engine, account or personal configuration is accessed.
"""
import fcntl
import json
import os
from pathlib import Path
import pty
import runpy
import select
import signal
import struct
import termios
import time
import unittest

HELPERS = runpy.run_path(str(Path(__file__).with_name('test-tui-messaging-e2e.py')), run_name='helpers')
STATE = runpy.run_path(str(Path(__file__).with_name('test-stateful-fake-daemon.py')), run_name='helpers')
SHOT = HELPERS['SHOT']


class InputFlows(unittest.TestCase):
    def setUp(self):
        self.fixture = HELPERS['MessagingFlows']()
        self.fixture.setUp()
        self.addCleanup(self.fixture.tearDown)
        self.fixture.start_daemon()

    def capture(self, keys):
        fixture = self.fixture
        with HELPERS['isolated_environment']({
            'BLOOM_SOCKET_PATH': str(fixture.socket_path),
            'XDG_CONFIG_HOME': str(fixture.root / 'config'),
            'XDG_STATE_HOME': str(fixture.root / 'state'),
            'XDG_DATA_HOME': str(fixture.root / 'data'),
        }):
            raw = SHOT['run_tui']([str(HELPERS['BINARY'])], 100, 30, keys, 1.0)
        text = '\n'.join(SHOT['render'](raw, 100, 30).display)
        artifacts = os.environ.get('BLOOM_E2E_ARTIFACTS')
        if artifacts:
            dest = Path(artifacts) / self._testMethodName
            dest.mkdir(parents=True, exist_ok=True)
            (dest / 'screen.txt').write_text(text + '\n')
            (dest / 'screen.ansi').write_bytes(raw)
        return text

    def test_paste_is_visible_without_sending(self):
        text = self.capture([b'\x1b[200~First pasted line\r\nSecond pasted line\x1b[201~'])
        self.assertIn('First pasted line', text)
        self.assertIn('Second pasted line', text)
        self.assertEqual(self.fixture.persisted()['messages'], [])

    def test_pasted_commands_send_as_exact_literal_body_only_on_enter(self):
        body = '  /new\ncarol@example.invalid\nDO NOT EXECUTE  '
        self.capture([b'\x1b[200~' + body.encode() + b'\x1b[201~', b'\r'])
        messages = self.fixture.persisted()['messages']
        self.assertEqual(len(messages), 1)
        self.assertEqual(messages[0]['conversation_id']['value'], 'alice@example.invalid')
        self.assertEqual(messages[0]['parts'], [{'type': 'text', 'value': body}])
        self.assertEqual(len(self.fixture.persisted()['conversations']), 2)

    def test_ctrl_z_undoes_the_typed_run_without_touching_the_transcript(self):
        baseline = self.capture([])
        text = self.capture([b'Undo this draft', b'\x1a'])
        self.assertNotIn('Undo this draft', text, 'Ctrl+Z must empty the composer')
        self.assertEqual(text, baseline, 'undo must not redraw or alter the transcript')
        self.assertEqual(self.fixture.persisted()['messages'], [], 'undo must never send')

    def test_ctrl_j_is_newline_not_send(self):
        self.capture([b'First line', b'\n', b'Second line', b'\r'])
        messages = self.fixture.persisted()['messages']
        self.assertEqual(len(messages), 1)
        self.assertEqual(messages[0]['parts'], [{'type': 'text', 'value': 'First line\nSecond line'}])

    def test_switching_chats_cannot_send_previous_recipients_draft(self):
        text = self.capture([b'Private Alice message', b'\t', b'\x1b[B', b'\r', b'\r'])
        self.assertIn('Bob', text.splitlines()[0])
        self.assertEqual(self.fixture.persisted()['messages'], [], 'Alice draft must not be sendable to Bob')

    def test_returning_to_chat_restores_its_own_draft(self):
        self.capture([b'Private Alice message', b'\t', b'\x1b[B', b'\r', b'Bob draft', b'\t', b'\x1b[A', b'\r', b'\r'])
        messages = self.fixture.persisted()['messages']
        self.assertEqual(len(messages), 1)
        self.assertEqual(messages[0]['conversation_id']['value'], 'alice@example.invalid')
        self.assertEqual(messages[0]['parts'], [{'type': 'text', 'value': 'Private Alice message'}])

    def test_new_chat_does_not_destroy_existing_draft(self):
        text = self.capture([b'Keep Alice draft', b'\x02', b'n', b'/new', b'\r', b'carol@example.invalid', b'\r', b'Carol draft', b'\x1b', b'\x10'])
        self.assertEqual(self.fixture.persisted()['messages'], [])
        self.assertIn('Keep Alice draft', text)

    def test_settings_preserves_chat_composition_and_new_chat_body(self):
        text = self.capture([b'Alice draft', b'\x02', b's', b'\x1b'])
        self.assertIn('Alice draft', text)
        text = self.capture([b'/new', b'\r', b'carol@example.invalid', b'\r', b'Carol body', b'\x02', b's', b'\x1b'])
        self.assertIn('Carol body', text)
        self.assertIn('carol@example.invalid', text)

    def test_modal_draft_does_not_follow_changed_selected_chat(self):
        text = self.capture([b'Alice private', b'\x02', b's', b'\t', b'\x1b[B', b'\r', b'\x1b', b'\r'])
        self.assertIn('Bob', text.splitlines()[0])
        self.assertNotIn('Alice private', text, 'modal-suspended Alice text must not appear in Bob')
        self.assertEqual(self.fixture.persisted()['messages'], [], 'closing a modal in Bob must not make Alice text sendable')

    def test_nickname_command_is_local_and_persists_without_sending(self):
        text = self.capture([b'/nickname Friendly Alice', b'\r'])
        self.assertIn('Friendly Alice', text)
        config = json.loads((self.fixture.root / 'config/bloom/tui.json').read_text())
        self.assertIn('Friendly Alice', config['nicknames'].values())
        self.assertEqual(self.fixture.persisted()['messages'], [])
        self.capture([b'/nickname clear', b'\r'])
        config = json.loads((self.fixture.root / 'config/bloom/tui.json').read_text())
        self.assertNotIn('Friendly Alice', config['nicknames'].values())

    def test_manual_sync_header_tracks_request_and_preserves_failure_priority(self):
        self.fixture.stop_daemon()
        self.fixture.start_daemon('--sync-delay', '8')
        text = self.capture([b'/sync', b'\r'])
        self.assertIn('Syncing', '\n'.join(text.splitlines()[:2]))
        self.fixture.stop_daemon()
        self.fixture.start_daemon('--sync-delay', '8', '--session-state', 'failed')
        text = self.capture([b'/sync', b'\r'])
        header = '\n'.join(text.splitlines()[:2])
        self.assertIn('Failed', header)
        self.assertNotIn('Syncing', header)

    def test_resize_redraws_without_a_followup_key(self):
        fixture = self.fixture
        with HELPERS['isolated_environment']({
            'BLOOM_SOCKET_PATH': str(fixture.socket_path),
            'XDG_CONFIG_HOME': str(fixture.root / 'config'),
            'XDG_STATE_HOME': str(fixture.root / 'state'),
        }):
            pid, master = pty.fork()
            if pid == 0:
                os.execv(str(HELPERS['BINARY']), [str(HELPERS['BINARY'])])
        raw = bytearray()
        def pump(seconds):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], .03)[0]:
                    raw.extend(os.read(master, 65536))
        try:
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 100, 0, 0))
            pump(1.5)
            if 'Open Bloom' not in '\n'.join(SHOT['render'](bytes(raw), 100, 30).display):
                pump(2.0)
            os.write(master, b'\r')
            pump(.5)
            raw.clear()
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 18, 60, 0, 0))
            os.kill(pid, signal.SIGWINCH)
            pump(.6)
            screen = SHOT['render'](bytes(raw), 60, 18)
            text = '\n'.join(screen.display)
            self.assertIn('Alice', text, 'Idle resize must produce a new frame before five-second reconciliation')
            self.assertIn('›', text)
        finally:
            os.kill(pid, signal.SIGTERM)
            os.waitpid(pid, 0)
            os.close(master)

    def test_startup_double_enter_cannot_send_a_restored_draft(self):
        draft = {
            'body': 'RepeatRaceDraft',
            'cursor': 0,
            'pasted_body': False,
            'uncertain': False,
        }
        STATE['request'](
            str(self.fixture.socket_path), 'draft_save',
            key={'kind': 'conversation', 'owner': {
                'transport': 'openbubbles', 'value': 'alice@example.invalid'}},
            draft=draft, revision=1,
        )
        with HELPERS['isolated_environment']({
            'BLOOM_SOCKET_PATH': str(self.fixture.socket_path),
            'XDG_CONFIG_HOME': str(self.fixture.root / 'config'),
            'XDG_STATE_HOME': str(self.fixture.root / 'state'),
            'XDG_DATA_HOME': str(self.fixture.root / 'data'),
            'NO_COLOR': '1',
        }):
            raw = SHOT['run_tui'](
                [str(HELPERS['BINARY'])], 100, 30, [b'\r', b'\r'], 1.5,
            )
        text = '\n'.join(SHOT['render'](raw, 100, 30).display)
        self.assertIn('RepeatRaceDraft', text, 'the restored draft must remain visible')
        self.assertEqual(self.fixture.persisted()['messages'], [],
                         'a startup Enter burst must not send the restored draft')

    def test_startup_checklist_blocks_burst_input_until_explicit_confirmation(self):
        fixture = self.fixture
        environment = {
            'BLOOM_SOCKET_PATH': str(fixture.socket_path),
            'BLOOM_CONFIG_HOME': str(fixture.root / 'config'),
            'XDG_CONFIG_HOME': str(fixture.root / 'config'),
            'XDG_STATE_HOME': str(fixture.root / 'state'),
            'XDG_DATA_HOME': str(fixture.root / 'data'),
            'NO_COLOR': '1',
        }
        with HELPERS['isolated_environment'](environment):
            checklist = SHOT['run_tui'](
                [str(HELPERS['BINARY'])], 100, 30, [], 1.5, False,
            )
            opened = SHOT['run_tui'](
                [str(HELPERS['BINARY'])], 100, 30, [b'burst', b'\r'], 1.5, False,
            )
        checklist_text = '\n'.join(SHOT['render'](checklist, 100, 30).display)
        opened_text = '\n'.join(SHOT['render'](opened, 100, 30).display)
        self.assertIn('Open Bloom', checklist_text)
        self.assertIn('5/5 checks', checklist_text)
        self.assertNotIn('Write a message', checklist_text)
        self.assertEqual(self.fixture.persisted()['messages'], [])
        self.assertIn('Alice', opened_text)
        self.assertEqual(self.fixture.persisted()['messages'], [], 'loading input must not send')

    def test_failed_settings_save_is_painted_without_another_key(self):
        fixture = self.fixture
        config_root = fixture.root / 'config'
        config_root.mkdir(parents=True)
        os.chmod(config_root, 0o700)
        tui_json = config_root / 'bloom' / 'tui.json'

        raw = bytearray()
        environment = {
            'BLOOM_SOCKET_PATH': str(fixture.socket_path),
            'XDG_CONFIG_HOME': str(config_root),
            'XDG_STATE_HOME': str(fixture.root / 'state'),
            'XDG_DATA_HOME': str(fixture.root / 'data'),
            'NO_COLOR': '1',
        }
        with HELPERS['isolated_environment'](environment):
            pid, master = pty.fork()
            if pid == 0:
                os.execv(str(HELPERS['BINARY']), [str(HELPERS['BINARY'])])

        def current_text():
            screen = SHOT['render'](bytes(raw), 100, 30)
            return '\n'.join(screen.display)

        def pump_until(predicate, timeout):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if select.select([master], [], [], .05)[0]:
                    try:
                        raw.extend(os.read(master, 65536))
                    except OSError:
                        break
                if predicate(current_text()):
                    return True
            return False

        try:
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 100, 0, 0))
            self.assertTrue(
                pump_until(lambda text: 'Open Bloom' in text, 15),
                'TUI did not offer Open Bloom: ' + current_text(),
            )
            os.write(master, b'\r')
            self.assertTrue(
                pump_until(lambda text: 'Alice' in text, 5),
                'TUI did not draw the normal transcript: ' + current_text(),
            )

            # Healthy baseline: the default composer focus accepts the Ctrl+B
            # leader, and `t` toggles the persisted theme without typing.
            if not pump_until(lambda text: tui_json.exists(), 2):
                os.write(master, b'\x02t')
            self.assertTrue(
                pump_until(lambda text: tui_json.exists(), 10),
                'Writable config root never produced tui.json: ' + current_text(),
            )
            self.assertNotIn('Could not save local settings', current_text())

            # Renaming the temp file onto a directory fails, so the next save
            # cannot succeed even though the directory itself is writable.
            os.remove(tui_json)
            os.mkdir(tui_json)
            self.addCleanup(os.rmdir, tui_json)

            os.write(master, b'\x02t')
            # Bound the user-visible latency well below the five-second
            # reconciliation poll. This proves the message paints promptly; it
            # does not isolate which queued frame request painted it.
            self.assertTrue(
                pump_until(lambda text: 'Could not save local settings' in text, 1.5),
                'Failed save was not painted without another key: ' + current_text(),
            )
            failing_frame = bytes(raw)
            failing_text = '\n'.join(SHOT['render'](failing_frame, 100, 30).display)
            self.assertIn('Could not save local settings', failing_text)
            self.assertTrue(tui_json.is_dir(), 'Failed save replaced the blocking directory')

            artifacts = os.environ.get('BLOOM_E2E_ARTIFACTS')
            if artifacts:
                dest = Path(artifacts) / self._testMethodName
                dest.mkdir(parents=True, exist_ok=True)
                (dest / 'screen.txt').write_text(failing_text + '\n')
                (dest / 'screen.ansi').write_bytes(failing_frame)

            os.write(master, b'?')
            self.assertTrue(
                pump_until(lambda text: 'this sheet' in text, 5),
                'Loop did not respond to a key after the failed save: ' + current_text(),
            )
            os.write(master, b'\x1b')
        finally:
            try:
                os.kill(pid, signal.SIGTERM)
            except OSError:
                pass
            os.waitpid(pid, 0)
            os.close(master)


if __name__ == '__main__':
    unittest.main(verbosity=2)
