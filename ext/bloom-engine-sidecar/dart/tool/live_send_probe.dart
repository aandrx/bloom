// Guarded live engine probe: upload one small file and send it from one of the
// account's own handles to another. Both endpoints are the user's own account;
// no third party is contacted. Prints structural fields only, never handles,
// message ids or paths.
import 'dart:convert';
import 'dart:io';

import 'package:flutter_rust_bridge/flutter_rust_bridge_for_generated_io.dart';

import '../lib/upstream/api/api.dart' as api;
import '../lib/upstream/frb_generated.dart';

// Printed only as a bounded code; exception text can contain handles.
String classify(Object error) {
  final text = error.toString();
  if (text.contains('No valid IDS targets')) return 'no_valid_targets';
  if (text.contains('not registered') || text.contains('NotRegistered')) {
    return 'not_registered';
  }
  if (text.contains('Failed to generate resource')) return 'resource_failure';
  if (text.contains('timed out') || text.contains('timeout')) return 'timeout';
  if (text.contains('unregistered')) return 'unregistered';
  return 'unclassified';
}

Future<void> main(List<String> arguments) async {
  if (Platform.environment['BLOOM_LIVE_ATTACHMENT_SEND_ACK'] !=
      'one-shot-authorized') {
    stderr.writeln('live attachment send probe is not authorized');
    exitCode = 2;
    return;
  }
  if (arguments.length != 3) {
    stderr.writeln('usage: live_send_probe.dart <state> <library> <file>');
    exitCode = 2;
    return;
  }
  final statePath = arguments[0];
  final libraryPath = arguments[1];
  final filePath = arguments[2];
  final file = File(filePath);
  if (!file.existsSync()) {
    print(jsonEncode(<String, Object?>{'result': 'invalid_file'}));
    exitCode = 2;
    return;
  }
  final stat = file.statSync();
  if (stat.type != FileSystemEntityType.file) {
    print(jsonEncode(<String, Object?>{'result': 'invalid_file'}));
    exitCode = 2;
    return;
  }
  if (stat.size <= 0 || stat.size > 1024 * 1024) {
    print(jsonEncode(<String, Object?>{'result': 'invalid_size'}));
    exitCode = 2;
    return;
  }

  await RustLib.init(externalLibrary: ExternalLibrary.open(libraryPath));
  final restored = await api.SharedPushState.restore(path: statePath);
  if (restored == null) {
    print(jsonEncode(<String, Object?>{'result': 'state_unavailable'}));
    exitCode = 3;
    return;
  }
  final (state, _) = restored;
  final handles = await api.getHandles(state: state.client);
  final mailto = handles.where((handle) => handle.startsWith('mailto:'));
  final tel = handles.where((handle) => handle.startsWith('tel:'));
  final summary = <String, Object?>{
    'result': 'no_self_pair',
    'handles': handles.length,
    'mailto': mailto.length,
    'tel': tel.length,
    'bytes': stat.size,
  };
  if (mailto.isEmpty || tel.isEmpty) {
    print(jsonEncode(summary));
    exitCode = 4;
    return;
  }
  final recipient = mailto.first;
  final sender = tel.first;

  var uploadEvents = 0;
  api.Attachment? uploaded;
  try {
    await for (final event in api.uploadAttachment(
      aps: state.conn,
      path: filePath,
      mime: 'text/plain',
      uti: 'public.plain-text',
      name: 'bloom-live-attachment-test.txt',
    )) {
      uploadEvents++;
      if (event.attachment != null) uploaded = event.attachment;
    }
  } catch (error) {
    print(jsonEncode(<String, Object?>{
      ...summary,
      'result': 'upload_failed',
      'error_type': error.runtimeType.toString(),
    }));
    exitCode = 5;
    return;
  }
  if (uploaded == null) {
    print(jsonEncode(<String, Object?>{
      ...summary,
      'result': 'upload_missing',
      'upload_events': uploadEvents,
    }));
    exitCode = 5;
    return;
  }

  api.MessageInst inst;
  try {
    inst = await api.newMsg(
      conversation: api.ConversationData(
        participants: <String>[recipient, sender],
      ),
      sender: sender,
      message: api.Message.message(
        api.NormalMessage(
          parts: api.MessageParts(
            field0: <api.IndexedMessagePart>[
              api.IndexedMessagePart(
                part_: api.MessagePart.attachment(uploaded),
              ),
            ],
          ),
          service: const api.MessageType.iMessage(),
          voice: false,
        ),
      ),
    );
  } catch (error) {
    print(jsonEncode(<String, Object?>{
      ...summary,
      'result': 'build_failed',
      'error_type': error.runtimeType.toString(),
    }));
    exitCode = 5;
    return;
  }

  final result = <String, Object?>{
    'result': 'send_call_returned',
    'handles': handles.length,
    'mailto': mailto.length,
    'tel': tel.length,
    'upload_events': uploadEvents,
    'upload_attached': true,
    'bytes': stat.size,
  };
  try {
    final stillRunning = await api.send(
      state: state.client,
      local: state.localBroadcast,
      msg: inst,
    );
    result['still_running'] = stillRunning;
  } catch (error) {
    result['result'] = 'send_failed';
    result['error_type'] = error.runtimeType.toString();
    result['error_code'] = classify(error);
    print(jsonEncode(result));
    exitCode = 6;
    return;
  }
  // Keep the engine process alive briefly so an asynchronous send can finish.
  await Future<void>.delayed(const Duration(seconds: 15));
  print(jsonEncode(result));
}
