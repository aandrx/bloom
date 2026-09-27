import 'protocol.dart';

typedef SendFrameWriter = void Function(String frame);

/// Coordinates command returns and asynchronous engine confirmations by message ID.
class SendCoordinator {
  SendCoordinator(this._write);

  final SendFrameWriter _write;
  final Map<String, int> _pending = <String, int>{};

  void register(String messageId, int requestId) {
    _pending[messageId] = requestId;
  }

  bool completePush({required String messageId, String? error}) {
    final requestId = _pending.remove(messageId);
    if (requestId == null) return false;
    _write(sendConfirmFrame(
      requestId: requestId,
      messageId: messageId,
      accepted: error == null,
      error: error,
    ));
    return true;
  }

  void sendReturned({required String messageId, required bool stillRunning}) {
    if (stillRunning) return;
    final requestId = _pending.remove(messageId);
    if (requestId == null) return;
    _write(sendConfirmFrame(
      requestId: requestId,
      messageId: messageId,
      accepted: true,
    ));
  }

  bool discard(String messageId) => _pending.remove(messageId) != null;
}
