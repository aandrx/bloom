import 'dart:async';
import 'dart:io';
import 'dart:typed_data';

import 'package:bloom_engine_sidecar/attachment_cache.dart';
import 'package:bloom_engine_sidecar/upstream/api/api.dart' as api;
import 'package:test/test.dart';

api.Attachment _attachment({
  String name = 'photo.jpg',
  String mime = 'image/jpeg',
  String uti = 'public.jpeg',
  int part = 0,
}) =>
    api.Attachment(
      aType: api.AttachmentType.inline(Uint8List(0)),
      part_: part,
      utiType: uti,
      mime: mime,
      name: name,
      iris: false,
    );

String Function() _tokens() {
  var next = 0;
  return () => (next++).toRadixString(16).padLeft(32, '0');
}

Future<AttachmentCachedFile> _fetch(
  AttachmentCache cache,
  String token, {
  Object? aps,
}) =>
    cache.fetch(token, aps: aps);

Future<void> _writeSparse(String path, int bytes) async {
  final file = await File(path).open(mode: FileMode.write);
  await file.truncate(bytes);
  await file.close();
}

int _mode(String path) => FileStat.statSync(path).mode & 0x1FF;

List<String> _partFiles(Directory root) => root
    .listSync()
    .whereType<File>()
    .map((file) => file.path)
    .where((path) => path.endsWith('.part'))
    .toList();

