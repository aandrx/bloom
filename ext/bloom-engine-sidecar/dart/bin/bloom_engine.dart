// Long-running engine sidecar.
//
// Owns the OpenBubbles engine library and its state directory, and exposes the
// engine to the Bloom daemon over a newline-delimited JSON protocol on stdio.
// stdout is protocol only; diagnostics go to stderr.
//
// Requirements: --state DIR (or BLOOM_ENGINE_STATE), --library FILE (or
// BLOOM_ENGINE_LIBRARY). The state directory must be owned exclusively: the
// sidecar refuses to start while the OpenBubbles app is running, because both
// would write the same registration state.

import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:math';

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated_io.dart';

import '../lib/payload_codec.dart';
import '../lib/protocol.dart';
import '../lib/upstream/api/api.dart' as api;
import '../lib/upstream/lib.dart' as upstream;
import '../lib/upstream/frb_generated.dart';

const String engineVersion = 'openbubbles-v1.15.0+205';

class SidecarConfig {
  SidecarConfig({
    required this.stateDirectory,
    required this.libraryPath,
    required this.syncChats,
  });

  final String stateDirectory;
  final String libraryPath;

  /// Listing the account's existing conversations. The daemon asks for it when its
  /// store has no conversations yet: a fresh store shows nothing otherwise, and an
  /// established one should not pay for a full listing on every start.
  final bool syncChats;
}

SidecarConfig? parseConfig(List<String> arguments) {
  final state = _option(arguments, '--state') ??
      Platform.environment['BLOOM_ENGINE_STATE'];
  final library = _option(arguments, '--library') ??
      Platform.environment['BLOOM_ENGINE_LIBRARY'];
  if (state == null || state.isEmpty) {
    stderr.writeln(
      'error: explicit state directory required (--state or BLOOM_ENGINE_STATE)',
    );
    return null;
  }
  if (library == null || library.isEmpty) {
    stderr.writeln(
      'error: engine library required (--library or BLOOM_ENGINE_LIBRARY)',
    );
    return null;
  }
  if (!Directory(state).existsSync()) {
    stderr.writeln('error: state directory does not exist');
    return null;
  }
  if (!File(library).existsSync()) {
    stderr.writeln('error: engine library not found');
    return null;
  }
  return SidecarConfig(
    stateDirectory: state,
    libraryPath: library,
    syncChats: arguments.contains('--sync-chats'),
  );
}

String? _option(List<String> arguments, String name) {
  final index = arguments.indexOf(name);
  if (index < 0 || index + 1 >= arguments.length) return null;
  return arguments[index + 1];
}

/// Another owner of the state directory would corrupt it. Refuse rather than share.
Future<bool> anotherOwnerIsRunning() async {
  final process = await Process.run('pgrep', <String>['-x', 'bluebubbles']);
  if (process.exitCode == 0) return true;
  final flatpak = await Process.run('flatpak', <String>['ps']);
  return flatpak.exitCode == 0 &&
      '${flatpak.stdout}'.contains('app.openbubbles.OpenBubbles');
}

String _uuid4() {
  final random = Random.secure();
  final bytes = List<int>.generate(16, (_) => random.nextInt(256));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  final hex = bytes.map((b) => b.toRadixString(16).padLeft(2, '0')).join();
  return '${hex.substring(0, 8)}-${hex.substring(8, 12)}-'
      '${hex.substring(12, 16)}-${hex.substring(16, 20)}-${hex.substring(20)}';
}

/// Pick a sending address whose scheme matches the recipient's.
String? chooseSender(List<String> handles, String recipient) {
  if (handles.isEmpty) return null;
  final scheme = recipient.contains('@') ? 'mailto' : 'tel';
  for (final handle in handles) {
    if (handle.startsWith('$scheme:')) return handle;
  }
  return handles.first;
}

