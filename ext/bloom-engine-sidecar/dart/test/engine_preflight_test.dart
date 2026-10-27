import 'package:bloom_engine_sidecar/engine_preflight.dart';
import 'package:test/test.dart';

void main() {
  test('classifies an FRB content-hash mismatch', () {
    expect(
      classifyEngineInitFailure(
        StateError(
          'Content hash on Dart side (1) is different from Rust side (2), '
          'indicating out-of-sync code.',
        ),
      ),
      'content_hash_mismatch',
    );
  });

  test('classifies a codegen/runtime mismatch', () {
    expect(
      classifyEngineInitFailure(
        StateError(
          "flutter_rust_bridge's codegen version (2.3.0) should be the same "
          'as runtime version (2.4.0).',
        ),
      ),
      'codegen_version_mismatch',
    );
  });

  test('classifies an unloadable library', () {
    expect(
      classifyEngineInitFailure(
        Exception('Failed to load dynamic library: librust_lib_bluebubbles.so'),
      ),
      'library_load_failed',
    );
  });

  test('falls back to a bounded generic initialization code', () {
    expect(
      classifyEngineInitFailure(Exception('opaque engine failure')),
      'engine_init_failed',
    );
  });
}
