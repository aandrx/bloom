import 'dart:async';
import 'dart:collection';
import 'dart:io';
import 'dart:math';

import 'upstream/api/api.dart' as api;
import 'upstream/lib.dart' as upstream;

/// Downloads an attachment to [path] and reports progress.
///
/// Production callers pass an `api.ApsConnection`; tests may pass any object
/// because the default implementation rejects unknown types before use.
typedef AttachmentDownloader = Stream<api.TransferProgress> Function(
  Object? aps,
  api.Attachment attachment,
  String path,
);

/// Resolves the engine-reported size without requiring a live connection.
typedef AttachmentSizeResolver = Future<int> Function(
    api.Attachment attachment);

/// A closed set of attachment failures. Messages never carry attachment data.
sealed class AttachmentFailure implements Exception {
  const AttachmentFailure(this.code);

  final String code;

  @override
  String toString() => '$runtimeType($code)';
}

class AttachmentInvalidTokenFailure extends AttachmentFailure {
  const AttachmentInvalidTokenFailure() : super('invalid_token');
}

class AttachmentTooLargeFailure extends AttachmentFailure {
  const AttachmentTooLargeFailure() : super('too_large');
}

class AttachmentTimeoutFailure extends AttachmentFailure {
  const AttachmentTimeoutFailure() : super('timeout');
}

class AttachmentDownloadFailure extends AttachmentFailure {
  const AttachmentDownloadFailure([this.stage = 'download', this.cause])
      : super('download_failed');

  /// Fixed tag for the failing step, so a live attempt is diagnosable without
  /// logging error text, paths or attachment metadata.
  final String stage;

  /// Allowlisted engine error token, or null when the step has no engine error.
  final String? cause;
}

/// Engine error prefixes mapped to fixed, content-free diagnostic tokens.
///
/// Engine errors can embed object identifiers, signatures or URLs, so only the
/// mapped token is ever reported; the scanned text is discarded.
const Map<String, String> downloadCauseTokens = {
  'MMCS GET failed': 'mmcs_get_failed',
  'MMCS Upload failed': 'mmcs_upload_failed',
  'Not Connected': 'not_connected',
  'Resource Timeout': 'resource_timeout',
  'Resource Panic': 'resource_panic',
  'ConnectionClosed': 'connection_closed',
  'Aps connection failed': 'aps_connect',
  'Your iMessage access is temporarily disabled': 'imessage_disabled',
  'Failed to authenticate': 'auth',
  'Bad auth cert': 'auth_invalid',
  'HTTP error': 'http',
  'IO error': 'io',
  'TLS error': 'tls',
  'Send timeout': 'send_timeout',
  'Could not deliver message': 'no_valid_targets',
  'Other side hung up': 'aps_send',
  'Keyed archive error': 'keyed_archive',
  'Plist parsing error': 'plist_parse',
};

/// Classifies a downloader error into a fixed token without keeping its text.
String downloadErrorCause(Object error) {
  final text = error.toString();
  for (final entry in downloadCauseTokens.entries) {
    if (text.contains(entry.key)) {
      return entry.value;
    }
  }
  return 'unclassified_${errorTypeToken(error)}';
}

/// Runtime type reduced to a bounded identifier, since Dart and FFI errors
/// carry no attachment data in their type name.
String errorTypeToken(Object error) {
  final safe = error.runtimeType.toString().replaceAll(
        RegExp(r'[^A-Za-z0-9_]'),
        '',
      );
  if (safe.isEmpty) {
    return 'unknown';
  }
  return safe.length <= 40 ? safe : safe.substring(0, 40);
}

/// Bounded, content-free suffix for a fetch-failure diagnostic line.
String attachmentFailureDetail(AttachmentFailure failure) {
  if (failure is! AttachmentDownloadFailure) {
    return '';
  }
  final cause = failure.cause;
  return ' stage=${failure.stage}${cause == null ? '' : ' cause=$cause'}';
}

class AttachmentUnsafeRootFailure extends AttachmentFailure {
  const AttachmentUnsafeRootFailure() : super('unsafe_root');
}

