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

import '../lib/attachment_cache.dart';
import '../lib/payload_codec.dart';
import '../lib/protocol.dart';
import '../lib/send_coordinator.dart';
import '../lib/single_flight.dart';
import '../lib/upstream/api/api.dart' as api;
import '../lib/upstream/lib.dart' as upstream;
import '../lib/upstream/frb_generated.dart';

const String engineVersion = 'openbubbles-v1.15.0+205';

void writeProtocolFrame(String line) => stdout.writeln(wireLine(line));

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

/// The engine's handle form: `mailto:` for an address, `tel:` for a number.
///
/// The daemon already sends this form. Re-prefixing an already prefixed handle
/// produces `mailto:mailto:...`, which resolves to no IDS target and fails the
/// send with "No valid IDS targets for message", so the prefix is only added
/// when it is missing.
String engineHandle(String value) {
  if (value.startsWith('mailto:') || value.startsWith('tel:')) return value;
  return value.contains('@') ? 'mailto:$value' : 'tel:$value';
}

/// Build the engine's ordered target list, followed by the account sender.
List<String> engineConversationParticipants({
  required List<String> recipients,
  required String sender,
}) {
  if (recipients.isEmpty ||
      recipients.any((recipient) => recipient.trim().isEmpty) ||
      sender.trim().isEmpty) {
    throw ArgumentError('recipients and sender must be non-empty');
  }

  final engineSender = engineHandle(sender);
  final targets = <String>{};
  for (final recipient in recipients) {
    final target = engineHandle(recipient);
    if (target != engineSender) targets.add(target);
  }
  if (targets.isEmpty) {
    throw ArgumentError('at least one recipient must differ from the sender');
  }
  return <String>[...targets, engineSender];
}

/// Create the exact conversation passed to `api.newMsg`, preserving group IDs.
api.ConversationData engineConversationData({
  required List<String> recipients,
  required String sender,
  String? chatGuid,
}) =>
    api.ConversationData(
      participants: engineConversationParticipants(
        recipients: recipients,
        sender: sender,
      ),
      cvName: null,
      senderGuid: chatGuid ?? _uuid4(),
      afterGuid: null,
    );

/// Pick a sending address whose scheme matches any recipient's.
String? chooseSender(List<String> handles, List<String> recipients) {
  if (handles.isEmpty || recipients.isEmpty) return null;
  final schemes =
      recipients.map((value) => engineHandle(value).split(':').first).toSet();
  for (final handle in handles) {
    if (schemes.contains(handle.split(':').first)) return handle;
  }
  return handles.first;
}

