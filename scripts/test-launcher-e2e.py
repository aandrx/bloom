#!/usr/bin/env python3
"""Exercise the actual installed-entry binary, with isolated synthetic children."""
import os
from pathlib import Path
import shutil
import pty
import termios
import signal
import socket
import subprocess
import tempfile
import time
import unittest

BINARY = Path(__file__).resolve().parents[1] / 'target/debug/bloom'


class LauncherFlows(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='bloom-launcher-')
        self.root = Path(self.temp.name)
        self.root.chmod(0o700)
        shutil.copy2(BINARY, self.root / 'bloom')
        self.env = dict(os.environ, HOME=str(self.root),
                        XDG_STATE_HOME=str(self.root / 'state'),
                        BLOOM_SOCKET_PATH=str(self.root / 'daemon.sock'))
        self.children = []
        self.stub('bloom-tui', "import time\nfrom pathlib import Path\nPath('tui.pid').write_text(str(os.getpid()))\ntime.sleep(60)")

    def tearDown(self):
        # Always reap the launcher, even if an assertion fails. Fixture PID files
        # cover descendants in the deliberately broken pre-fix case.
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.wait()
            if child.stdout:
                child.stdout.close()
            if child.stderr:
                child.stderr.close()
        for name in ['daemon.pid', 'tui.pid']:
            path = self.root / name
            if path.exists():
                try:
                    pid = int(path.read_text())
                    command = Path(f'/proc/{pid}/cmdline').read_bytes()
                    if str(self.root).encode() in command:
                        os.kill(pid, signal.SIGKILL)
                except (ProcessLookupError, FileNotFoundError):
                    pass
        self.temp.cleanup()

    def stub(self, name, code):
        path = self.root / name
        path.write_text('#!/usr/bin/env python3\nimport os\n' + code + '\n')
        path.chmod(0o700)

    def daemon(self, startup_delay=0, drain=0):
        self.stub('bloomd', f'''import signal, socket, time
from pathlib import Path
Path('daemon.pid').write_text(str(os.getpid()))
def stop(signum, frame):
    time.sleep({drain})
    Path('drained').write_text('yes')
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
time.sleep({startup_delay})
server = socket.socket(socket.AF_UNIX)
server.bind(os.environ['BLOOM_SOCKET_PATH'])
server.listen(8)
while True:
    client, _ = server.accept()
    client.close()
''')

    def launch(self):
        child = subprocess.Popen([str(self.root / 'bloom')], cwd=self.root,
                                 env=self.env, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.children.append(child)
        return child

    def wait_file(self, name):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            path = self.root / name
            if path.exists() and path.stat().st_size:
                return path
            time.sleep(.02)
        self.fail(f'{name} was not created')

    def assert_gone(self, name):
        pid = int((self.root / name).read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_termination_during_startup_reaps_owned_daemon(self):
        self.daemon(startup_delay=60)
        child = self.launch()
        self.wait_file('daemon.pid')
        child.send_signal(signal.SIGTERM)
        self.assertEqual(child.wait(timeout=5), 143)
        self.assert_gone('daemon.pid')
        self.assertFalse((self.root / 'tui.pid').exists())

    def test_termination_with_active_tui_reaps_both_children(self):
        self.daemon()
        child = self.launch()
        self.wait_file('tui.pid')
        child.send_signal(signal.SIGTERM)
        self.assertEqual(child.wait(timeout=5), 143)
        self.assert_gone('daemon.pid')
        self.assert_gone('tui.pid')

    def test_reused_listener_is_not_terminated(self):
        listener = socket.socket(socket.AF_UNIX)
        self.addCleanup(listener.close)
        listener.bind(self.env['BLOOM_SOCKET_PATH'])
        listener.listen(8)
        child = self.launch()
        self.wait_file('tui.pid')
        child.send_signal(signal.SIGTERM)
        self.assertEqual(child.wait(timeout=5), 143)
        self.assert_gone('tui.pid')
        self.assertFalse((self.root / 'daemon.pid').exists())
        connection = socket.socket(socket.AF_UNIX)
        self.addCleanup(connection.close)
        connection.connect(self.env['BLOOM_SOCKET_PATH'])

    def test_terminated_raw_tui_restores_original_terminal_modes(self):
        self.daemon()
        self.stub('bloom-tui', "import tty, time\nfrom pathlib import Path\ntty.setraw(0)\nPath('tui.pid').write_text(str(os.getpid()))\ntime.sleep(60)")
        master, slave = pty.openpty()
        try:
            original = termios.tcgetattr(slave)
            child = subprocess.Popen([str(self.root / 'bloom')], cwd=self.root,
                                     env=self.env, stdin=slave, stdout=slave,
                                     stderr=subprocess.PIPE)
            self.children.append(child)
            self.wait_file('tui.pid')
            self.assertNotEqual(termios.tcgetattr(slave), original)
            child.send_signal(signal.SIGTERM)
            self.assertEqual(child.wait(timeout=5), 143)
            self.assertEqual(termios.tcgetattr(slave), original)
            self.assert_gone('tui.pid')
            self.assert_gone('daemon.pid')
        finally:
            os.close(master)
            os.close(slave)

    def test_daemon_gets_more_than_ten_seconds_to_drain(self):
        self.daemon(drain=11)
        self.stub('bloom-tui', 'raise SystemExit(0)')
        child = self.launch()
        self.assertEqual(child.wait(timeout=18), 0)
        self.assertEqual((self.root / 'drained').read_text(), 'yes')
        self.assert_gone('daemon.pid')

    def test_log_is_private_and_tui_exec_failure_reaps_daemon(self):
        self.daemon()
        (self.root / 'bloom-tui').write_text('#!/does/not/exist\n')
        child = self.launch()
        self.assertEqual(child.wait(timeout=5), 1)
        self.assert_gone('daemon.pid')
        log = self.root / 'state/bloom/bloomd.log'
        self.assertEqual(log.stat().st_mode & 0o777, 0o600)
        self.assertEqual(log.parent.stat().st_mode & 0o777, 0o700)

    def test_symlink_log_directory_refused_without_chmod_target(self):
        self.daemon()
        target = self.root / 'target'
        target.mkdir(mode=0o755)
        original = target.stat().st_mode
        (self.root / 'state').mkdir()
        (self.root / 'state/bloom').symlink_to(target, target_is_directory=True)
        child = self.launch()
        self.assertEqual(child.wait(timeout=5), 1)
        self.assertEqual(target.stat().st_mode, original)
        self.assertFalse((self.root / 'daemon.pid').exists())

    def test_symlink_log_file_refused(self):
        self.daemon()
        parent = self.root / 'state/bloom'
        parent.mkdir(parents=True, mode=0o700)
        target = self.root / 'target.log'
        target.write_text('unchanged')
        (parent / 'bloomd.log').symlink_to(target)
        child = self.launch()
        self.assertEqual(child.wait(timeout=5), 1)
        self.assertEqual(target.read_text(), 'unchanged')
        self.assertFalse((self.root / 'daemon.pid').exists())


if __name__ == '__main__':
    unittest.main(verbosity=2)
