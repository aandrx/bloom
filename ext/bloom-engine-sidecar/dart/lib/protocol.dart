// Private child-process protocol between the Bloom daemon and this sidecar.
//
// Transport: newline-delimited UTF-8 JSON, one object per line, on stdin/stdout.
// stdout carries protocol frames only; diagnostics go to stderr.
//
// Every frame carries a protocol version. The daemon owns the socket IPC used by
// clients; this protocol is internal to the engine boundary.

import 'dart:convert';

const int protocolVersion = 1;

/// Reject absurd lines before parsing rather than allocating unbounded memory.
const int maxFrameBytes = 1 << 20;

/// Bounded failure codes. Arbitrary exception text never crosses this boundary.
class ProtocolCodes {
  static const String invalidFrame = 'invalid_frame';
  static const String frameTooLarge = 'frame_too_large';
  static const String unknownCommand = 'unknown_command';
  static const String unsupportedVersion = 'unsupported_version';
  static const String noSenderHandle = 'no_sender_handle';
  static const String sendFailed = 'send_failed';
  static const String engineUnavailable = 'engine_unavailable';
}

class ProtocolException implements Exception {
  ProtocolException(this.code, this.detail);

  final String code;
  final String detail;

  @override
  String toString() => 'ProtocolException($code: $detail)';
}

sealed class Command {
  const Command();
}

final class SendCommand extends Command {
  const SendCommand({
    required this.requestId,
    required this.recipient,
    required this.text,
    this.chatGuid,
  });

  final int requestId;
  final String recipient;
  final String text;

  /// When present, the message is threaded into this existing conversation.
  final String? chatGuid;
}

final class ShutdownCommand extends Command {
  const ShutdownCommand({required this.requestId});

  final int requestId;
}

/// Parse one stdin line. Throws ProtocolException on any malformed input.
Command decodeCommand(String line) {
  if (line.length > maxFrameBytes) {
    throw ProtocolException(
      ProtocolCodes.frameTooLarge,
      'line exceeds $maxFrameBytes bytes',
    );
  }
  final Object? decoded;
  try {
    decoded = jsonDecode(line);
  } catch (_) {
    throw ProtocolException(
      ProtocolCodes.invalidFrame,
      'line is not valid JSON',
    );
  }
  if (decoded is! Map<String, dynamic>) {
    throw ProtocolException(
      ProtocolCodes.invalidFrame,
      'frame must be a JSON object',
    );
  }
  final version = decoded['version'];
  if (version != protocolVersion) {
    throw ProtocolException(
      ProtocolCodes.unsupportedVersion,
      'expected version $protocolVersion',
    );
  }
  final type = decoded['type'];
  switch (type) {
    case 'send':
      final requestId = decoded['request_id'];
      final recipient = decoded['recipient'];
      final text = decoded['text'];
      if (requestId is! int || recipient is! String || text is! String) {
        throw ProtocolException(
          ProtocolCodes.invalidFrame,
          'send requires integer request_id, string recipient and string text',
        );
      }
      if (recipient.isEmpty) {
        throw ProtocolException(
          ProtocolCodes.invalidFrame,
          'recipient must not be empty',
        );
      }
      return SendCommand(
        requestId: requestId,
        recipient: recipient,
        text: text,
        chatGuid: _optionalChatGuid(decoded),
      );
    case 'shutdown':
      final requestId = decoded['request_id'];
      if (requestId is! int) {
        throw ProtocolException(
          ProtocolCodes.invalidFrame,
          'shutdown requires integer request_id',
        );
      }
      return ShutdownCommand(requestId: requestId);
    default:
      throw ProtocolException(
        ProtocolCodes.unknownCommand,
        'unknown command type',
      );
  }
}

String? _optionalChatGuid(Map<String, dynamic> decoded) {
  final chatGuid = decoded['chat_guid'];
  if (chatGuid == null) return null;
  if (chatGuid is! String) {
    throw ProtocolException(
      ProtocolCodes.invalidFrame,
      'chat_guid must be a string when present',
    );
  }
  return chatGuid;
}

String _frame(Map<String, Object?> body) =>
    jsonEncode(<String, Object?>{'version': protocolVersion, ...body});

/// Emitted once the engine is restored and the receive loop is running.
String readyFrame({
  required String engineVersion,
  required bool canSend,
  required bool canReadHistory,
  List<String> handles = const [],
}) =>
    _frame(<String, Object?>{
      'type': 'ready',
      'engine_version': engineVersion,
      'handles': handles,
      'capabilities': <String, Object?>{
        'send': canSend,
        'history': canReadHistory
      },
    });

/// One decoded inbound push payload, in the order it was received.
String inboundFrame({
  required int sequence,
  required Map<String, Object?> payload,
}) =>
    _frame(<String, Object?>{
      'type': 'inbound_message',
      'sequence': sequence,
      'payload': payload,
    });

/// Result of a correlated send command.
String sendConfirmFrame({
  required int requestId,
  required String messageId,
  required bool accepted,
  String? error,
}) =>
    _frame(<String, Object?>{
      'type': 'send_confirmation',
      'request_id': requestId,
      'message_id': messageId,
      'accepted': accepted,
      if (error != null) 'error': error,
    });

/// Acknowledgement that the sidecar has begun shutting down.
String shutdownAckFrame({required int requestId}) =>
    _frame(<String, Object?>{'type': 'shutdown_ack', 'request_id': requestId});

/// One conversation imported from the existing local history, emitted once during
/// startup import so the daemon can serve History from its own store afterwards.
String conversationFrame({
  required int sequence,
  required Map<String, Object?> conversation,
}) =>
    _frame(<String, Object?>{
      'type': 'conversation',
      'sequence': sequence,
      'conversation': conversation,
    });

/// One historical message imported from the existing local history.
String historyMessageFrame({
  required int sequence,
  required Map<String, Object?> message,
}) =>
    _frame(<String, Object?>{
      'type': 'history_message',
      'sequence': sequence,
      'message': message,
    });

/// Marks the end of the startup import. `limited` is true when the import stopped
/// early, so the daemon can log that coverage was partial rather than assume
/// completeness.
String historyCompleteFrame({
  required int conversations,
  required int messages,
  required bool limited,
}) =>
    _frame(<String, Object?>{
      'type': 'history_complete',
      'conversations': conversations,
      'messages': messages,
      'limited': limited,
    });

/// Engine session/registration state change.
String sessionFrame({required String state, String? reason}) =>
    _frame(<String, Object?>{
      'type': 'session_state',
      'state': state,
      if (reason != null) 'reason': reason,
    });

/// Bounded error frame. Detail is a code, never raw exception text.
String errorFrame({
  int? requestId,
  required String code,
  bool retryable = false,
}) =>
    _frame(<String, Object?>{
      'type': 'error',
      'code': code,
      'retryable': retryable,
      if (requestId != null) 'request_id': requestId,
    });