void main() {
  group('AttachmentRegistry', () {
    test('issues unique opaque tokens and retains metadata', () {
      final registry = AttachmentRegistry(tokenFactory: _tokens());
      final attachment = _attachment();
      final first = registry.register(
        messageId: 'm1',
        partIndex: 0,
        attachment: attachment,
      );
      final second = registry.register(
        messageId: 'm1',
        partIndex: 1,
        attachment: attachment,
      );

      expect(first, matches(RegExp(r'^[0-9a-f]{32}$')));
      expect(second, matches(RegExp(r'^[0-9a-f]{32}$')));
      expect(first, isNot(second));
      final entry = registry.lookup(first);
      expect(entry?.messageId, 'm1');
      expect(entry?.partIndex, 0);
      expect(identical(entry?.attachment, attachment), isTrue);
      expect(registry.remove(first)?.token, first);
      expect(registry.lookup(first), isNull);
    });

    test('default tokens are unique and evicts the oldest entry', () {
      final registry = AttachmentRegistry();
      final tokens = <String>{};
      for (var index = 0; index < 128; index++) {
        tokens.add(
          registry.register(
            messageId: 'm$index',
            partIndex: 0,
            attachment: _attachment(part: index),
          ),
        );
      }
      expect(tokens, hasLength(128));
      final oldest = tokens.first;
      final newest = registry.register(
        messageId: 'overflow',
        partIndex: 0,
        attachment: _attachment(),
      );

      expect(registry.lookup(oldest), isNull);
      expect(registry.lookup(newest)?.messageId, 'overflow');
      expect(registry.length, 128);
    });

    test('rejects empty message ids and negative part indexes', () {
      final registry = AttachmentRegistry(tokenFactory: _tokens());
      expect(
        () => registry.register(
          messageId: '',
          partIndex: 0,
          attachment: _attachment(),
        ),
        throwsArgumentError,
      );
      expect(
        () => registry.register(
          messageId: 'm1',
          partIndex: -1,
          attachment: _attachment(),
        ),
        throwsArgumentError,
      );
    });

    test('debug output never includes attachment fields', () {
      final registry = AttachmentRegistry(tokenFactory: _tokens());
      registry.register(
        messageId: 'm1',
        partIndex: 0,
        attachment: _attachment(
          name: 'secret-name',
          mime: 'secret/mime',
          uti: 'secret.uti',
        ),
      );
      final text = registry.toString();
      expect(text, isNot(contains('secret-name')));
      expect(text, isNot(contains('secret/mime')));
      expect(text, isNot(contains('secret.uti')));
      expect(text.length, lessThan(200));
    });
  });

  group('AttachmentCache', () {
    late Directory temp;
    late Directory root;
    late AttachmentRegistry registry;

    setUp(() async {
      temp = await Directory.systemTemp.createTemp('bloom-attachment-test-');
      root = Directory('${temp.path}/cache');
      registry = AttachmentRegistry(tokenFactory: _tokens());
    });

    tearDown(() async {
      await temp.delete(recursive: true);
    });

    String register({
      String name = 'photo.jpg',
      String mime = 'image/jpeg',
      String uti = 'public.jpeg',
    }) =>
        registry.register(
          messageId: 'message-1',
          partIndex: 0,
          attachment: _attachment(name: name, mime: mime, uti: uti),
        );

    test('resolves the default root from XDG_CACHE_HOME or HOME', () {
      expect(
        AttachmentCache.resolveDefaultRoot(
          environment: {'XDG_CACHE_HOME': '/tmp/xdg'},
        ).path,
        '/tmp/xdg/bloom/attachments',
      );
      expect(
        AttachmentCache.resolveDefaultRoot(
          environment: {'HOME': '/home/person'},
        ).path,
        '/home/person/.cache/bloom/attachments',
      );
      expect(
        () => AttachmentCache.resolveDefaultRoot(environment: const {}),
        throwsA(isA<AttachmentUnsafeRootFailure>()),
      );
    });

    test('rejects invalid tokens before touching the filesystem', () async {
      var downloads = 0;
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, ___) {
          downloads++;
          return const Stream<api.TransferProgress>.empty();
        },
        sizeResolver: (_) async => 0,
      );

      for (final token in [
        '',
        '../escape',
        'a' * 31,
        'A' * 32,
        'a' * 32 + '/',
        '0' * 32 + '\n',
      ]) {
        await expectLater(
          _fetch(cache, token),
          throwsA(isA<AttachmentInvalidTokenFailure>()),
        );
      }
      expect(downloads, 0);
      expect(root.existsSync(), isFalse);
    });

    test('rejects a symlink root', () async {
      final target = Directory('${temp.path}/target')..createSync();
      final link = Link('${temp.path}/cache')..createSync(target.path);
      final cache = AttachmentCache(
        root: Directory(link.path),
        registry: registry,
      );
      final token = register();

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentUnsafeRootFailure>()),
      );
      expect(File('${target.path}/$token.bin').existsSync(), isFalse);
    });

    test('writes exact bytes atomically with private permissions', () async {
      final payload = Uint8List.fromList(List<int>.generate(64, (i) => i));
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await File(path).writeAsBytes(payload);
          yield api.TransferProgress(
              prog: payload.length, total: payload.length);
        },
        sizeResolver: (_) async => payload.length,
      );
      final token = register();

      final result = await _fetch(cache, token, aps: Object());

      expect(result.reused, isFalse);
      expect(result.bytes, payload.length);
      expect(await result.file.readAsBytes(), payload);
      expect(result.file.path, '${root.path}/$token.bin');
      expect(_partFiles(root), isEmpty);
      expect(_mode(root.path), 0x1C0);
      expect(_mode(result.file.path), 0x180);
    });

    test('stream failure leaves no temporary or final file', () async {
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await File(path).writeAsBytes([1, 2, 3]);
          yield const api.TransferProgress(prog: 3, total: 3);
          throw StateError('super-secret-payload');
        },
        sizeResolver: (_) async => 3,
      );
      final token = register();

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentDownloadFailure>()),
      );
      expect(File('${root.path}/$token.bin').existsSync(), isFalse);
      expect(_partFiles(root), isEmpty);
    });

    test('rejects a known zero-size attachment that delivers bytes', () async {
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await File(path).writeAsBytes([1, 2, 3]);
          yield const api.TransferProgress(prog: 3, total: 3);
        },
        sizeResolver: (_) async => 0,
      );
      final token = register();

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentDownloadFailure>()),
      );
      expect(File('${root.path}/$token.bin').existsSync(), isFalse);
      expect(_partFiles(root), isEmpty);
    });

    test('rejects a known oversized size before download', () async {
      var downloads = 0;
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, ___) {
          downloads++;
          return const Stream<api.TransferProgress>.empty();
        },
        sizeResolver: (_) async => AttachmentCache.maxFileBytes + 1,
      );
      final token = register();

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentTooLargeFailure>()),
      );
      expect(downloads, 0);
      expect(root.listSync(), isEmpty);
    });

    test('rejects a progress total over the per-file cap', () async {
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await File(path).writeAsBytes([1]);
          yield const api.TransferProgress(
            prog: 1,
            total: AttachmentCache.maxFileBytes + 1,
          );
        },
        sizeResolver: (_) async => 1,
      );
      final token = register();

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentTooLargeFailure>()),
      );
      expect(File('${root.path}/$token.bin').existsSync(), isFalse);
      expect(_partFiles(root), isEmpty);
    });

    test('rejects a final file over the per-file cap', () async {
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await _writeSparse(path, AttachmentCache.maxFileBytes + 1);
          yield const api.TransferProgress(prog: 1, total: 1);
        },
        sizeResolver: (_) async => 1,
      );
      final token = register();

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentTooLargeFailure>()),
      );
      expect(File('${root.path}/$token.bin').existsSync(), isFalse);
      expect(_partFiles(root), isEmpty);
    });

    test('times out, cancels the stream, and cleans up', () async {
      var cancelled = false;
      late StreamController<api.TransferProgress> controller;
      controller = StreamController<api.TransferProgress>(
        onCancel: () {
          cancelled = true;
        },
      );
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        timeout: const Duration(milliseconds: 30),
        downloader: (_, __, ___) => controller.stream,
        sizeResolver: (_) async => 1,
      );
      final token = register();

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentTimeoutFailure>()),
      );
      await Future<void>.delayed(Duration.zero);
      expect(cancelled, isTrue);
      expect(File('${root.path}/$token.bin').existsSync(), isFalse);
      expect(_partFiles(root), isEmpty);
      await controller.close();
    });

    test('does not reuse a zero-byte file for a non-empty attachment',
        () async {
      root.createSync(recursive: true);
      final token = register();
      final finalFile = File('${root.path}/$token.bin');
      await finalFile.writeAsBytes(const []);
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, ___) => const Stream<api.TransferProgress>.empty(),
        sizeResolver: (_) async => 10,
      );

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentDownloadFailure>()),
      );
      expect(finalFile.existsSync(), isFalse);
    });

    test('reuses an existing regular final file without downloading', () async {
      root.createSync(recursive: true);
      final token = register();
      final finalFile = File('${root.path}/$token.bin');
      await finalFile.writeAsBytes([9, 8, 7]);
      var downloads = 0;
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, ___) {
          downloads++;
          return const Stream<api.TransferProgress>.empty();
        },
        sizeResolver: (_) async => 1,
      );

      final result = await _fetch(cache, token);

      expect(downloads, 0);
      expect(result.reused, isTrue);
      expect(await finalFile.readAsBytes(), [9, 8, 7]);
    });

    test('rejects an existing final file over the per-file cap', () async {
      root.createSync(recursive: true);
      final token = register();
      final finalFile = File('${root.path}/$token.bin');
      await _writeSparse(finalFile.path, AttachmentCache.maxFileBytes + 1);
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, ___) {
          throw StateError('must not download');
        },
        sizeResolver: (_) async => 0,
      );

      await expectLater(
        _fetch(cache, token),
        throwsA(isA<AttachmentTooLargeFailure>()),
      );
      expect(finalFile.existsSync(), isFalse);
    });

    test('treats reported size as an upper bound for a smaller download',
        () async {
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await File(path).writeAsBytes([1, 2, 3]);
          yield const api.TransferProgress(prog: 3, total: 3);
        },
        sizeResolver: (_) async => 10,
      );
      final token = register();

      final result = await _fetch(cache, token);
      expect(result.bytes, 3);
      expect(await result.file.readAsBytes(), [1, 2, 3]);
    });

    test('concurrent fetches share one download', () async {
      final gate = Completer<void>();
      var downloads = 0;
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          downloads++;
          await gate.future;
          await File(path).writeAsBytes([4, 5]);
          yield const api.TransferProgress(prog: 2, total: 2);
        },
        sizeResolver: (_) async => 2,
      );
      final token = register();

      final first = _fetch(cache, token);
      final second = _fetch(cache, token);
      gate.complete();
      final results = await Future.wait([first, second]);

      expect(downloads, 1);
      expect(results[0].file.path, results[1].file.path);
      expect(await results[0].file.readAsBytes(), [4, 5]);
    });

    test('quota eviction removes oldest files but keeps the new one', () async {
      root.createSync(recursive: true);
      final oldPaths = <String>[];
      final now = DateTime.now();
      for (var index = 0; index < 9; index++) {
        final name = (index + 100).toRadixString(16).padLeft(32, '0');
        final path = '${root.path}/$name.bin';
        await _writeSparse(path, 60 * 1024 * 1024);
        await File(path)
            .setLastModified(now.subtract(Duration(days: index + 1)));
        oldPaths.add(path);
      }

      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await File(path).writeAsBytes([1, 2, 3]);
          yield const api.TransferProgress(prog: 3, total: 3);
        },
        sizeResolver: (_) async => 3,
      );
      final token = register();

      final result = await _fetch(cache, token);

      expect(File(oldPaths.last).existsSync(), isFalse);
      expect(File(oldPaths.first).existsSync(), isTrue);
      expect(result.file.existsSync(), isTrue);
      final total = root
          .listSync()
          .whereType<File>()
          .fold<int>(0, (sum, file) => sum + file.statSync().size);
      expect(total, lessThanOrEqualTo(AttachmentCache.maxTotalBytes));
    });

    test('evicts expired files and respects the entry bound', () async {
      root.createSync(recursive: true);
      final now = DateTime.now();
      final expired = '${root.path}/${'1' * 32}.bin';
      await File(expired).writeAsBytes([1]);
      await File(expired)
          .setLastModified(now.subtract(const Duration(days: 31)));

      for (var index = 0; index < 128; index++) {
        final name = (index + 200).toRadixString(16).padLeft(32, '0');
        final path = '${root.path}/$name.bin';
        await File(path).writeAsBytes([1]);
        await File(path)
            .setLastModified(now.subtract(Duration(minutes: index)));
      }

      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await File(path).writeAsBytes([1, 2]);
          yield const api.TransferProgress(prog: 2, total: 2);
        },
        sizeResolver: (_) async => 2,
      );
      final token = register();

      await _fetch(cache, token);

      expect(File(expired).existsSync(), isFalse);
      final count = root.listSync().whereType<File>().length;
      expect(count, lessThanOrEqualTo(AttachmentCache.maxEntries));
      expect(File('${root.path}/$token.bin').existsSync(), isTrue);
    });

    test('failures and debug strings never include attachment data', () async {
      final cache = AttachmentCache(
        root: root,
        registry: registry,
        downloader: (_, __, path) async* {
          await File(path).writeAsBytes([1]);
          throw StateError('super-secret-payload');
        },
        sizeResolver: (_) async => 1,
      );
      final token = register(
        name: 'secret-name',
        mime: 'secret/mime',
        uti: 'secret.uti',
      );

      try {
        await _fetch(cache, token);
        fail('expected a failure');
      } on AttachmentFailure catch (failure) {
        final text = failure.toString();
        expect(text, isNot(contains('super-secret-payload')));
        expect(text, isNot(contains('secret-name')));
        expect(text, isNot(contains('secret/mime')));
        expect(text, isNot(contains('secret.uti')));
      }
      expect(cache.toString(), isNot(contains('secret-name')));
      expect(registry.toString(), isNot(contains('secret-name')));
    });
  });
}
