// Tests for the chat-sync mapping, using plain values so no engine is needed.

import '../bin/bloom_engine.dart' as sidecar;
import 'package:test/test.dart';

void main() {
  group('conversation sync bounds', () {
    test('an empty page has no remaining results', () {
      expect(
        sidecar.conversationSyncHasMore(
          status: 1,
          pageIsEmpty: true,
          hasContinuation: true,
        ),
        isFalse,
      );
    });

    test('an exact row cap is complete when no continuation remains', () {
      expect(
        sidecar.conversationSyncLimitReached(
          pages: 1,
          rows: 20000,
          hasMore: false,
        ),
        isFalse,
      );
      expect(
        sidecar.conversationSyncLimitReached(
          pages: 1,
          rows: 20000,
          hasMore: true,
        ),
        isTrue,
      );
    });

    test('a page cap is limited only when a continuation remains', () {
      expect(
        sidecar.conversationSyncLimitReached(
          pages: 200,
          rows: 10,
          hasMore: false,
        ),
        isFalse,
      );
      expect(
        sidecar.conversationSyncLimitReached(
          pages: 200,
          rows: 10,
          hasMore: true,
        ),
        isTrue,
      );
    });
  });

  group('conversation mapping', () {
    test('uses the display name and flags a group', () {
      final mapped = sidecar.conversationFromChat(
        guid: 'iMessage;-;group',
        chatIdentifier: 'group',
        displayName: 'Weekend crew',
        participants: [
          'mailto:a@example.invalid',
          'mailto:b@example.invalid',
          'mailto:me@example.invalid'
        ],
      );
      expect(mapped, isNotNull);
      expect(mapped!['guid'], 'iMessage;-;group');
      expect(mapped['title'], 'Weekend crew');
      expect(mapped['is_group'], isTrue);
      expect((mapped['participants']! as List).length, 3);
    });

    test('nulls an empty title instead of inventing one', () {
      final mapped = sidecar.conversationFromChat(
        guid: 'g',
        chatIdentifier: 'g',
        displayName: '   ',
        participants: ['mailto:a@example.invalid'],
      );
      expect(mapped!['title'], isNull);
      expect(mapped['is_group'], isFalse);
    });

    test('falls back to the chat identifier when the guid is empty', () {
      final mapped = sidecar.conversationFromChat(
        guid: '  ',
        chatIdentifier: 'iMessage;-;fallback',
        displayName: null,
        participants: ['tel:+15550000001'],
      );
      expect(mapped!['guid'], 'iMessage;-;fallback');
    });

    test('returns null when neither identifier is usable', () {
      expect(
        sidecar.conversationFromChat(
          guid: '',
          chatIdentifier: ' ',
          displayName: 'x',
          participants: ['mailto:a@example.invalid'],
        ),
        isNull,
      );
    });

    test('drops blank participant entries', () {
      final mapped = sidecar.conversationFromChat(
        guid: 'g',
        chatIdentifier: 'g',
        displayName: null,
        participants: ['mailto:a@example.invalid', '  ', ''],
      );
      expect((mapped!['participants']! as List), ['mailto:a@example.invalid']);
    });
  });
}
