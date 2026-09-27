// Handle normalisation tests.
//
// The regression these guard: the daemon already sends handles in engine form
// (`mailto:`/`tel:`). Prefixing them again produced `mailto:mailto:...`, which
// resolves to no IDS target, so every reply failed with "No valid IDS targets
// for message".

import '../bin/bloom_engine.dart' as sidecar;

import 'package:test/test.dart';

void main() {
  group('engineHandle', () {
    test('leaves an already prefixed handle alone', () {
      expect(
        sidecar.engineHandle('mailto:person@example.invalid'),
        'mailto:person@example.invalid',
      );
      expect(sidecar.engineHandle('tel:+15551234567'), 'tel:+15551234567');
    });

    test('prefixes a bare handle by its shape', () {
      expect(
        sidecar.engineHandle('person@example.invalid'),
        'mailto:person@example.invalid',
      );
      expect(sidecar.engineHandle('+15551234567'), 'tel:+15551234567');
    });

    test('is idempotent: a second pass cannot double the prefix', () {
      for (final value in <String>[
        'person@example.invalid',
        '+15551234567',
        'mailto:person@example.invalid',
        'tel:+15551234567',
      ]) {
        final once = sidecar.engineHandle(value);
        expect(sidecar.engineHandle(once), once, reason: value);
      }
    });
  });

  group('chooseSender', () {
    test('prefers a handle whose scheme matches the recipient', () {
      final handles = <String>['tel:+15550000000', 'mailto:me@example.invalid'];
      expect(
        sidecar.chooseSender(handles, 'mailto:person@example.invalid'),
        'mailto:me@example.invalid',
      );
      expect(sidecar.chooseSender(handles, 'tel:+15551111111'),
          'tel:+15550000000');
    });

    test('falls back to the first account handle', () {
      expect(sidecar.chooseSender(<String>['tel:+15550000000'], 'mailto:a@b.c'),
          'tel:+15550000000');
    });

    test('has no sender without handles', () {
      expect(sidecar.chooseSender(<String>[], 'mailto:a@b.c'), isNull);
    });
  });
}
