import 'dart:io';

/// Conservative bound for one outbound file. The daemon applies the same limit;
/// this second check runs immediately before the engine reads the path.
const maxOutboundAttachmentBytes = 25 * 1024 * 1024;

/// A path or file problem that is safe to describe without the path itself.
final class AttachmentSendFailure implements Exception {
  const AttachmentSendFailure(this.code);

  /// Bounded code such as `invalid_path`, `not_a_file` or `too_large`.
  final String code;

  @override
  String toString() => 'AttachmentSendFailure($code)';
}

/// A validated local file plus the display metadata the engine needs.
final class OutgoingAttachment {
  const OutgoingAttachment({
    required this.path,
    required this.name,
    required this.mime,
    required this.uti,
    required this.bytes,
  });

  final String path;
  final String name;
  final String mime;
  final String uti;
  final int bytes;
}

/// Validate one absolute path and infer its MIME type and Apple UTI.
///
/// The path never appears in an exception or diagnostic. Symlinks are refused
/// before the file is read, matching the daemon's check.
OutgoingAttachment validateOutgoingAttachment(String path) {
  if (!path.startsWith('/') ||
      path.isEmpty ||
      path.split('/').any((part) => part == '.' || part == '..')) {
    throw const AttachmentSendFailure('invalid_path');
  }
  if (FileSystemEntity.isLinkSync(path)) {
    throw const AttachmentSendFailure('invalid_path');
  }
  final FileStat stat;
  try {
    stat = File(path).statSync();
  } on FileSystemException {
    throw const AttachmentSendFailure('unreadable');
  }
  if (stat.type != FileSystemEntityType.file) {
    throw const AttachmentSendFailure('not_a_file');
  }
  if (stat.size <= 0) {
    throw const AttachmentSendFailure('empty');
  }
  if (stat.size > maxOutboundAttachmentBytes) {
    throw const AttachmentSendFailure('too_large');
  }
  final name = path.split('/').last;
  if (name.isEmpty) {
    throw const AttachmentSendFailure('invalid_path');
  }
  final type = attachmentType(name);
  return OutgoingAttachment(
    path: path,
    name: name,
    mime: type.$1,
    uti: type.$2,
    bytes: stat.size,
  );
}

/// Infer a MIME type and UTI from the extension only. Unknown extensions fall
/// back to the same generic values upstream uses when it has no stored type.
(String, String) attachmentType(String name) {
  final dot = name.lastIndexOf('.');
  if (dot <= 0 || dot == name.length - 1) {
    return ('application/octet-stream', 'public.data');
  }
  final extension = name.substring(dot + 1).toLowerCase();
  return switch (extension) {
    'png' => ('image/png', 'public.png'),
    'jpg' || 'jpeg' => ('image/jpeg', 'public.jpeg'),
    'gif' => ('image/gif', 'com.compuserve.gif'),
    'heic' => ('image/heic', 'public.heic'),
    'heif' => ('image/heif', 'public.heif'),
    'webp' => ('image/webp', 'org.webmproject.webp'),
    'tif' || 'tiff' => ('image/tiff', 'public.tiff'),
    'mp4' || 'm4v' => ('video/mp4', 'public.mpeg-4'),
    'mov' => ('video/quicktime', 'com.apple.quicktime-movie'),
    'pdf' => ('application/pdf', 'com.adobe.pdf'),
    'txt' => ('text/plain', 'public.plain-text'),
    _ => ('application/octet-stream', 'public.data'),
  };
}
