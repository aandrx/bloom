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

ROOT = Path(__file__).resolve().parents[1]
# Prefer the release launcher, which is the supported default; CI still builds debug.
BINARY = next(
    (path for path in (ROOT / 'target/release/bloom', ROOT / 'target/debug/bloom')
     if path.is_file()),
    ROOT / 'target/debug/bloom',
)


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


class CheckoutLauncherProfile(unittest.TestCase):
    """Run the checkout launcher against fake profile-specific binaries."""

    repo = Path(__file__).resolve().parents[1]

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='bloom-checkout-')
        self.root = Path(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        scripts = self.root / 'scripts'
        scripts.mkdir()
        for name in ('bloom', 'bloom-probe'):
            target = scripts / name
            shutil.copy2(self.repo / 'scripts' / name, target)
            target.chmod(0o755)
        (self.root / 'bin').mkdir()
        cargo = self.root / 'bin/cargo'
        cargo.write_text(
            '#!/usr/bin/env python3\n'
            'import sys\n'
            'from pathlib import Path\n'
            "Path('cargo.args').write_text(' '.join(sys.argv[1:]))\n"
        )
        cargo.chmod(0o755)
        self.env = dict(
            os.environ,
            HOME=str(self.root),
            PATH=f"{self.root / 'bin'}:{os.environ['PATH']}",
            XDG_STATE_HOME=str(self.root / 'state'),
            XDG_DATA_HOME=str(self.root / 'data'),
            BLOOM_SOCKET_PATH=str(self.root / 'daemon.sock'),
            CARGO_BUILD_JOBS='1',
        )
        self.addCleanup(self.kill_daemon)

    def stub(self, profile, name, body):
        path = self.root / 'target' / profile / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('#!/usr/bin/env python3\n' + body)
        path.chmod(0o755)

    def install_profile(self, profile):
        self.stub(profile, 'bloomd', (
            'import os, signal, socket\n'
            'from pathlib import Path\n'
            "Path('daemon.pid').write_text(str(os.getpid()))\n"
            'def stop(signum, frame):\n'
            '    raise SystemExit(0)\n'
            'signal.signal(signal.SIGTERM, stop)\n'
            "server = socket.socket(socket.AF_UNIX)\n"
            "server.bind(os.environ['BLOOM_SOCKET_PATH'])\n"
            'server.listen(8)\n'
            'while True:\n'
            '    client, _ = server.accept()\n'
            '    client.close()\n'
        ))
        self.stub(profile, 'bloom-tui', (
            'from pathlib import Path\n'
            f"Path('tui.marker').write_text('{profile}')\n"
        ))

    def install_wrong_profile(self, profile):
        for name in ('bloomd', 'bloom-tui'):
            self.stub(profile, name, (
                'from pathlib import Path\n'
                "Path('wrong-profile.marker').write_text('used')\n"
                'raise SystemExit(1)\n'
            ))

    def run_launcher(self, *arguments):
        return subprocess.run(
            [str(self.root / 'scripts/bloom'), *arguments],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=15,
        )

    def kill_daemon(self):
        path = self.root / 'daemon.pid'
        if not path.exists():
            return
        try:
            pid = int(path.read_text())
            os.kill(pid, signal.SIGKILL)
        except (ValueError, ProcessLookupError):
            pass

    def test_release_flag_builds_and_runs_the_release_binaries(self):
        self.install_profile('release')
        self.install_wrong_profile('debug')
        result = self.run_launcher('--release')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('release profile', result.stderr)
        self.assertIn('--release', (self.root / 'cargo.args').read_text())
        self.assertEqual((self.root / 'tui.marker').read_text(), 'release')
        self.assertFalse((self.root / 'wrong-profile.marker').exists())

    def test_default_builds_and_runs_the_release_binaries(self):
        self.install_profile('release')
        self.install_wrong_profile('debug')
        result = self.run_launcher()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('release profile', result.stderr)
        self.assertIn('--release', (self.root / 'cargo.args').read_text())
        self.assertEqual((self.root / 'tui.marker').read_text(), 'release')
        self.assertFalse((self.root / 'wrong-profile.marker').exists())

    def test_debug_flag_builds_and_runs_the_debug_binaries(self):
        self.install_profile('debug')
        self.install_wrong_profile('release')
        result = self.run_launcher('--debug')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('debug profile', result.stderr)
        self.assertNotIn('--release', (self.root / 'cargo.args').read_text())
        self.assertEqual((self.root / 'tui.marker').read_text(), 'debug')
        self.assertFalse((self.root / 'wrong-profile.marker').exists())

    def test_existing_daemon_is_reused_without_starting_another(self):
        self.install_profile('release')
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(self.env['BLOOM_SOCKET_PATH'])
        listener.listen(8)
        self.addCleanup(listener.close)
        result = self.run_launcher('--release')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('reusing the existing daemon', result.stderr)
        self.assertEqual((self.root / 'tui.marker').read_text(), 'release')
        self.assertFalse((self.root / 'daemon.pid').exists())


class CheckoutLauncherOptions(unittest.TestCase):
    """The checkout launcher must expose the profile opt-in without starting."""

    launcher = Path(__file__).resolve().parents[1] / 'scripts/bloom'

    def run_launcher(self, *arguments):
        return subprocess.run(
            [str(self.launcher), *arguments],
            cwd=self.launcher.parents[1],
            capture_output=True,
            text=True,
            timeout=10,
        )

    def test_help_documents_the_release_default_and_debug_opt_in(self):
        result = self.run_launcher('--help')
        self.assertEqual(result.returncode, 0)
        self.assertIn('--release', result.stdout)
        self.assertIn('--debug', result.stdout)
        self.assertIn('(default)', result.stdout)

    def test_unknown_option_is_refused_before_building(self):
        result = self.run_launcher('--bogus')
        self.assertEqual(result.returncode, 2)
        self.assertIn('usage:', result.stderr)

    def test_extra_arguments_are_refused(self):
        result = self.run_launcher('--release', 'extra')
        self.assertEqual(result.returncode, 2)
        self.assertIn('usage:', result.stderr)


if __name__ == '__main__':
    unittest.main(verbosity=2)
