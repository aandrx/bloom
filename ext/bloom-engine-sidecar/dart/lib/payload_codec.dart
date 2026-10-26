// Explicit encoder for engine push payloads.
//
// Design rule: structured engine values are encoded field by field. No structured
// value is ever rendered with toString(), because that silently produces
// "Instance of ..." and loses message content. Variants we do not model yet are
// emitted with an explicit `unsupported` marker naming the variant and the reason,
// which is a loud, testable signal rather than a silent loss.
//
// toString() appears only for enum-like and integer-wrapper fields, where it yields
// a stable name or decimal value rather than a type placeholder.

import 'dart:convert';
import 'dart:typed_data';

import 'upstream/api/api.dart' as api;

/// Marker used when a payload variant or nested value is identified but not modelled.
const String kUnsupported = 'unsupported';

const String _opaqueHandleReason =
    'carries opaque engine handles; not serializable outside the engine process';

Map<String, Object?> encodePushMessage(api.PushMessage value,
        {Map<int, String>? fetchTokens}) =>
    value.map(
      iMessage: (variant) => <String, Object?>{
        'variant': 'iMessage',
        'complete': true,
        'inst': encodeMessageInst(variant.field0, fetchTokens: fetchTokens),
      },
      sendConfirm: (variant) => <String, Object?>{
        'variant': 'sendConfirm',
        'complete': true,
        'uuid': variant.uuid,
        'error': variant.error,
      },
      registrationState: (variant) => <String, Object?>{
        'variant': 'registrationState',
        'complete': true,
        'state': encodeRegisterState(variant.field0),
      },
      twoFaAuthEvent: (variant) => <String, Object?>{
        'variant': 'twoFaAuthEvent',
        'complete': true,
        'value': variant.field0,
      },
      circleFinishEvent: (_) => <String, Object?>{
        'variant': 'circleFinishEvent',
        'complete': true,
      },
      beaconShared: (variant) => <String, Object?>{
        'variant': 'beaconShared',
        'complete': true,
        'sender': variant.sender,
        'beacon': variant.beacon,
        'attributes': encodeBeaconAttributes(variant.attributes),
      },
      newPhotostream: (variant) => <String, Object?>{
        'variant': 'newPhotostream',
        'complete': false,
        kUnsupported: 'newPhotostream',
        'reason': _opaqueHandleReason,
        'album': encodeAlbumMeta(variant.field0),
      },
      faceTime: (variant) => <String, Object?>{
        'variant': 'faceTime',
        'complete': false,
        kUnsupported: 'faceTime',
        'reason': 'FaceTime signalling not modelled yet',
        'message': encodeFtMessage(variant.field0),
      },
      statusUpdate: (variant) => <String, Object?>{
        'variant': 'statusUpdate',
        'complete': true,
        'message': encodeStatusKit(variant.field0),
      },
      idms: (variant) => <String, Object?>{
        'variant': 'idms',
        'complete': false,
        kUnsupported: 'idms',
        'reason': 'IDMS sign-in signalling not modelled yet',
        'message': encodeIdms(variant.field0),
      },
    );

Map<String, Object?> encodeMessageInst(api.MessageInst value,
        {Map<int, String>? fetchTokens}) =>
    <String, Object?>{
      'id': value.id,
      'sender': value.sender,
      'conversation': value.conversation == null
          ? null
          : encodeConversation(value.conversation!),
      'message': encodeMessage(value.message, fetchTokens: fetchTokens),
      'sent_timestamp_ms': value.sentTimestamp,
      'send_delivered': value.sendDelivered,
      'verification_failed': value.verificationFailed,
    };

Map<String, Object?> encodeConversation(api.ConversationData value) =>
    <String, Object?>{
      'participants': value.participants,
      'cv_name': value.cvName,
      'sender_guid': value.senderGuid,
      'after_guid': value.afterGuid,
    };

Map<String, Object?> _unmodelled(String name) => <String, Object?>{
      'type': name,
      'complete': false,
      kUnsupported: name,
      'reason': 'message variant not modelled yet',
    };

