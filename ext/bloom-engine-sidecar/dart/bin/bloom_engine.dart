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

import 'dart:convert';
import 'dart:io';
import 'dart:math';

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated_io.dart';

import '../lib/payload_codec.dart';
import '../lib/protocol.dart';
import '../lib/upstream/api/api.dart' as api;
import '../lib/upstream/frb_generated.dart';

const String engineVersion = 'openbubbles-v1.15.0+205';

class SidecarConfig {
  SidecarConfig({required this.stateDirectory, required this.libraryPath});

  final String stateDirectory;
  final String libraryPath;
}

SidecarConfig? parseConfig(List<String> arguments) {
  final state = _option(arguments, '--state') ??
      Platform.environment['BLOOM_ENGINE_STATE'];
  final library = _option(arguments, '--library') ??
      Platform.environment['BLOOM_ENGINE_LIBRARY'];
  if (state == null || state.isEmpty) {
    stderr.writeln(
        'error: explicit state directory required (--state or BLOOM_ENGINE_STATE)');
    return null;
  }
  if (library == null || library.isEmpty) {
    stderr.writeln(
        'error: engine library required (--library or BLOOM_ENGINE_LIBRARY)');
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
  return SidecarConfig(stateDirectory: state, libraryPath: library);
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
}) {
  final scheme = recipient.contains('@') ? 'mailto' : 'tel';
  return api.MessageInst(
    id: _uuid4(),
    sender: sender,
    conversation: api.ConversationData(
      participants: <String>['$scheme:$recipient', sender],
      cvName: null,
      senderGuid: _uuid4(),
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
    stderr
        .writeln('error: OpenBubbles is running; refusing shared-state access');
    exitCode = 2;
    return;
  }

  await RustLib.init(externalLibrary: ExternalLibrary.open(config.libraryPath));
  final restored =
      await api.SharedPushState.restore(path: config.stateDirectory);
  if (restored == null) {
    stdout.writeln(errorFrame(code: ProtocolCodes.engineUnavailable));
    exitCode = 3;
    return;
  }
  final (push, watcher) = restored;
  final (pollState, deskState) = api.dupDaemonDesk(state: push);

  final sends = <String, int>{}; // message id -> originating request id
  var sequence = 0;
  var stopping = false;

  stdout.writeln(readyFrame(
    engineVersion: engineVersion,
    canSend: true,
    canReadHistory: false,
  ));

  final commands = stdin
      .transform(utf8.decoder)
      .transform(const LineSplitter())
      .listen((line) => _handleCommand(
            line: line,
            deskState: deskState,
            sends: sends,
            stop: () {
              stopping = true;
              api.cancelPoll(cancel: deskState.cancelPoll);
            },
          ));

  while (!stopping) {
    final api.PollResult result;
    try {
      result = await api.recvWait(watcher: watcher, state: pollState);
    } catch (error) {
      stderr.writeln('poll failed: $error');
      stdout.writeln(
          errorFrame(code: ProtocolCodes.engineUnavailable, retryable: true));
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
            stdout.writeln(sendConfirmFrame(
              requestId: requestId,
              messageId: uuid as String,
              accepted: payload['error'] == null,
              error: payload['error'] as String?,
            ));
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
          stdout.writeln(errorFrame(
            requestId: command.requestId,
            code: ProtocolCodes.noSenderHandle,
          ));
          return;
        }
        final inst = buildMessage(
          recipient: command.recipient,
          sender: sender,
          text: command.text,
        );
        sends[inst.id] = command.requestId;
        final accepted = await api.send(
          state: deskState.client,
          local: deskState.localBroadcast,
          msg: inst,
        );
        stdout.writeln(sendConfirmFrame(
          requestId: command.requestId,
          messageId: inst.id,
          accepted: accepted,
        ));
      } catch (error) {
        stderr.writeln('send failed: $error');
        stdout.writeln(errorFrame(
          requestId: command.requestId,
          code: ProtocolCodes.sendFailed,
          retryable: true,
        ));
      }
  }
}
