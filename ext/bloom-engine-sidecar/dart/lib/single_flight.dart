/// Shares one in-flight asynchronous operation, then permits a later retry.
class SingleFlight<T> {
  Future<T>? _active;

  Future<T> run(Future<T> Function() operation) {
    final active = _active;
    if (active != null) return active;

    final running = operation();
    _active = running;
    return running.whenComplete(() {
      if (identical(_active, running)) _active = null;
    });
  }
}