Map<String, Object?> encodeMessage(api.Message value,
        {Map<int, String>? fetchTokens}) =>
    value.map(
      message: (variant) => <String, Object?>{
        'type': 'normal',
        'complete': true,
        'value': encodeNormalMessage(variant.field0, fetchTokens: fetchTokens),
      },
      react: (variant) => <String, Object?>{
        'type': 'react',
        'complete': true,
        'to_uuid': variant.field0.toUuid,
        'to_part': variant.field0.toPart,
        'reaction': variant.field0.reaction.map(
          react: (r) => <String, Object?>{
            'kind': 'react',
            'reaction': r.reaction.map(
              heart: (_) => 'heart',
              like: (_) => 'like',
              dislike: (_) => 'dislike',
              laugh: (_) => 'laugh',
              emphasize: (_) => 'emphasize',
              question: (_) => 'question',
              emoji: (v) => v.field0,
              sticker: (_) => 'sticker',
            ),
            'enable': r.enable,
          },
          extension_: (r) => <String, Object?>{
            'kind': 'extension',
            'is_meta': r.isMeta,
            'body': encodeParts(r.body),
          },
        ),
        'to_text': variant.field0.toText,
      },
      unsend: (variant) => <String, Object?>{
        'type': 'unsend',
        'complete': true,
        'tuuid': variant.field0.tuuid,
        'edit_part': variant.field0.editPart,
      },
      edit: (variant) => <String, Object?>{
        'type': 'edit',
        'complete': true,
        'tuuid': variant.field0.tuuid,
        'edit_part': variant.field0.editPart,
        'new_parts': encodeParts(variant.field0.newParts),
      },
      renameMessage: (variant) => <String, Object?>{
        'type': 'rename',
        'complete': true,
        'new_name': variant.field0.newName,
      },
      changeParticipants: (variant) => <String, Object?>{
        'type': 'changeParticipants',
        'complete': true,
        'new_participants': variant.field0.newParticipants,
        'group_version': variant.field0.groupVersion,
      },
      delivered: (_) =>
          <String, Object?>{'type': 'delivered', 'complete': true},
      read: (_) => <String, Object?>{'type': 'read', 'complete': true},
      markUnread: (_) =>
          <String, Object?>{'type': 'markUnread', 'complete': true},
      typing: (variant) => <String, Object?>{
        'type': 'typing',
        'complete': true,
        'typing': variant.field0,
      },
      enableSmsActivation: (variant) => <String, Object?>{
        'type': 'enableSmsActivation',
        'complete': true,
        'value': variant.field0,
      },
      smsConfirmSent: (variant) => <String, Object?>{
        'type': 'smsConfirmSent',
        'complete': true,
        'value': variant.field0,
      },
      messageReadOnDevice: (_) => <String, Object?>{
        'type': 'messageReadOnDevice',
        'complete': true,
      },
      peerCacheInvalidate: (_) => <String, Object?>{
        'type': 'peerCacheInvalidate',
        'complete': true,
      },
      unschedule: (_) =>
          <String, Object?>{'type': 'unschedule', 'complete': true},
      notifyAnyways: (_) => <String, Object?>{
        'type': 'notifyAnyways',
        'complete': true,
      },
      iconChange: (variant) => _unmodelled('iconChange'),
      updateExtension: (variant) => _unmodelled('updateExtension'),
      error: (variant) => _unmodelled('error'),
      moveToRecycleBin: (variant) => _unmodelled('moveToRecycleBin'),
      recoverChat: (variant) => _unmodelled('recoverChat'),
      permanentDelete: (variant) => _unmodelled('permanentDelete'),
      updateProfile: (variant) => _unmodelled('updateProfile'),
      updateProfileSharing: (variant) => _unmodelled('updateProfileSharing'),
      shareProfile: (variant) => _unmodelled('shareProfile'),
      setTranscriptBackground: (variant) =>
          _unmodelled('setTranscriptBackground'),
    );

Map<String, Object?> encodeNormalMessage(api.NormalMessage value,
        {Map<int, String>? fetchTokens}) =>
    <String, Object?>{
      'parts': encodeParts(value.parts, fetchTokens: fetchTokens),
      'service': encodeService(value.service),
      'effect': value.effect,
      'reply_guid': value.replyGuid,
      'reply_part': value.replyPart,
      'subject': value.subject,
      'voice': value.voice,
      'has_app': value.app != null,
      'has_link_meta': value.linkMeta != null,
      'has_embedded_profile': value.embeddedProfile != null,
      'scheduled': value.scheduled != null,
    };

Map<String, Object?> encodeService(api.MessageType value) => value.map(
      iMessage: (_) => <String, Object?>{'type': 'iMessage', 'complete': true},
      sms: (variant) => <String, Object?>{
        'type': 'sms',
        'complete': true,
        'is_phone': variant.isPhone,
        'using_number': variant.usingNumber,
        'from_handle': variant.fromHandle,
      },
    );

List<Map<String, Object?>> encodeParts(api.MessageParts value,
        {Map<int, String>? fetchTokens}) =>
    <Map<String, Object?>>[
      for (var position = 0; position < value.field0.length; position++)
        encodeIndexedPart(
          value.field0[position],
          fetchToken: fetchTokens?[position],
        ),
    ];