/// Metadata retained for one registered inbound attachment part.
class AttachmentRegistration {
  const AttachmentRegistration({
    required this.token,
    required this.messageId,
    required this.partIndex,
    required this.attachment,
  });

  final String token;
  final String messageId;
  final int partIndex;
  final api.Attachment attachment;

  @override
  String toString() => 'AttachmentRegistration(partIndex: $partIndex)';
}

/// Bounded in-memory map from an opaque token to an engine attachment.
class AttachmentRegistry {
  AttachmentRegistry({
    this.maxEntries = 128,
    String Function()? tokenFactory,
  }) : _tokenFactory = tokenFactory ?? _randomToken {
    if (maxEntries < 1) {
      throw ArgumentError.value(maxEntries, 'maxEntries');
    }
  }

  static final RegExp _tokenPattern = RegExp(r'^[0-9a-f]{32}$');

  final int maxEntries;
  final String Function() _tokenFactory;
  final LinkedHashMap<String, AttachmentRegistration> _entries =
      LinkedHashMap<String, AttachmentRegistration>();
  final Map<String, String> _byPart = {};

  /// Registers one part and returns its opaque token.
  String register({
    required String messageId,
    required int partIndex,
    required api.Attachment attachment,
  }) {
    if (messageId.isEmpty) {
      throw ArgumentError('messageId must not be empty');
    }
    if (partIndex < 0) {
      throw ArgumentError.value(partIndex, 'partIndex');
    }

    final partKey = '$messageId\u0000$partIndex';
    final existing = _byPart[partKey];
    if (existing != null && _entries.containsKey(existing)) {
      // Keep the advertised token but refresh the engine handle and recency:
      // a redelivery can carry a refreshed attachment object.
      final registration = _entries.remove(existing)!;
      _entries[existing] = AttachmentRegistration(
        token: existing,
        messageId: registration.messageId,
        partIndex: registration.partIndex,
        attachment: attachment,
      );
      return existing;
    }
    final token = _uniqueToken();
    if (_entries.length >= maxEntries) {
      final oldest = _entries.keys.first;
      final removed = _entries.remove(oldest);
      if (removed != null) {
        _byPart.remove('${removed.messageId}\u0000${removed.partIndex}');
      }
    }
    _entries[token] = AttachmentRegistration(
      token: token,
      messageId: messageId,
      partIndex: partIndex,
      attachment: attachment,
    );
    _byPart[partKey] = token;
    return token;
  }

  AttachmentRegistration? lookup(String token) => _entries[token];

  AttachmentRegistration? remove(String token) {
    final removed = _entries.remove(token);
    if (removed != null) {
      _byPart.remove('${removed.messageId}\u0000${removed.partIndex}');
    }
    return removed;
  }

  int get length => _entries.length;

  String _uniqueToken() {
    for (var attempt = 0; attempt < 16; attempt++) {
      final token = _tokenFactory();
      if (!_tokenPattern.hasMatch(token)) {
        throw StateError('token factory returned an invalid token');
      }
      if (!_entries.containsKey(token)) return token;
    }
    throw StateError('token factory did not produce a unique token');
  }

  static String _randomToken() {
    final random = Random.secure();
    final bytes = List<int>.generate(16, (_) => random.nextInt(256));
    return bytes.map((byte) => byte.toRadixString(16).padLeft(2, '0')).join();
  }

  @override
  String toString() =>
      'AttachmentRegistry(entries: $_entries.length/$maxEntries)';
}

/// A fetched file and whether an existing complete file was reused.
class AttachmentCachedFile {
  const AttachmentCachedFile({
    required this.file,
    required this.bytes,
    required this.reused,
  });

  final File file;
  final int bytes;
  final bool reused;

  @override
  String toString() => 'AttachmentCachedFile(bytes: $bytes, reused: $reused)';
}

/// Writes registered attachments into a private, bounded cache directory.
class AttachmentCache {
  AttachmentCache({
    required this.root,
    AttachmentRegistry? registry,
    AttachmentDownloader? downloader,
    AttachmentSizeResolver? sizeResolver,
    this.timeout = defaultTimeout,
    DateTime Function()? clock,
  })  : registry = registry ?? AttachmentRegistry(),
        _downloader = downloader ?? _engineDownloader,
        _sizeResolver = sizeResolver ?? _engineSize,
        _clock = clock ?? DateTime.now;

