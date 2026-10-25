// Encoder tests. These run offline: every engine object is constructed locally, so
// no engine library and no account state are involved.

import 'dart:convert';
import 'dart:typed_data';

import 'package:bloom_engine_sidecar/payload_codec.dart';
import 'package:bloom_engine_sidecar/upstream/api/api.dart' as api;
import 'package:test/test.dart';

api.MMCSFile _mmcs() => api.MMCSFile(
      signature: Uint8List.fromList(<int>[1, 2, 3]),
      object: 'object',
      url: 'https://example.invalid/file',
      key: Uint8List.fromList(<int>[4, 5, 6]),
      size: 128,
    );

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

api.MessageInst fullInstance({
  String id = 'message-uuid',
  String sender = 'tel:+15550000001',
}) =>
    api.MessageInst(
      id: id,
      sender: sender,
      conversation: api.ConversationData(
        participants: <String>[sender, 'mailto:self@example.invalid'],
        cvName: null,
        senderGuid: 'iMessage;-;$sender',
        afterGuid: null,
      ),
      message: api.Message.message(
        api.NormalMessage(
          parts: api.MessageParts(
            field0: <api.IndexedMessagePart>[
              api.IndexedMessagePart(part_: textPart('hello'), idx: 0),
              api.IndexedMessagePart(
                part_: api.MessagePart.attachment(
                  api.Attachment(
                    aType: api.AttachmentType.mmcs(_mmcs()),
                    part_: 1,
                    utiType: 'public.jpeg',
                    mime: 'image/jpeg',
                    name: 'photo.jpg',
                    iris: false,
                  ),
                ),
                idx: 1,
              ),
              api.IndexedMessagePart(
                part_:
                    const api.MessagePart.mention('@bob', 'tel:+15550000002'),
                idx: 2,
              ),
            ],
          ),
          service: const api.MessageType.iMessage(),
          voice: false,
        ),
      ),
      sentTimestamp: 1700000000000,
      sendDelivered: true,
      verificationFailed: false,
    );

/// The contract every encoded payload must satisfy: it names its variant and either
/// carries complete content or an explicit unsupported marker. Silent degradation
/// such as "Instance of ..." must never appear.
void expectCompleteOrMarked(Map<String, Object?> encoded) {
  expect(
    encoded['variant'],
    isA<String>(),
    reason: 'payload must name its variant',
  );
  final complete = encoded['complete'];
  expect(complete, isA<bool>(), reason: 'payload must declare completeness');
  if (complete == false) {
    expect(
      encoded[kUnsupported],
      isNotNull,
      reason: 'incomplete payloads must carry an explicit marker',
    );
    expect(
      encoded['reason'],
      isNotNull,
      reason: 'incomplete payloads must explain why',
    );
  }
  expect(jsonEncode(encoded), isNot(contains('Instance of')));
}

Map<String, Object?> messageBody(Map<String, Object?> encoded) =>
    (encoded['inst']! as Map)['message']! as Map<String, Object?>;

