// Protocol tests: framing, versioning, validation and bounded errors.

import 'dart:convert';

import 'package:bloom_engine_sidecar/protocol.dart';
import 'package:test/test.dart';

Map<String, Object?> decodeFrame(String line) =>
    jsonDecode(line) as Map<String, Object?>;

void main() {
  group('commands', () {
    test('accepts a well formed send', () {
      final command = decodeCommand(
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'send',
          'request_id': 7,
          'recipients': ['someone@example.com'],
          'text': 'hello',
        }),
      );
      expect(command, isA<SendCommand>());
      final send = command as SendCommand;
      expect(send.requestId, 7);
      expect(send.recipients, ['someone@example.com']);
      expect(send.text, 'hello');
    });

    test('accepts an optional chat guid for threading', () {
      final command = decodeCommand(
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'send',
          'request_id': 8,
          'recipients': ['someone@example.com'],
          'text': 'hi',
          'chat_guid': 'iMessage;-;someone@example.com',
        }),
      );
      expect(
        (command as SendCommand).chatGuid,
        'iMessage;-;someone@example.com',
      );

      expect(
        () => decodeCommand(
          jsonEncode(<String, Object?>{
            'version': protocolVersion,
            'type': 'send',
            'request_id': 8,
            'recipients': ['someone@example.com'],
            'text': 'hi',
            'chat_guid': 5,
          }),
        ),
        throwsA(
          isA<ProtocolException>().having(
            (e) => e.code,
            'code',
            ProtocolCodes.invalidFrame,
          ),
        ),
      );
    });

    test('accepts sync and requires an integer request id', () {
      final command = decodeCommand(
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'sync_conversations',
          'request_id': 9,
        }),
      );
      expect(command, isA<SyncConversationsCommand>());
      expect((command as SyncConversationsCommand).requestId, 9);
      expect(
        () => decodeCommand(
          jsonEncode(<String, Object?>{
            'version': protocolVersion,
            'type': 'sync_conversations',
            'request_id': '9',
          }),
        ),
        throwsA(isA<ProtocolException>()),
      );
    });

    test('accepts shutdown', () {
      final command = decodeCommand(
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'shutdown',
          'request_id': 1,
        }),
      );
      expect(command, isA<ShutdownCommand>());
    });

    test('rejects a scalar-only legacy send with a correlated response', () {
      final frame = decodeFrame(
        rejectedCommandFrame(
          jsonEncode(<String, Object?>{
            'version': protocolVersion,
            'type': 'send',
            'request_id': 12,
            'recipient': 'one@example.invalid',
            'text': 'hello',
          }),
          ProtocolCodes.invalidFrame,
        ),
      );
      expect(frame['type'], 'send_confirmation');
      expect(frame['request_id'], 12);
      expect(frame['accepted'], isFalse);
      expect(frame['message_id'], '');
      expect(frame['error'], ProtocolCodes.invalidFrame);

      final unrelated = decodeFrame(
        rejectedCommandFrame(
          jsonEncode(<String, Object?>{
            'version': protocolVersion,
            'type': 'shutdown',
            'request_id': 12,
          }),
          ProtocolCodes.invalidFrame,
        ),
      );
      expect(unrelated['type'], 'error');
      expect(rejectedSendRequestId('not json'), isNull);
    });

    test('rejects malformed frames with bounded codes', () {
      final cases = <String, String>{
        'not json at all': ProtocolCodes.invalidFrame,
        '[1,2,3]': ProtocolCodes.invalidFrame,
        jsonEncode(<String, Object?>{
          'version': protocolVersion + 1,
          'type': 'send',
        }): ProtocolCodes.unsupportedVersion,
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'nonsense',
        }): ProtocolCodes.unknownCommand,
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'send',
          'request_id': 'seven',
          'recipients': ['a'],
          'text': 'b',
        }): ProtocolCodes.invalidFrame,
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'send',
          'request_id': 1,
          'recipients': [],
          'text': 'b',
        }): ProtocolCodes.invalidFrame,
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'send',
          'request_id': 1,
          'recipients': [''],
          'text': 'b',
        }): ProtocolCodes.invalidFrame,
        jsonEncode(<String, Object?>{
          'version': protocolVersion,
          'type': 'send',
          'request_id': 1,
          'recipients': [3],
          'text': 'b',
        }): ProtocolCodes.invalidFrame,
      };

      for (final entry in cases.entries) {
        expect(
          () => decodeCommand(entry.key),
          throwsA(
            isA<ProtocolException>().having((e) => e.code, 'code', entry.value),
          ),
          reason: 'input: ${entry.key}',
        );
      }
    });

    test('rejects oversized lines before parsing', () {
      final huge = 'x' * (maxFrameBytes + 1);
      expect(
        () => decodeCommand(huge),
        throwsA(
          isA<ProtocolException>().having(
            (e) => e.code,
            'code',
            ProtocolCodes.frameTooLarge,
          ),
        ),
      );
    });
  });

  group('frames', () {
    test('sync result carries its correlated status and count', () {
      final frame = decodeFrame(
        syncResultFrame(requestId: 9, status: 'limited', conversations: 12),
      );
      expect(frame['type'], 'sync_result');
      expect(frame['request_id'], 9);
      expect(frame['status'], 'limited');
      expect(frame['conversations'], 12);
      expect(frame['version'], protocolVersion);
    });

    test('wire marker protects protocol lines from foreign stdout', () {
      final line = wireLine('{"version":1}');
      expect(line.startsWith(wireMarker), isTrue);
      expect(line.substring(wireMarker.length), '{"version":1}');
    });

    test('every frame carries the protocol version', () {
      for (final line in <String>[
        readyFrame(engineVersion: 'v', canSend: true, canReadHistory: false),
        inboundFrame(
          sequence: 1,
          payload: <String, Object?>{'variant': 'iMessage', 'complete': true},
        ),
        sendConfirmFrame(requestId: 1, messageId: 'm', accepted: true),
        shutdownAckFrame(requestId: 2),
        sessionFrame(state: 'active'),
        errorFrame(code: ProtocolCodes.sendFailed, retryable: true),
      ]) {
        expect(decodeFrame(line)['version'], protocolVersion);
      }
    });

    test('history stays unavailable until a producer exists', () {
      expect(historyCapability, isFalse);
      // Assert the frame the bin emits, not a frame reconstructed here, so a
      // call-site revert to chat sync fails this test.
      final frame = decodeFrame(
        sidecarReadyFrame(
          engineVersion: 'v',
          handles: const ['mailto:me@example.invalid'],
        ),
      );
      expect(frame['capabilities'], <String, Object?>{
        'send': true,
        'history': false,
        'recipient_lists': true,
      });
      expect(frame['handles'], ['mailto:me@example.invalid']);
    });

    test('ready advertises capabilities', () {
      final frame = decodeFrame(
        readyFrame(
          engineVersion: 'openbubbles-v1',
          canSend: true,
          canReadHistory: false,
          handles: const ['mailto:me@example.invalid'],
        ),
      );
      expect(frame['type'], 'ready');
      expect(frame['handles'], ['mailto:me@example.invalid']);
      expect(frame['engine_version'], 'openbubbles-v1');
      expect(frame['capabilities'], <String, Object?>{
        'send': true,
        'history': false,
        'recipient_lists': true,
      });
    });

    test('send confirmation correlates the request', () {
      final frame = decodeFrame(
        sendConfirmFrame(
          requestId: 42,
          messageId: 'uuid',
          accepted: false,
          error: 'not delivered',
        ),
      );
      expect(frame['type'], 'send_confirmation');
      expect(frame['request_id'], 42);
      expect(frame['accepted'], isFalse);
      expect(frame['error'], 'not delivered');
    });

    test('shutdown is acknowledged distinctly from a send', () {
      final frame = decodeFrame(shutdownAckFrame(requestId: 9));
      expect(frame['type'], 'shutdown_ack');
      expect(frame['request_id'], 9);
      expect(frame.containsKey('message_id'), isFalse);
    });

    test('error frames omit absent request ids rather than sending null', () {
      final frame = decodeFrame(errorFrame(code: ProtocolCodes.invalidFrame));
      expect(frame.containsKey('request_id'), isFalse);
      expect(frame['retryable'], isFalse);
    });
  });
}
