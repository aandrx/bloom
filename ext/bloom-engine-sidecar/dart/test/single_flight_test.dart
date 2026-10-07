import 'dart:async';

import 'package:bloom_engine_sidecar/single_flight.dart';
import 'package:test/test.dart';

void main() {
  test('shares active work and allows a new run after completion', () async {
    final flight = SingleFlight<int>();
    final completer = Completer<int>();
    var starts = 0;

    final first = flight.run(() {
      starts++;
      return completer.future;
    });
    final joined = flight.run(() {
      starts++;
      return Future<int>.value(99);
    });
    expect(starts, 1);

    completer.complete(7);
    expect(await first, 7);
    expect(await joined, 7);

    expect(
      await flight.run(() {
        starts++;
        return Future<int>.value(11);
      }),
      11,
    );
    expect(starts, 2);
  });

  test('clears failed work so the next call can retry', () async {
    final flight = SingleFlight<int>();
    var starts = 0;

    await expectLater(
      flight.run(() {
        starts++;
        return Future<int>.error(StateError('failed'));
      }),
      throwsA(isA<StateError>()),
    );
    expect(
      await flight.run(() {
        starts++;
        return Future<int>.value(3);
      }),
      3,
    );
    expect(starts, 2);
  });
}
