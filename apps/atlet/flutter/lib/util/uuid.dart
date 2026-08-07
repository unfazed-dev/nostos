import 'dart:math';

/// RFC 4122 version-4 UUID from a cryptographically secure source.
///
/// The Postgres tables (`sessions`, `cart_items`, `orders`) use `uuid`
/// primary keys; client-generated ids MUST be valid UUIDs or the
/// server-side write-back rejects the row with
/// `invalid input syntax for type uuid` (root cause of the 2026-08-07
/// dead-lettered outbox writes — do not regress to `s-<micros>` ids).
String uuidV4() {
  final rng = Random.secure();
  final bytes = List<int>.generate(16, (_) => rng.nextInt(256));
  bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
  bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
  final hex =
      bytes.map((b) => b.toRadixString(16).padLeft(2, '0')).join();
  return '${hex.substring(0, 8)}-${hex.substring(8, 12)}-'
      '${hex.substring(12, 16)}-${hex.substring(16, 20)}-'
      '${hex.substring(20)}';
}