  static const int maxFileBytes = 64 * 1024 * 1024;
  static const int maxTotalBytes = 512 * 1024 * 1024;
  static const int maxEntries = 128;
  static const Duration maxAge = Duration(days: 30);
  static const Duration defaultTimeout = Duration(seconds: 120);

  static final RegExp _tokenPattern = RegExp(r'^[0-9a-f]{32}$');
  static final RegExp _tokenFilePattern = RegExp(r'^([0-9a-f]{32})\.bin$');
  static final RegExp _tokenPartPattern =
      RegExp(r'^\.[0-9a-f]{32}\.[0-9a-f]{8}\.part$');
  static const Duration stalePartAge = Duration(hours: 1);

  final Directory root;
  final AttachmentRegistry registry;
  final AttachmentDownloader _downloader;
  final AttachmentSizeResolver _sizeResolver;
  final Duration timeout;
  final DateTime Function() _clock;
  final Map<String, Future<AttachmentCachedFile>> _inFlight = {};
  int _attempt = 0;

  /// Resolves `$XDG_CACHE_HOME/bloom/attachments` or
  /// `$HOME/.cache/bloom/attachments`.
  static Directory resolveDefaultRoot({Map<String, String>? environment}) {
    final env = environment ?? Platform.environment;
    final xdg = env['XDG_CACHE_HOME'];
    if (xdg != null && xdg.isNotEmpty) {
      return Directory('$xdg/bloom/attachments');
    }
    final home = env['HOME'];
    if (home == null || home.isEmpty) {
      throw const AttachmentUnsafeRootFailure();
    }
    return Directory('$home/.cache/bloom/attachments');
  }

  /// Fetches the attachment registered under [token].
  Future<AttachmentCachedFile> fetch(String token, {Object? aps}) {
    if (!_tokenPattern.hasMatch(token)) {
      return Future.error(const AttachmentInvalidTokenFailure());
    }
    if (registry.lookup(token) == null) {
      return Future.error(const AttachmentInvalidTokenFailure());
    }
    // Two callers for one token must share one download. A shared temp name is
    // otherwise deletable by the second caller while the first is still writing.
    return _inFlight.putIfAbsent(token, () {
      final pending = _fetchLocked(token, aps);
      return pending.whenComplete(() {
        _inFlight.remove(token);
      });
    });
  }

