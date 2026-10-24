#!/usr/bin/env python3
"""Tests for scripts/stage-prefix using temporary prefixes and fake payloads."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SOURCE_SCRIPT = Path(__file__).resolve().parent / 'stage-prefix'


class StagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='bloom-stage-')
        self.root = Path(self.temp.name)
        self.source = self.root / 'source'
        (self.source / 'scripts').mkdir(parents=True)
        shutil.copy2(SOURCE_SCRIPT, self.source / 'scripts' / 'stage-prefix')
        self.bin = self.root / 'build-bin'
        self.bin.mkdir()
        for name in ('bloom', 'bloom-tui', 'bloomd'):
            path = self.bin / name
            path.write_text(f'#!/bin/sh\n# fake {name}\necho {name}\n')
            path.chmod(0o755)
        self.prefix = self.root / 'prefix'

    def tearDown(self):
        self.temp.cleanup()

    def make_sidecar(self):
        sidecar = self.source / 'ext' / 'bloom-engine-sidecar'
        dart = sidecar / 'dart'
        (dart / 'bin').mkdir(parents=True)
        (dart / 'lib' / 'upstream').mkdir(parents=True)
        (dart / 'bin' / 'bloom_engine.dart').write_text('void main() {}\n')
        (dart / 'lib' / 'protocol.dart').write_text('class Protocol {}\n')
        (dart / 'lib' / 'upstream' / 'api.dart').write_text('class Api {}\n')
        (dart / 'pubspec.yaml').write_text('name: bloom_engine_sidecar\n')
        (dart / 'pubspec.lock').write_text('packages: {}\n')
        (dart / '.dart_tool').mkdir()
        (dart / '.dart_tool' / 'package_config.json').write_text('{}\n')
        (dart / 'build').mkdir()
        (dart / 'build' / 'generated.dart').write_text('// generated\n')
        (dart / '.packages').write_text('cache\n')
        (sidecar / 'ATTRIBUTION').write_text(
            'Generated Dart bindings from OpenBubbles, public release tag '
            'v1.15.0+205, commit 852c9910f456492f7f840ac4ff2a8bad7a1ee8c8.\n')
        return sidecar / 'dart'

    def run_script(self, *args, env=None):
        command = [sys.executable, str(self.source / 'scripts' / 'stage-prefix')]
        if env is None:
            env = {key: value for key, value in os.environ.items()
                   if key not in ('BLOOM_ENGINE_LIBRARY', 'BLOOM_ENGINE_SCRIPT')}
        return subprocess.run(command + list(args), capture_output=True,
                              text=True, env=env)

    def manifest(self):
        path = self.prefix / 'stage-manifest.json'
        return json.loads(path.read_text())

    def assert_mode(self, path):
        self.assertEqual(path.stat().st_mode & 0o777, 0o755, str(path))

    def test_happy_path_layout_modes_and_manifest_digests(self):
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix))
        self.assertEqual(result.returncode, 0, result.stderr)
        for name in ('bloom', 'bloom-tui', 'bloomd'):
            staged = self.prefix / 'bin' / name
            self.assertTrue(staged.is_file())
            self.assert_mode(staged)
            self.assertEqual(staged.read_bytes(), (self.bin / name).read_bytes())
        manifest = self.manifest()
        self.assertEqual(manifest['schema_version'], 1)
        self.assertIsNone(manifest['attribution'])
        entries = manifest['entries']
        self.assertEqual([entry['path'] for entry in entries],
                         ['bin/bloom', 'bin/bloom-tui', 'bin/bloomd'])
        for entry in entries:
            staged = self.prefix / entry['path']
            self.assertEqual(entry['mode'], 0o755)
            self.assertEqual(entry['sha256'],
                             hashlib.sha256(staged.read_bytes()).hexdigest())

    def test_sidecar_staging_excludes_artifacts_and_records_attribution(self):
        self.make_sidecar()
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix),
                                 '--with-sidecar')
        self.assertEqual(result.returncode, 0, result.stderr)
        engine = self.prefix / 'share' / 'bloom' / 'engine'
        self.assertTrue((engine / 'bin' / 'bloom_engine.dart').is_file())
        self.assertTrue((engine / 'lib' / 'upstream' / 'api.dart').is_file())
        self.assertTrue((engine / 'pubspec.yaml').is_file())
        self.assertTrue((engine / 'ATTRIBUTION').is_file())
        self.assert_mode(engine / 'bin' / 'bloom_engine.dart')
        self.assertFalse((engine / '.dart_tool').exists())
        self.assertFalse((engine / 'build').exists())
        self.assertFalse((engine / '.packages').exists())
        manifest = self.manifest()
        self.assertEqual(manifest['attribution']['tag'], 'v1.15.0+205')
        self.assertEqual(manifest['attribution']['commit'],
                         '852c9910f456492f7f840ac4ff2a8bad7a1ee8c8')
        self.assertIn('share/bloom/engine/ATTRIBUTION',
                      [entry['path'] for entry in manifest['entries']])
        self.assertIn('BLOOM_ENGINE_SCRIPT=', result.stdout)
        self.assertIn('dart pub get', result.stdout)

    def test_dry_run_writes_nothing(self):
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix), '--dry-run')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.prefix.exists())
        self.assertIn('would copy bin/bloom', result.stdout)
        self.assertIn('nothing written', result.stdout)

    def test_non_empty_prefix_refused_without_force_and_accepted_with_it(self):
        self.prefix.mkdir()
        (self.prefix / 'keep.txt').write_text('keep\n')
        refused = self.run_script('--bin-dir', str(self.bin),
                                  '--prefix', str(self.prefix))
        self.assertEqual(refused.returncode, 1)
        self.assertIn('not empty', refused.stderr)
        self.assertFalse((self.prefix / 'bin').exists())
        accepted = self.run_script('--bin-dir', str(self.bin),
                                   '--prefix', str(self.prefix), '--force')
        self.assertEqual(accepted.returncode, 0, accepted.stderr)
        self.assertTrue((self.prefix / 'keep.txt').is_file())
        self.assertTrue((self.prefix / 'bin' / 'bloom').is_file())

    def test_prefix_inside_source_tree_refused(self):
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.source / 'install'))
        self.assertEqual(result.returncode, 1)
        self.assertIn('source tree', result.stderr)
        self.assertFalse((self.source / 'install').exists())

    def test_prefix_with_state_store_or_registration_refused(self):
        markers = {'state': True, 'store.db': False,
                   'share/bloom/registration.json': False}
        for marker, is_dir in markers.items():
            with self.subTest(marker=marker):
                shutil.rmtree(self.prefix, ignore_errors=True)
                target = self.prefix / marker
                if is_dir:
                    target.mkdir(parents=True)
                else:
                    target.parent.mkdir(parents=True)
                    target.write_text('synthetic\n')
                result = self.run_script('--bin-dir', str(self.bin),
                                         '--prefix', str(self.prefix))
                self.assertEqual(result.returncode, 1)
                self.assertIn('state or registration', result.stderr)
                self.assertFalse((self.prefix / 'bin').exists())

    def test_versioned_shared_library_refused(self):
        dart = self.make_sidecar()
        (dart / 'lib' / 'librust_lib_bluebubbles.so.1').write_text('binary\n')
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix),
                                 '--with-sidecar')
        self.assertEqual(result.returncode, 1)
        self.assertIn('shared library', result.stderr)
        self.assertFalse((self.prefix / 'share' / 'bloom' / 'engine' / 'lib'
                          / 'librust_lib_bluebubbles.so.1').exists())

    def test_engine_library_inside_prefix_refused(self):
        library = self.prefix / 'lib' / 'librust_lib_bluebubbles.so'
        library.parent.mkdir(parents=True)
        library.write_text('binary\n')
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix),
                                 env={**{k: v for k, v in os.environ.items()},
                                      'BLOOM_ENGINE_LIBRARY': str(library)})
        self.assertEqual(result.returncode, 1)
        self.assertIn('must stay external', result.stderr)

    def test_prefix_path_that_is_a_file_refused(self):
        self.prefix.write_text('not a directory\n')
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix))
        self.assertEqual(result.returncode, 1)
        self.assertIn('not a directory', result.stderr)
        self.assertEqual(self.prefix.read_text(), 'not a directory\n')

    def test_missing_binary_refused(self):
        (self.bin / 'bloomd').unlink()
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix))
        self.assertEqual(result.returncode, 1)
        self.assertIn('missing binary', result.stderr)
        self.assertFalse(self.prefix.exists())

    def test_missing_sidecar_tree_refused(self):
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix),
                                 '--with-sidecar')
        self.assertEqual(result.returncode, 1)
        self.assertIn('sidecar source tree', result.stderr)
        self.assertFalse(self.prefix.exists())

    def test_manifest_contains_no_absolute_or_home_paths(self):
        self.make_sidecar()
        result = self.run_script('--bin-dir', str(self.bin),
                                 '--prefix', str(self.prefix),
                                 '--with-sidecar')
        self.assertEqual(result.returncode, 0, result.stderr)
        text = (self.prefix / 'stage-manifest.json').read_text()
        self.assertNotIn(str(self.root), text)
        self.assertNotIn(str(Path.home()), text)

        def walk(value):
            if isinstance(value, dict):
                for item in value.values():
                    walk(item)
            elif isinstance(value, list):
                for item in value:
                    walk(item)
            elif isinstance(value, str):
                self.assertFalse(value.startswith('/'), value)

        walk(json.loads(text))

    def test_usage_errors_exit_two(self):
        cases = [
            (),
            ('--bin-dir', str(self.bin)),
            ('--bin-dir', str(self.bin), '--prefix', str(self.prefix),
             '--unknown'),
            ('--bin-dir', str(self.bin), '--prefix', str(self.prefix),
             '--dry-run', '--force'),
        ]
        for args in cases:
            with self.subTest(args=args):
                result = self.run_script(*args)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertTrue(result.stderr.strip())


if __name__ == '__main__':
    unittest.main()
