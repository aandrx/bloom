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
    test('prefers a handle whose scheme matches a recipient', () {
      final handles = <String>['tel:+15550000000', 'mailto:me@example.invalid'];
      expect(
        sidecar.chooseSender(handles, ['mailto:person@example.invalid']),
        'mailto:me@example.invalid',
      );
      expect(
        sidecar.chooseSender(handles, ['tel:+15551111111']),
        'tel:+15550000000',
      );
    });

    test('supports a mixed email and telephone group', () {
      final handles = <String>['tel:+15550000000', 'mailto:me@example.invalid'];
      expect(
        sidecar.chooseSender(
          handles,
          ['mailto:person@example.invalid', 'tel:+15551111111'],
        ),
        'tel:+15550000000',
      );
    });

    test('falls back to the first account handle', () {
      expect(
        sidecar.chooseSender(<String>['tel:+15550000000'], ['mailto:a@b.c']),
        'tel:+15550000000',
      );
    });

    test('has no sender without handles or recipients', () {
      expect(sidecar.chooseSender(<String>[], ['mailto:a@b.c']), isNull);
      expect(sidecar.chooseSender(['tel:+15550000000'], []), isNull);
    });
  });

  group('engine conversation data', () {
    test('keeps every target, deduplicates sender, and preserves group guid',
        () {
      const sender = 'mailto:me@example.invalid';
      const guid = 'iMessage;-;group-chat-guid';
      final conversation = sidecar.engineConversationData(
        recipients: [
          'mailto:one@example.invalid',
          'tel:+15550000002',
          'one@example.invalid',
          'me@example.invalid',
        ],
        sender: sender,
        chatGuid: guid,
      );
      expect(conversation.participants, [
        'mailto:one@example.invalid',
        'tel:+15550000002',
        sender,
      ]);
      expect(conversation.senderGuid, guid);
    });

    test('rejects a recipient set with no remote target', () {
      expect(
        () => sidecar.engineConversationData(
          recipients: ['me@example.invalid'],
          sender: 'mailto:me@example.invalid',
        ),
        throwsArgumentError,
      );
    });
  });
}