  Future<AttachmentCachedFile> _fetchLocked(String token, Object? aps) async {
    final registration = registry.lookup(token);
    if (registration == null) {
      throw const AttachmentInvalidTokenFailure();
    }

    await _ensureRoot();

    final finalFile = File('${root.path}/$token.bin');
    final attempt = (_attempt++).toRadixString(16).padLeft(8, '0');
    final tempFile = File('${root.path}/.$token.$attempt.part');
    final knownSize = await _resolveSize(registration.attachment);
    if (knownSize > maxFileBytes) {
      throw const AttachmentTooLargeFailure();
    }
    if (knownSize < 0) {
      throw const AttachmentDownloadFailure('size');
    }

    final finalType = await FileSystemEntity.type(
      finalFile.path,
      followLinks: false,
    );
    if (finalType == FileSystemEntityType.file) {
      try {
        final stat = await finalFile.stat();
        if (stat.type == FileSystemEntityType.file) {
          // A planted or previously oversized file must not bypass the cap on reuse.
          if (stat.size > maxFileBytes) {
            await finalFile.delete();
            throw const AttachmentTooLargeFailure();
          }
          // A zero-byte cached file cannot be a completed non-empty attachment.
          if (knownSize > 0 && stat.size == 0) {
            await finalFile.delete();
            throw const AttachmentDownloadFailure('reuse_empty');
          }
          if (stat.size > knownSize) {
            await finalFile.delete();
            throw const AttachmentDownloadFailure('reuse_oversized');
          }
          await _setMode(finalFile.path, '600');
          await _evict(protect: finalFile.path);
          return AttachmentCachedFile(
            file: finalFile,
            bytes: stat.size,
            reused: true,
          );
        }
      } on AttachmentFailure {
        rethrow;
      } on FileSystemException {
        throw const AttachmentDownloadFailure('reuse_stat');
      }
    }
    if (finalType != FileSystemEntityType.notFound) {
      throw const AttachmentDownloadFailure('cache_entry');
    }

    await _removeTemp(tempFile);
    try {
      await tempFile.create(exclusive: true);
    } on FileSystemException {
      throw const AttachmentDownloadFailure('temp_create');
    }

    try {
      await _setMode(tempFile.path, '600');
      await _runDownload(registration.attachment, aps, tempFile.path);

      final stat = await tempFile.stat();
      if (stat.type != FileSystemEntityType.file) {
        throw const AttachmentDownloadFailure('temp_stat');
      }
      if (stat.size > maxFileBytes) {
        throw const AttachmentTooLargeFailure();
      }
      // `Attachment.getSize()` may sum multiple MMCS variants while the
      // downloader writes only the largest one. Treat it as an upper bound,
      // never as an exact-content assertion; zero bytes for a positive report
      // is still a failed transfer.
      if (stat.size > knownSize || (knownSize > 0 && stat.size == 0)) {
        throw const AttachmentDownloadFailure('size_mismatch');
      }

      await _setMode(tempFile.path, '600');
      await tempFile.rename(finalFile.path);
      try {
        await _setMode(finalFile.path, '600');
      } on FileSystemException {
        // Never leave a reusable file whose privacy mode was not applied.
        await finalFile.delete();
        rethrow;
      }
      await _evict(protect: finalFile.path);
      return AttachmentCachedFile(
        file: finalFile,
        bytes: stat.size,
        reused: false,
      );
    } on AttachmentFailure {
      rethrow;
    } on FileSystemException {
      throw const AttachmentDownloadFailure('filesystem');
    } finally {
      await _removeTemp(tempFile);
    }
  }

  Future<int> _resolveSize(api.Attachment attachment) async {
    try {
      return await Future<int>.sync(() => _sizeResolver(attachment)).timeout(
        timeout,
        onTimeout: () => throw const AttachmentTimeoutFailure(),
      );
    } on AttachmentFailure {
      rethrow;
    } catch (error) {
      throw AttachmentDownloadFailure('size_threw', downloadErrorCause(error));
    }
  }

  Future<void> _runDownload(
    api.Attachment attachment,
    Object? aps,
    String path,
  ) {
    final completer = Completer<void>();
    StreamSubscription<api.TransferProgress>? subscription;
    Timer? timer;
    var settled = false;

    void fail(AttachmentFailure failure) {
      if (settled) return;
      settled = true;
      timer?.cancel();
      final cancelled = subscription?.cancel();
      if (cancelled == null) {
        if (!completer.isCompleted) completer.completeError(failure);
        return;
      }
      // Wait briefly for cancellation before the caller deletes the temp file.
      // A broken or never-completing native cancel must not defeat the timeout.
      cancelled
          .timeout(const Duration(seconds: 1), onTimeout: () {})
          .whenComplete(() {
        if (!completer.isCompleted) completer.completeError(failure);
      });
    }

    void complete() {
      if (settled) return;
      settled = true;
      timer?.cancel();
      if (!completer.isCompleted) completer.complete();
    }

    final Stream<api.TransferProgress> stream;
    try {
      stream = _downloader(aps, attachment, path);
    } catch (error) {
      fail(AttachmentDownloadFailure('construct', downloadErrorCause(error)));
      return completer.future;
    }

    try {
      timer = Timer(timeout, () => fail(const AttachmentTimeoutFailure()));
      final created = stream.listen(
        (progress) {
          if (settled) return;
          if (progress.prog < 0 || progress.total < 0) {
            fail(const AttachmentDownloadFailure('progress_negative'));
          } else if (progress.total > maxFileBytes ||
              progress.prog > maxFileBytes) {
            fail(const AttachmentTooLargeFailure());
          } else if (progress.prog > progress.total) {
            fail(const AttachmentDownloadFailure('progress_over_total'));
          }
        },
        onError: (Object error) => fail(
          AttachmentDownloadFailure('stream', downloadErrorCause(error)),
        ),
        onDone: complete,
        cancelOnError: true,
      );
      subscription = created;
      if (settled) {
        unawaited(created.cancel());
      }
    } catch (error) {
      fail(AttachmentDownloadFailure('listen', downloadErrorCause(error)));
    }

    return completer.future;
  }

