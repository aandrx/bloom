/// Classify an engine initialization failure without exposing raw exception text.
///
/// The Flutter Rust Bridge content hash check is the real compatibility gate:
/// the installed library and the generated bindings must come from the same
/// OpenBubbles tag. The returned code is bounded and safe to log.
String classifyEngineInitFailure(Object error) {
  final text = error.toString();
  if (text.contains('Content hash on Dart side') ||
      text.contains('out-of-sync code')) {
    return 'content_hash_mismatch';
  }
  if (text.contains('codegen version')) {
    return 'codegen_version_mismatch';
  }
  if (text.contains('Failed to load dynamic library') ||
      text.contains('Could not load dynamic library') ||
      text.contains('dlopen')) {
    return 'library_load_failed';
  }
  return 'engine_init_failed';
}
