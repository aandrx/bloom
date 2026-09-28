// Configuration and portability tests.
//
// These guard two hard requirements: paths come from arguments or the environment
// only, and the package must not embed any personal machine path.

import 'dart:io';

import '../bin/bloom_engine.dart' as sidecar;

import 'package:test/test.dart';

void main() {
  group('configuration', () {
    test('refuses to run without an explicit state directory', () {
      expect(
        sidecar.parseConfig(<String>['--library', '/nonexistent/lib.so']),
        isNull,
      );
      expect(sidecar.parseConfig(<String>[]), isNull);
    });

    test('refuses a missing library or state directory', () {
      Directory.systemTemp.createTempSync('bloom-sidecar-test');
      expect(
        sidecar.parseConfig(<String>[
          '--state',
          '/tmp',
          '--library',
          '/nope/lib.so',
        ]),
        isNull,
      );
      expect(
        sidecar.parseConfig(<String>[
          '--state',
          '/nope/state',
          '--library',
          '/tmp',
        ]),
        isNull,
      );
    });

    test('accepts explicit arguments', () {
      final dir = Directory.systemTemp.createTempSync('bloom-sidecar-ok');
      final library = File('${dir.path}/engine.so')..createSync();
      final config = sidecar.parseConfig(<String>[
        '--state',
        dir.path,
        '--library',
        library.path,
      ]);
      expect(config, isNotNull);
      expect(config!.stateDirectory, dir.path);
      expect(config.libraryPath, library.path);
    });
  });

  group('sender selection', () {
    test('matches the recipient scheme when possible', () {
      final handles = <String>['tel:+15550000001', 'mailto:me@example.invalid'];
      expect(
        sidecar.chooseSender(handles, ['someone@example.com']),
        'mailto:me@example.invalid',
      );
      expect(
          sidecar.chooseSender(handles, ['+15550000009']), 'tel:+15550000001');
    });

    test('falls back to the first handle and reports no handles', () {
      expect(
        sidecar
            .chooseSender(<String>['mailto:only@example.invalid'], ['+1555']),
        'mailto:only@example.invalid',
      );
      expect(sidecar.chooseSender(<String>[], ['x@example.com']), isNull);
    });
  });

  group('portability', () {
    test('no personal machine path is compiled into the sidecar', () {
      for (final path in <String>[
        'bin/bloom_engine.dart',
        'lib/payload_codec.dart',
        'lib/protocol.dart',
        'README.md',
        'ATTRIBUTION',
        'tool/refresh_bindings.sh',
      ]) {
        final file = File(path);
        if (!file.existsSync()) continue;
        final content = file.readAsStringSync();
        expect(
          content.contains('/home/'),
          isFalse,
          reason: '$path must not embed a personal path',
        );
      }
    });
  });
}
