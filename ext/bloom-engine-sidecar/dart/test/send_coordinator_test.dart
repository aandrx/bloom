import 'dart:convert';

import '../lib/send_coordinator.dart';
import 'package:test/test.dart';

void main() {
  group('SendCoordinator', () {
    late List<Map<String, Object?>> frames;
    late SendCoordinator coordinator;

    setUp(() {
      frames = <Map<String, Object?>>[];
      coordinator = SendCoordinator((frame) {
        frames.add(jsonDecode(frame) as Map<String, Object?>);
      });
    });

    test('false return emits accepted, not rejection', () {
      coordinator.register('message-1', 11);
      coordinator.sendReturned(messageId: 'message-1', stillRunning: false);
      expect(frames, hasLength(1));
      expect(frames.single['accepted'], isTrue);
      expect(frames.single['request_id'], 11);
    });

    test('true return waits for success push', () {
      coordinator.register('message-1', 12);
      coordinator.sendReturned(messageId: 'message-1', stillRunning: true);
      expect(frames, isEmpty);
      expect(coordinator.completePush(messageId: 'message-1'), isTrue);
      expect(frames.single['accepted'], isTrue);
    });

    test('push before false return wins exactly once', () {
      coordinator.register('message-1', 13);
      expect(coordinator.completePush(messageId: 'message-1', error: 'failed'),
          isTrue);
      coordinator.sendReturned(messageId: 'message-1', stillRunning: false);
      expect(frames, hasLength(1));
      expect(frames.single['accepted'], isFalse);
      expect(frames.single['error'], 'failed');
    });

    test('failure cleanup and duplicate pushes do not emit twice', () {
      coordinator.register('message-1', 14);
      coordinator.discard('message-1');
      expect(coordinator.completePush(messageId: 'message-1'), isFalse);
      expect(coordinator.completePush(messageId: 'unknown'), isFalse);
      expect(frames, <Map<String, Object?>>[]);
    });

    test('duplicate completion cannot contradict first completion', () {
      coordinator.register('message-1', 15);
      coordinator.completePush(messageId: 'message-1');
      coordinator.completePush(messageId: 'message-1', error: 'late error');
      coordinator.sendReturned(messageId: 'message-1', stillRunning: false);
      expect(frames, hasLength(1));
      expect(frames.single['accepted'], isTrue);
      expect(frames.single['error'], isNull);
    });
  });
}