/// Build a sendable message through the engine's own constructor.
///
/// `api.newMsg` runs `MessageInst::new`, which derives the IDS target list from
/// the conversation participants. Constructing `MessageInst` by hand leaves
/// `target` null and the engine then rejects the send with
/// "No valid IDS targets for message".
Future<api.MessageInst> buildMessage({
  required List<String> recipients,
  required String sender,
  required String text,
  String? chatGuid,
}) {
  final engineSender = engineHandle(sender);
  return api.newMsg(
    conversation: engineConversationData(
      recipients: recipients,
      sender: engineSender,
      chatGuid: chatGuid,
    ),
    sender: engineSender,
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
    writeProtocolFrame(errorFrame(code: ProtocolCodes.engineUnavailable));
    exitCode = 3;
    return;
  }
  final (push, watcher) = restored;
  final attachmentRegistry = AttachmentRegistry();
  final attachmentCache = AttachmentCache(
    root: AttachmentCache.resolveDefaultRoot(),
    registry: attachmentRegistry,
  );
  final (pollState, deskState) = api.dupDaemonDesk(state: push);
  // Take the CloudKit client from the state dupDaemonDesk returned: it consumes the
  // restored state, and handles read from the original are disposed with it. The
  // app itself reads the client from its desktop state for the same reason.
  final cloudMessagesClient = deskState.icloudServices?.cloudMessagesClient;

  late final SendCoordinator sends = SendCoordinator(writeProtocolFrame);
  var sequence = 0;
  var stopping = false;

  // A failure here must not stop the sidecar from becoming ready: the daemon treats
  // a missing ready frame as an engine failure and the whole client refuses to
  // start. Handles only affect how the daemon labels our own messages, so an empty
  // list degrades that label rather than the session.
  var handles = const <String>[];
  try {
    handles = await api.getHandles(state: deskState.client);
  } catch (_) {
    stderr.writeln(
      'could not read account handles; own messages may be labelled incorrectly',
    );
  }
  // The ready frame owns the capability wiring, so chat sync cannot advertise
  // history that no command can produce.
  writeProtocolFrame(
    sidecarReadyFrame(engineVersion: engineVersion, handles: handles),
  );

  final syncFlight = SingleFlight<_SyncResult>();
  Future<_SyncResult> runSync(
    upstream.ArcCloudMessagesClientDefaultAnisetteProvider client,
  ) =>
      syncFlight.run(() => _syncChats(client));

  if (config.syncChats) {
    final client = cloudMessagesClient;
    if (client == null) {
      stderr.writeln('chat sync unavailable: no cloud messages client');
    } else {
      // Request id zero is reserved for an unsolicited startup completion. The
      // daemon uses it to publish the ordered conversation-list refresh event.
      unawaited(runSync(client).then((result) {
        writeProtocolFrame(syncResultFrame(
          requestId: 0,
          status: result.status,
          conversations: result.count,
        ));
      }));
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
              sync: (requestId) async {
                final client = cloudMessagesClient;
                if (client == null) {
                  writeProtocolFrame(syncResultFrame(
                    requestId: requestId,
                    status: 'failed',
                    conversations: 0,
                  ));
                  return;
                }
                final result = await runSync(client);
                writeProtocolFrame(syncResultFrame(
                  requestId: requestId,
                  status: result.status,
                  conversations: result.count,
                ));
              },
              stop: stop,
              attachments: attachmentCache,
              // `dupDaemonDesk` takes ownership of every opaque handle on the
              // restored state, so `push.conn` is disposed from here on. The
              // returned desk state carries the live connection.
              aps: deskState.conn,
            ),
            onDone: stop,
          );

  while (!stopping) {
    final api.PollResult result;
    try {
      result = await api.recvWait(watcher: watcher, state: pollState);
    } catch (error) {
      stderr.writeln('poll failed: $error');
      writeProtocolFrame(
        errorFrame(code: ProtocolCodes.engineUnavailable, retryable: true),
      );
      await Future<void>.delayed(const Duration(seconds: 1));
      continue;
    }
    final receivedAt = Stopwatch()..start();
    await result.map(
      stop: (_) => stopping = true,
      cont: (value) async {
        final message = value.field0;
        if (message == null) return;
        sequence++;
        final fetchTokens =
            registerInboundAttachments(message, attachmentRegistry);
        final payload = encodePushMessage(message, fetchTokens: fetchTokens);
        if (payload['variant'] == 'sendConfirm') {
          final uuid = payload['uuid'];
          if (uuid is String &&
              sends.completePush(
                messageId: uuid,
                error: payload['error'] as String?,
              )) {
            return;
          }
        }
        writeProtocolFrame(inboundFrame(sequence: sequence, payload: payload));
        if (Platform.environment['BLOOM_TIMING_DIAGNOSTICS'] == '1') {
          stderr.writeln(
            'bloom timing sidecar frame_encode_us=${receivedAt.elapsedMicroseconds} sequence=$sequence',
          );
        }
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
class _SyncResult {
  const _SyncResult(this.status, this.count);
  final String status;
  final int count;
}

const _maxSyncPages = 200;
const _maxSyncRows = 20000;

bool conversationSyncHasMore({
  required int status,
  required bool pageIsEmpty,
  required bool hasContinuation,
}) =>
    !pageIsEmpty && status == 1 && hasContinuation;

bool conversationSyncLimitReached({
  required int pages,
  required int rows,
  required bool hasMore,
}) =>
    (pages >= _maxSyncPages && hasMore) || (rows >= _maxSyncRows && hasMore);

Future<_SyncResult> _syncChats(
  upstream.ArcCloudMessagesClientDefaultAnisetteProvider client,
) async {
  Uint8List? token;
  var pages = 0;
  var rows = 0;
  var limited = false;
  var successfulListing = false;
  var hasMore = false;
  final seen = <String>{};
  try {
    while (pages < _maxSyncPages && rows < _maxSyncRows) {
      final (next, page, status) = await api.syncChats(
        cloudMessagesClient: client,
        continuationToken: token,
      );
      pages++;
      successfulListing = true;
      hasMore = conversationSyncHasMore(
        status: status,
        pageIsEmpty: page.isEmpty,
        hasContinuation: next.isNotEmpty,
      );
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
        if (rows >= _maxSyncRows) {
          limited = true;
          break;
        }
        rows++;
        writeProtocolFrame(
            conversationFrame(sequence: rows, conversation: mapped));
      }
      if (limited) break;
      if (!hasMore) break;
      token = next;
    }
    limited = limited ||
        conversationSyncLimitReached(
          pages: pages,
          rows: rows,
          hasMore: hasMore,
        );
  } catch (error) {
    stderr.writeln(
      'chat sync failed: ${error.runtimeType}: ${error.toString().split('\n').first}',
    );
    limited = successfulListing;
  }
  stderr.writeln(
    'chat sync complete: pages=$pages conversations=$rows limited=$limited',
  );
  return _SyncResult(
    !successfulListing
        ? 'failed'
        : limited
            ? 'limited'
            : 'complete',
    rows,
  );
}

Future<void> _handleCommand({
  required String line,
  required dynamic deskState,
  required SendCoordinator sends,
  required Future<void> Function(int) sync,
  required AttachmentCache attachments,
  required Object? aps,
  required void Function() stop,
}) async {
  final Command command;
  try {
    command = decodeCommand(line);
  } on ProtocolException catch (error) {
    stderr.writeln('rejected command: ${error.code}');
    writeProtocolFrame(rejectedCommandFrame(line, error.code));
    return;
  }

  switch (command) {
    case SyncConversationsCommand():
      await sync(command.requestId);
    case AttachmentFetchCommand():
      try {
        final fetched = await attachments.fetch(command.token, aps: aps);
        writeProtocolFrame(attachmentFetchedFrame(
          requestId: command.requestId,
          path: fetched.file.path,
          bytes: fetched.bytes,
        ));
      } on AttachmentFailure catch (error) {
        final failure = error is AttachmentDownloadFailure ? error : null;
        final detail = failure == null
            ? ''
            : ' stage=${failure.stage}'
                '${failure.cause == null ? '' : ' cause=${failure.cause}'}';
        stderr.writeln('attachment fetch failed: ${error.code}$detail');
        writeProtocolFrame(errorFrame(
          requestId: command.requestId,
          code: _attachmentErrorCode(error),
        ));
      } catch (error) {
        stderr.writeln(
          'attachment fetch failed: failed stage=unexpected '
          'cause=${downloadErrorCause(error)}',
        );
        writeProtocolFrame(errorFrame(
          requestId: command.requestId,
          code: ProtocolCodes.failed,
        ));
      }
    case ShutdownCommand():
      writeProtocolFrame(shutdownAckFrame(requestId: command.requestId));
      stop();
    case SendCommand():
      String? messageId;
      try {
        final handles = await api.getHandles(state: deskState.client);
        final sender = chooseSender(handles, command.recipients);
        if (sender == null) {
          writeProtocolFrame(
            errorFrame(
              requestId: command.requestId,
              code: ProtocolCodes.noSenderHandle,
            ),
          );
          return;
        }
        final inst = await buildMessage(
          recipients: command.recipients,
          sender: sender,
          text: command.text,
          chatGuid: command.chatGuid,
        );
        messageId = inst.id;
        sends.register(inst.id, command.requestId);
        final stillRunning = await api.send(
          state: deskState.client,
          local: deskState.localBroadcast,
          msg: inst,
        );
        // `false` means synchronous completion, not rejection. The coordinator
        // suppresses this if a confirmation push already consumed the request.
        sends.sendReturned(messageId: inst.id, stillRunning: stillRunning);
      } catch (error) {
        if (messageId != null) sends.discard(messageId);
        stderr.writeln('send failed: $error');
        writeProtocolFrame(
          errorFrame(
            requestId: command.requestId,
            code: ProtocolCodes.sendFailed,
            retryable: true,
          ),
        );
      }
  }
}

String _attachmentErrorCode(AttachmentFailure failure) {
  if (failure is AttachmentInvalidTokenFailure)
    return ProtocolCodes.invalidToken;
  if (failure is AttachmentTooLargeFailure) return ProtocolCodes.tooLarge;
  if (failure is AttachmentTimeoutFailure) return ProtocolCodes.timeout;
  if (failure is AttachmentUnsafeRootFailure) return ProtocolCodes.unavailable;
  return ProtocolCodes.failed;
}

/// Register every attachment part of one inbound normal message by physical
/// position, returning the token map the codec injects into those parts.
Map<int, String> registerInboundAttachments(
  api.PushMessage message,
  AttachmentRegistry registry,
) {
  if (message is! api.PushMessage_IMessage) return const <int, String>{};
  final inst = message.field0;
  if (inst.id.isEmpty) return const <int, String>{};
  final encoded = inst.message;
  if (encoded is! api.Message_Message) return const <int, String>{};
  final parts = encoded.field0.parts.field0;
  final tokens = <int, String>{};
  for (var position = 0; position < parts.length; position++) {
    final part = parts[position].part_;
    if (part is api.MessagePart_Attachment) {
      tokens[position] = registry.register(
        messageId: inst.id,
        partIndex: position,
        attachment: part.field0,
      );
    }
  }
  return tokens;
}