Map<String, Object?> encodeIndexedPart(api.IndexedMessagePart value,
        {String? fetchToken}) =>
    <String, Object?>{
      'idx': value.idx,
      'part': value.part_.map(
        text: (variant) => <String, Object?>{
          'type': 'text',
          'text': variant.field0,
          'format': variant.field1.map(
            flags: (format) => <String, Object?>{
              'type': 'flags',
              'bold': format.field0.bold,
              'italic': format.field0.italic,
              'underline': format.field0.underline,
              'strikethrough': format.field0.strikethrough,
            },
            effect: (format) => <String, Object?>{
              'type': 'effect',
              'complete': false,
              kUnsupported: 'textEffect',
              'reason': 'text effect payload not modelled yet',
            },
          ),
        },
        attachment: (variant) => <String, Object?>{
          'type': 'attachment',
          'complete': true,
          'part': variant.field0.part_,
          'name': variant.field0.name,
          'uti_type': variant.field0.utiType,
          'mime': variant.field0.mime,
          if (fetchToken != null) 'fetch_token': fetchToken,
          'attachment_type': variant.field0.aType.map(
            inline: (v) => <String, Object?>{
              'kind': 'inline',
              'byte_length': v.field0.length,
            },
            mmcs: (v) => <String, Object?>{
              'kind': 'mmcs',
              'object': v.field0.object,
              'url': v.field0.url,
              'size': v.field0.size,
              'signature_base64': base64.encode(v.field0.signature),
              'key_base64': base64.encode(v.field0.key),
            },
          ),
          'iris': variant.field0.iris,
        },
        mention: (variant) => <String, Object?>{
          'type': 'mention',
          'text': variant.field0,
          'handle': variant.field1,
        },
        object: (variant) => <String, Object?>{
          'type': 'object',
          'value': variant.field0,
        },
      ),
    };

Map<String, Object?> encodeRegisterState(api.RegisterState value) => value.map(
      registered: (variant) => <String, Object?>{
        'state': 'registered',
        'next_s': variant.nextS.toString(),
      },
      registering: (variant) => <String, Object?>{'state': 'registering'},
      failed: (variant) => <String, Object?>{
        'state': 'failed',
        'error': variant.error,
        'retry_wait': variant.retryWait?.toString(),
      },
    );

Map<String, Object?> encodeBeaconAttributes(api.BeaconAttributes value) =>
    <String, Object?>{
      'name': value.name,
      'role_id': value.roleId.toString(),
      'emoji': value.emoji,
      'system_version': value.systemVersion,
      'serial_number': value.serialNumber,
    };

Map<String, Object?> encodeAlbumMeta(api.SharedAlbum value) =>
    <String, Object?>{
      'name': value.name,
      'fullname': value.fullname,
      'email': value.email,
      'album_guid': value.albumguid,
      'sharing_type': value.sharingtype,
      'subscription_date': value.subscriptiondate,
      'album_location': value.albumlocation,
      'asset_count': value.assets.length,
      'delete': value.delete,
    };

Map<String, Object?> encodeFtMessage(api.FTMessage value) => value.map(
      letMeInRequest: (_) => <String, Object?>{'variant': 'letMeInRequest'},
      linkChanged: (_) => <String, Object?>{'variant': 'linkChanged'},
      joinEvent: (_) => <String, Object?>{'variant': 'joinEvent'},
      addMembers: (_) => <String, Object?>{'variant': 'addMembers'},
      removeMembers: (_) => <String, Object?>{'variant': 'removeMembers'},
      leaveEvent: (_) => <String, Object?>{'variant': 'leaveEvent'},
      ring: (_) => <String, Object?>{'variant': 'ring'},
      decline: (_) => <String, Object?>{'variant': 'decline'},
      respondedElsewhere: (_) =>
          <String, Object?>{'variant': 'respondedElsewhere'},
    );

Map<String, Object?> encodeStatusKit(api.StatusKitMessage value) => value.map(
      statusChanged: (variant) => <String, Object?>{
        'variant': 'statusChanged',
        'user': variant.user,
        'mode': variant.mode,
        'allowed': variant.allowed,
      },
    );

Map<String, Object?> encodeIdms(api.IdmsMessage value) => value.map(
      requestedSignIn: (_) => <String, Object?>{'variant': 'requestedSignIn'},
      teardownSignIn: (_) => <String, Object?>{'variant': 'teardownSignIn'},
      circleRequest: (_) => <String, Object?>{'variant': 'circleRequest'},
    );

/// Concatenate text parts of an encoded inbound message, if any.
String? textFromEncoded(Map<String, Object?> encoded) {
  if (encoded['variant'] != 'iMessage') return null;
  final inst = encoded['inst'];
  if (inst is! Map) return null;
  final message = inst['message'];
  if (message is! Map || message['type'] != 'normal') return null;
  final normal = message['value'];
  if (normal is! Map) return null;
  final parts = normal['parts'];
  if (parts is! List) return null;
  final buffer = StringBuffer();
  for (final part in parts) {
    if (part is! Map) continue;
    final inner = part['part'];
    if (inner is! Map || inner['type'] != 'text') continue;
    final text = inner['text'];
    if (text is String) buffer.write(text);
  }
  final result = buffer.toString();
  return result.isEmpty ? null : result;
}

/// Encode raw bytes for transport without lossy conversion.
Map<String, Object?> encodeBytes(Uint8List bytes) => <String, Object?>{
      'bytes_base64': base64.encode(bytes),
      'length': bytes.length,
    };