  Future<void> _ensureRoot() async {
    try {
      final type = await FileSystemEntity.type(root.path, followLinks: false);
      if (type == FileSystemEntityType.link) {
        throw const AttachmentUnsafeRootFailure();
      }
      if (type == FileSystemEntityType.notFound) {
        await root.create(recursive: true);
      }
      final created =
          await FileSystemEntity.type(root.path, followLinks: false);
      if (created != FileSystemEntityType.directory) {
        throw const AttachmentUnsafeRootFailure();
      }
      await _setMode(root.path, '700');
    } on AttachmentFailure {
      rethrow;
    } on FileSystemException {
      throw const AttachmentUnsafeRootFailure();
    }
  }

  Future<void> _removeTemp(File tempFile) async {
    try {
      final type = await FileSystemEntity.type(
        tempFile.path,
        followLinks: false,
      );
      if (type != FileSystemEntityType.notFound) {
        await tempFile.delete(recursive: true);
      }
    } on FileSystemException {
      // A failed cleanup is reported by the next exclusive create.
    }
  }

  Future<void> _setMode(String path, String mode) async {
    final result = await Process.run('chmod', <String>[mode, path]);
    if (result.exitCode != 0) {
      throw FileSystemException('chmod failed');
    }
  }

  Future<void> _evict({required String protect}) async {
    final now = _clock();
    final entries = <_CacheEntry>[];

    try {
      await for (final entity in root.list(followLinks: false)) {
        if (entity is! File) continue;
        final name = entity.path.split(Platform.pathSeparator).last;
        if (_tokenPartPattern.hasMatch(name)) {
          final stat = await entity.stat();
          if (now.difference(stat.modified) > stalePartAge) {
            await entity.delete();
          }
          continue;
        }
        if (!_tokenFilePattern.hasMatch(name)) continue;
        final stat = await entity.stat();
        if (stat.type != FileSystemEntityType.file) continue;
        if (now.difference(stat.modified) > maxAge && entity.path != protect) {
          await entity.delete();
          continue;
        }
        entries.add(
          _CacheEntry(
              path: entity.path, size: stat.size, modified: stat.modified),
        );
      }
    } on FileSystemException {
      // Keep whatever entries were readable; one bad entry must not disable
      // retention for the rest of the cache.
    }

    entries.sort((a, b) {
      final byTime = a.modified.compareTo(b.modified);
      return byTime != 0 ? byTime : a.path.compareTo(b.path);
    });

    var total = entries.fold<int>(0, (sum, entry) => sum + entry.size);
    var count = entries.length;
    for (final entry in entries) {
      if (total <= maxTotalBytes && count <= maxEntries) break;
      if (entry.path == protect) continue;
      try {
        await File(entry.path).delete();
      } on FileSystemException {
        continue;
      }
      total -= entry.size;
      count--;
    }
  }

  @override
  String toString() => 'AttachmentCache()';

  static Future<int> _engineSize(api.Attachment attachment) =>
      attachment.getSize();

  static Stream<api.TransferProgress> _engineDownloader(
    Object? aps,
    api.Attachment attachment,
    String path,
  ) {
    final connection = aps;
    if (connection is! upstream.ApsConnection) {
      return Stream<api.TransferProgress>.error(
        const AttachmentDownloadFailure(),
      );
    }
    return api.downloadAttachment(
      aps: connection,
      attachment: attachment,
      path: path,
    );
  }
}

class _CacheEntry {
  const _CacheEntry({
    required this.path,
    required this.size,
    required this.modified,
  });

  final String path;
  final int size;
  final DateTime modified;
}
