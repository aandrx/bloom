import 'package:bloom_engine_sidecar/upstream/api/api.dart' as api;
import 'package:test/test.dart';

api.TextFormat plain() => api.TextFormat.flags(
      api.TextFlags(
        bold: false,
        italic: false,
        underline: false,
        strikethrough: false,
      ),
    );

void main() {
  test('edit payload names the target guid, part and replacement text', () {
    final message = api.Message.edit(
      api.EditMessage(
        tuuid: 'engine-guid',
        editPart: 2,
        newParts: api.MessageParts(
          field0: <api.IndexedMessagePart>[
            api.IndexedMessagePart(
              part_: api.MessagePart.text('replacement body', plain()),
            ),
          ],
        ),
      ),
    );
    expect(message, isA<api.Message_Edit>());
    final edit = (message as api.Message_Edit).field0;
    expect(edit.tuuid, 'engine-guid');
    expect(edit.editPart, 2);
    expect(edit.newParts.field0, hasLength(1));
    final part = edit.newParts.field0.single.part_;
    expect(part, isA<api.MessagePart_Text>());
    expect((part as api.MessagePart_Text).field0, 'replacement body');
  });

  test('unsend payload names one target guid and part', () {
    final message = api.Message.unsend(
      api.UnsendMessage(tuuid: 'engine-guid', editPart: 2),
    );
    expect(message, isA<api.Message_Unsend>());
    final unsend = (message as api.Message_Unsend).field0;
    expect(unsend.tuuid, 'engine-guid');
    expect(unsend.editPart, 2);
  });
}
