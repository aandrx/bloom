import 'dart:io';

import 'package:bloom_engine_sidecar/attachment_send.dart';
import 'package:test/test.dart';

void main() {
  late Directory root;

  setUp(() {
    root = Directory.systemTemp.createTempSync('bloom-attachment-send-');
  });

  tearDown(() {
    if (root.existsSync()) root.deleteSync(recursive: true);
  });

  File write(String name, int bytes) {
    final file = File('${root.path}/$name');
    file.writeAsBytesSync(List<int>.filled(bytes, 0x41));
    return file;
  }

  test('accepts a regular absolute file and infers its type', () {
    final file = write('Photo.JPG', 5);
    final attachment = validateOutgoingAttachment(file.path);
    expect(attachment.path, file.path);
    expect(attachment.name, 'Photo.JPG');
    expect(attachment.mime, 'image/jpeg');
    expect(attachment.uti, 'public.jpeg');
    expect(attachment.bytes, 5);
  });

  test('uses generic values for an unknown extension', () {
    final file = write('archive.bloomish', 3);
    final attachment = validateOutgoingAttachment(file.path);
    expect(attachment.mime, 'application/octet-stream');
    expect(attachment.uti, 'public.data');
  });

  test('refuses relative, parent, symlink, directory and empty inputs', () {
    final directory = Directory('${root.path}/folder')..createSync();
    final target = write('target', 1);
    final link = Link('${root.path}/link')..createSync(target.path);
    final empty = write('empty', 0);

    for (final path in <String>[
      'relative.bin',
      '${root.path}/../escape.bin',
      '${root.path}/./dot.bin',
      directory.path,
      link.path,
      empty.path,
      '${root.path}/missing.bin',
      '/',
    ]) {
      expect(
        () => validateOutgoingAttachment(path),
        throwsA(isA<AttachmentSendFailure>()),
        reason: 'expected a rejection',
      );
    }
  });

  test('refuses a file above the outbound bound', () {
    final file = write('large.bin', 0);
    final handle = file.openSync(mode: FileMode.write);
    try {
      handle.truncateSync(maxOutboundAttachmentBytes + 1);
    } finally {
      handle.closeSync();
    }
    expect(
      () => validateOutgoingAttachment(file.path),
      throwsA(isA<AttachmentSendFailure>()
          .having((failure) => failure.code, 'code', 'too_large')),
    );
  });

  test('maps common extensions without reading content', () {
    expect(attachmentType('a.png'), ('image/png', 'public.png'));
    expect(attachmentType('a.jpeg'), ('image/jpeg', 'public.jpeg'));
    expect(attachmentType('a.mov'),
        ('video/quicktime', 'com.apple.quicktime-movie'));
    expect(attachmentType('a.pdf'), ('application/pdf', 'com.adobe.pdf'));
    expect(attachmentType('no-extension'),
        ('application/octet-stream', 'public.data'));
  });
}
