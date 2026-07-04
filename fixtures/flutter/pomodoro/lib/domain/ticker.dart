/// The clock port. The ViewModel consumes ticks through this seam so unit
/// tests can drive time by hand (mocktail + a StreamController) and never
/// wait on a real clock.
abstract interface class Ticker {
  /// Emits once per second while listened to.
  Stream<void> ticks();
}

/// Production clock: one tick per wall-clock second.
class SystemTicker implements Ticker {
  @override
  Stream<void> ticks() => Stream<void>.periodic(const Duration(seconds: 1));
}