api.MessageInst buildMessage({
  required String recipient,
  required String sender,
  required String text,
  String? chatGuid,
}) {
  final scheme = recipient.contains('@') ? 'mailto' : 'tel';
  return api.MessageInst(
    id: _uuid4(),
    sender: sender,
    conversation: api.ConversationData(
      participants: <String>['$scheme:$recipient', sender],
      cvName: null,
      senderGuid: chatGuid ?? _uuid4(),
      afterGuid: null,
    ),
    message: api.Message.message(
      api.NormalMessage(
        parts: api.MessageParts(
          field0: <api.IndexedMessagePart>[
            api.IndexedMessagePart(part_: textPart(text), idx: 0),
          ],
        ),
        service: const api.MessageType.iMessage(),
        voice: false,
      ),
    ),
    sentTimestamp: DateTime.now().millisecondsSinceEpoch,
    sendDelivered: false,
    verificationFailed: false,
  );
}

api.MessagePart textPart(String text) => api.MessagePart.text(
      text,
      api.TextFormat.flags(
        api.TextFlags(
          bold: false,
          italic: false,
          underline: false,
          strikethrough: false,
        ),
      ),
    );

Future<void> main(List<String> arguments) async {
  final config = parseConfig(arguments);
  if (config == null) {
    exitCode = 2;
    return;
  }
  if (await anotherOwnerIsRunning()) {
    stderr.writeln(
      'error: OpenBubbles is running; refusing shared-state access',
    );
    exitCode = 2;
    return;
  }

  await RustLib.init(externalLibrary: ExternalLibrary.open(config.libraryPath));
  final restored = await api.SharedPushState.restore(
    path: config.stateDirectory,
  );
  if (restored == null) {
    stdout.writeln(errorFrame(code: ProtocolCodes.engineUnavailable));
    exitCode = 3;
    return;
  }
  final (push, watcher) = restored;
  final (pollState, deskState) = api.dupDaemonDesk(state: push);
  // Take the CloudKit client from the state dupDaemonDesk returned: it consumes the
  // restored state, and handles read from the original are disposed with it. The
  // app itself reads the client from its desktop state for the same reason.
  final cloudMessagesClient = deskState.icloudServices?.cloudMessagesClient;

  final sends = <String, int>{}; // message id -> originating request id
  var sequence = 0;
  var stopping = false;

  stdout.writeln(
    readyFrame(
      engineVersion: engineVersion,
      canSend: true,
      canReadHistory: config.syncChats,
    ),
  );

  if (config.syncChats) {
    final client = cloudMessagesClient;
    if (client == null) {
      stderr.writeln('chat sync unavailable: no cloud messages client');
      stdout.writeln(
          historyCompleteFrame(conversations: 0, messages: 0, limited: true));
    } else {
      unawaited(_syncChats(client));
    }
  }

  // Explicit stop and stdin EOF (the parent daemon going away, which closes our
  // stdin) both end the process. Without the EOF path the sidecar survives as an
  // orphan and keeps owning the user's engine state with no daemon supervising it.
  void stop() {
    stopping = true;
    api.cancelPoll(cancel: deskState.cancelPoll);
  }

  final commands =
      stdin.transform(utf8.decoder).transform(const LineSplitter()).listen(
            (line) => _handleCommand(
              line: line,
              deskState: deskState,
              sends: sends,
              stop: stop,
            ),
            onDone: stop,
          );

  while (!stopping) {
    final api.PollResult result;
    try {
      result = await api.recvWait(watcher: watcher, state: pollState);
    } catch (error) {
      stderr.writeln('poll failed: $error');
      stdout.writeln(
        errorFrame(code: ProtocolCodes.engineUnavailable, retryable: true),
      );
      await Future<void>.delayed(const Duration(seconds: 1));
      continue;
    }
    await result.map(
      stop: (_) => stopping = true,
      cont: (value) async {
        final message = value.field0;
        if (message == null) return;
        sequence++;
        final payload = encodePushMessage(message);
        if (payload['variant'] == 'sendConfirm') {
          final uuid = payload['uuid'];
          final requestId = uuid is String ? sends.remove(uuid) : null;
          if (requestId != null) {
            stdout.writeln(
              sendConfirmFrame(
                requestId: requestId,
                messageId: uuid as String,
                accepted: payload['error'] == null,
                error: payload['error'] as String?,
              ),
            );
            return;
          }
        }
        stdout.writeln(inboundFrame(sequence: sequence, payload: payload));
      },
    );
    if (!stopping) {
      await Future<void>.delayed(const Duration(milliseconds: 100));
    }
  }

  await commands.cancel();
  stderr.writeln('sidecar stopped');
}