void main() {
  group('inbound iMessage', () {
    test('encodes identity, participants, service and every part', () {
      final encoded = encodePushMessage(
        api.PushMessage.iMessage(fullInstance()),
      );

      expectCompleteOrMarked(encoded);
      expect(encoded['complete'], isTrue);

      final inst = encoded['inst']! as Map<String, Object?>;
      expect(inst['id'], 'message-uuid');
      expect(inst['sender'], 'tel:+15550000001');
      expect(inst['sent_timestamp_ms'], 1700000000000);
      expect(inst['send_delivered'], isTrue);

      final conversation = inst['conversation']! as Map<String, Object?>;
      expect(conversation['participants'], <String>[
        'tel:+15550000001',
        'mailto:self@example.invalid',
      ]);
      expect(conversation['sender_guid'], 'iMessage;-;tel:+15550000001');

      final message = inst['message']! as Map<String, Object?>;
      expect(message['type'], 'normal');
      final normal = message['value']! as Map<String, Object?>;
      expect((normal['service']! as Map)['type'], 'iMessage');

      final parts = normal['parts']! as List;
      expect(parts, hasLength(3));
      final first = (parts[0] as Map)['part']! as Map;
      expect(first['type'], 'text');
      expect(first['text'], 'hello');
      final second = (parts[1] as Map)['part']! as Map;
      expect(second['type'], 'attachment');
      expect(second['name'], 'photo.jpg');
      expect(second['mime'], 'image/jpeg');
      final attachmentType = second['attachment_type']! as Map;
      expect(attachmentType['kind'], 'mmcs');
      expect(attachmentType['object'], 'object');
      expect(attachmentType['size'], 128);
      final third = (parts[2] as Map)['part']! as Map;
      expect(third['type'], 'mention');
      expect(third['handle'], 'tel:+15550000002');
    });

    test('distinct messages stay distinct', () {
      final first = jsonEncode(
        encodePushMessage(
          api.PushMessage.iMessage(fullInstance(id: 'uuid-one')),
        ),
      );
      final second = jsonEncode(
        encodePushMessage(
          api.PushMessage.iMessage(fullInstance(id: 'uuid-two')),
        ),
      );
      expect(first, isNot(second));
    });

    test('plain text is recoverable from the encoded payload', () {
      final encoded = encodePushMessage(
        api.PushMessage.iMessage(fullInstance()),
      );
      expect(textFromEncoded(encoded), 'hello');
    });

    test('a message with no text parts yields no text', () {
      final attachmentOnly = api.MessageInst(
        id: 'uuid-attachment',
        sender: 'tel:+15550000001',
        conversation: api.ConversationData(
          participants: <String>['tel:+15550000001'],
          cvName: null,
          senderGuid: null,
          afterGuid: null,
        ),
        message: api.Message.message(
          api.NormalMessage(
            parts: api.MessageParts(
              field0: <api.IndexedMessagePart>[
                api.IndexedMessagePart(
                  part_: api.MessagePart.attachment(
                    api.Attachment(
                      aType: api.AttachmentType.mmcs(_mmcs()),
                      part_: 0,
                      utiType: 'public.jpeg',
                      mime: 'image/jpeg',
                      name: 'photo.jpg',
                      iris: false,
                    ),
                  ),
                  idx: 0,
                ),
              ],
            ),
            service: const api.MessageType.iMessage(),
            voice: false,
          ),
        ),
        sentTimestamp: 1,
        sendDelivered: false,
        verificationFailed: false,
      );
      final encoded = encodePushMessage(
        api.PushMessage.iMessage(attachmentOnly),
      );
      expect(textFromEncoded(encoded), isNull);
    });
  });

  group('small variants', () {
    test('send confirmation carries uuid and error', () {
      final ok = encodePushMessage(
        const api.PushMessage.sendConfirm(uuid: 'abc', error: null),
      );
      expectCompleteOrMarked(ok);
      expect(ok['uuid'], 'abc');
      expect(ok['error'], isNull);

      final failed = encodePushMessage(
        const api.PushMessage.sendConfirm(uuid: 'abc', error: 'not delivered'),
      );
      expect(failed['error'], 'not delivered');
    });

    test('registration state is encoded per variant', () {
      final registering = encodePushMessage(
        const api.PushMessage.registrationState(
          api.RegisterState.registering(),
        ),
      );
      expectCompleteOrMarked(registering);
      expect((registering['state']! as Map)['state'], 'registering');

      final failed = encodePushMessage(
        api.PushMessage.registrationState(
          const api.RegisterState.failed(error: 'throttled'),
        ),
      );
      expectCompleteOrMarked(failed);
      expect((failed['state']! as Map)['error'], 'throttled');
    });

    test('two factor and circle events encode', () {
      expectCompleteOrMarked(
        encodePushMessage(const api.PushMessage.twoFaAuthEvent(true)),
      );
      expectCompleteOrMarked(
        encodePushMessage(const api.PushMessage.circleFinishEvent()),
      );
    });

    test('status update carries its fields', () {
      final encoded = encodePushMessage(
        api.PushMessage.statusUpdate(
          const api.StatusKitMessage.statusChanged(
            user: 'someone',
            allowed: true,
          ),
        ),
      );
      expectCompleteOrMarked(encoded);
      final message = encoded['message']! as Map<String, Object?>;
      expect(message['user'], 'someone');
      expect(message['allowed'], isTrue);
    });
  });

  group('unmodelled variants are marked, never degraded', () {
    test('faceTime names its variant without inventing content', () {
      final encoded = encodePushMessage(
        api.PushMessage.faceTime(const api.FTMessage.ring(guid: 'g')),
      );
      expectCompleteOrMarked(encoded);
      expect(encoded['complete'], isFalse);
      expect(encoded[kUnsupported], 'faceTime');
      expect((encoded['message']! as Map)['variant'], 'ring');
    });

    test('photo stream marks its opaque album payload', () {
      final encoded = encodePushMessage(
        api.PushMessage.newPhotostream(
          const api.SharedAlbum(
            albumguid: 'album',
            sharingtype: 'public',
            assets: <String>[],
          ),
        ),
      );
      expectCompleteOrMarked(encoded);
      expect(encoded[kUnsupported], 'newPhotostream');
      expect((encoded['album']! as Map)['album_guid'], 'album');
    });
  });

  group('outbound message variants', () {
    api.MessageInst withMessage(api.Message message) => api.MessageInst(
          id: 'uuid',
          sender: 'mailto:me@example.invalid',
          conversation: null,
          message: message,
          sentTimestamp: 5,
          sendDelivered: false,
          verificationFailed: false,
        );

    test('unsend and edit encode their fields', () {
      final unsend = encodePushMessage(
        api.PushMessage.iMessage(
          withMessage(
            const api.Message.unsend(
              api.UnsendMessage(tuuid: 't', editPart: 1),
            ),
          ),
        ),
      );
      expectCompleteOrMarked(unsend);
      final unsendMessage = messageBody(unsend);
      expect(unsendMessage['type'], 'unsend');
      expect(unsendMessage['tuuid'], 't');
      expect(unsendMessage['edit_part'], 1);
      expect(unsendMessage['complete'], isTrue);

      final edit = encodePushMessage(
        api.PushMessage.iMessage(
          withMessage(
            api.Message.edit(
              api.EditMessage(
                tuuid: 't',
                editPart: 1,
                newParts: api.MessageParts(
                  field0: <api.IndexedMessagePart>[
                    api.IndexedMessagePart(part_: textPart('edited'), idx: 0),
                  ],
                ),
              ),
            ),
          ),
        ),
      );
      expectCompleteOrMarked(edit);
      final editMessage = messageBody(edit);
      expect(editMessage['type'], 'edit');
      expect(editMessage['tuuid'], 't');
      expect(editMessage['edit_part'], 1);
      expect(editMessage['complete'], isTrue);
      expect(editMessage['new_parts'], <Object?>[
        <String, Object?>{
          'idx': 0,
          'part': <String, Object?>{
            'type': 'text',
            'text': 'edited',
            'format': <String, Object?>{
              'type': 'flags',
              'bold': false,
              'italic': false,
              'underline': false,
              'strikethrough': false,
            },
          },
        },
      ]);
    });

    test('react encodes its target, part and reaction state', () {
      api.ReactMessage reactMessage({
        required api.Reaction reaction,
        required bool enable,
      }) =>
          api.ReactMessage(
            toUuid: 'target-uuid',
            toPart: 2,
            reaction: api.ReactMessageType.react(
              reaction: reaction,
              enable: enable,
            ),
            toText: 'placeholder',
          );

      Map<String, Object?> encodeReact({
        required api.Reaction reaction,
        required bool enable,
      }) =>
          messageBody(
            encodePushMessage(
              api.PushMessage.iMessage(
                withMessage(
                  api.Message.react(
                    reactMessage(reaction: reaction, enable: enable),
                  ),
                ),
              ),
            ),
          );

      final added = encodeReact(
        reaction: const api.Reaction.heart(),
        enable: true,
      );
      expect(added['type'], 'react');
      expect(added['complete'], isTrue);
      expect(added['to_uuid'], 'target-uuid');
      expect(added['to_part'], 2);
      final addedReaction = added['reaction']! as Map<String, Object?>;
      expect(addedReaction['kind'], 'react');
      expect(addedReaction['enable'], isTrue);
      expect(addedReaction['reaction'], 'heart');

      final removed = encodeReact(
        reaction: const api.Reaction.heart(),
        enable: false,
      );
      final removedReaction = removed['reaction']! as Map<String, Object?>;
      expect(removedReaction['enable'], isFalse);
      expect(removedReaction['reaction'], 'heart');

      // Reaction is a sealed union, so an unknown kind cannot reach the codec.
      // Every arm is mapped; assert the mapping stays total.
      final expected = <api.Reaction, Object?>{
        const api.Reaction.heart(): 'heart',
        const api.Reaction.like(): 'like',
        const api.Reaction.dislike(): 'dislike',
        const api.Reaction.laugh(): 'laugh',
        const api.Reaction.emphasize(): 'emphasize',
        const api.Reaction.question(): 'question',
        const api.Reaction.emoji('synthetic'): 'synthetic',
      };
      for (final entry in expected.entries) {
        final message = encodeReact(reaction: entry.key, enable: true);
        final reaction = message['reaction']! as Map<String, Object?>;
        expect(reaction['kind'], 'react');
        expect(reaction['enable'], isTrue);
        expect(reaction['reaction'], entry.value);
      }
    });

    test('unmodelled message variants are marked', () {
      final encoded = encodePushMessage(
        api.PushMessage.iMessage(
          withMessage(
            const api.Message.iconChange(
              api.IconChangeMessage(groupVersion: 1),
            ),
          ),
        ),
      );
      expectCompleteOrMarked(encoded);
      final value = messageBody(encoded);
      expect(value['type'], 'iconChange');
      expect(value['complete'], isFalse);
      expect(value[kUnsupported], 'iconChange');
    });

    test('delivered and read are complete', () {
      for (final message in <api.Message>[
        const api.Message.delivered(),
        const api.Message.read(),
      ]) {
        final encoded = encodePushMessage(
          api.PushMessage.iMessage(withMessage(message)),
        );
        expectCompleteOrMarked(encoded);
        expect(messageBody(encoded)['complete'], isTrue);
      }
    });
  });
}