/// Build the conversation shape for a `conversation` frame from plain values, so
/// the mapping can be tested without constructing engine objects.
Map<String, Object?>? conversationFromChat({
  required String guid,
  required String chatIdentifier,
  required String? displayName,
  required List<String> participants,
}) {
  final resolved = guid.trim().isNotEmpty ? guid.trim() : chatIdentifier.trim();
  if (resolved.isEmpty) return null;
  final title = (displayName ?? '').trim();
  return <String, Object?>{
    'guid': resolved,
    'title': title.isEmpty ? null : title,
    'participants': participants
        .map((uri) => uri.trim())
        .where((uri) => uri.isNotEmpty)
        .toList(),
    'is_group': participants.length > 2,
  };
}

/// Page through the account's existing conversations and emit them.
///
/// Runs off the receive path so live messaging is unaffected. Bounded, and marks
/// the result limited rather than silently truncating.
Future<void> _syncChats(
  upstream.ArcCloudMessagesClientDefaultAnisetteProvider client,
) async {
  const maxPages = 200;
  const maxRows = 20000;
  Uint8List? token;
  var pages = 0;
  var rows = 0;
  var limited = false;
  final seen = <String>{};
  try {
    while (pages < maxPages && rows < maxRows) {
      final (next, page, status) = await api.syncChats(
        cloudMessagesClient: client,
        continuationToken: token,
      );
      pages++;
      if (page.isEmpty) break;
      for (final chat in page.values) {
        if (chat == null) continue;
        final mapped = conversationFromChat(
          guid: chat.guid,
          chatIdentifier: chat.chatIdentifier,
          displayName: chat.displayName,
          participants: chat.participants.map((p) => p.uri).toList(),
        );
        if (mapped == null) continue;
        if (!seen.add(mapped['guid']! as String)) continue;
        if (rows >= maxRows) {
          limited = true;
          break;
        }
        rows++;
        stdout.writeln(conversationFrame(sequence: rows, conversation: mapped));
      }
      if (limited) break;
      if (status != 1 || next.isEmpty) break;
      token = next;
    }
    if (pages >= maxPages || rows >= maxRows) limited = true;
  } catch (error) {
    stderr.writeln(
      'chat sync failed: ${error.runtimeType}: ${error.toString().split('\n').first}',
    );
    limited = true;
  }
  stderr.writeln(
      'chat sync complete: pages=$pages conversations=$rows limited=$limited');
  stdout.writeln(
    historyCompleteFrame(conversations: rows, messages: 0, limited: limited),
  );
}

Future<void> _handleCommand({
  required String line,
  required dynamic deskState,
  required Map<String, int> sends,
  required void Function() stop,
}) async {
  final Command command;
  try {
    command = decodeCommand(line);
  } on ProtocolException catch (error) {
    stderr.writeln('rejected command: ${error.code}');
    stdout.writeln(errorFrame(code: error.code));
    return;
  }

  switch (command) {
    case ShutdownCommand():
      stdout.writeln(shutdownAckFrame(requestId: command.requestId));
      stop();
    case SendCommand():
      try {
        final handles = await api.getHandles(state: deskState.client);
        final sender = chooseSender(handles, command.recipient);
        if (sender == null) {
          stdout.writeln(
            errorFrame(
              requestId: command.requestId,
              code: ProtocolCodes.noSenderHandle,
            ),
          );
          return;
        }
        final inst = buildMessage(
          recipient: command.recipient,
          sender: sender,
          text: command.text,
          chatGuid: command.chatGuid,
        );
        sends[inst.id] = command.requestId;
        final accepted = await api.send(
          state: deskState.client,
          local: deskState.localBroadcast,
          msg: inst,
        );
        stdout.writeln(
          sendConfirmFrame(
            requestId: command.requestId,
            messageId: inst.id,
            accepted: accepted,
          ),
        );
      } catch (error) {
        stderr.writeln('send failed: $error');
        stdout.writeln(
          errorFrame(
            requestId: command.requestId,
            code: ProtocolCodes.sendFailed,
            retryable: true,
          ),
        );
      }
  }
}
