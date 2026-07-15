// Typed records for the 5 Provider Dashboard tables. Each `fromRow` decodes a
// row from the WS2 read-view (`SELECT * FROM <table>`): `_pk` is the stamped
// primary key; the rest are the json_extract'd payload columns projected by the
// view `SqliteStorage::apply_schema` materializes. Used by
// `NostosDatabase.watchMapped<T>` for reactive typed lists.
//
// Schema (docker/pg-init/01-sources.sql): every PK is UUID (gen_random_uuid);
// FKs all ON DELETE CASCADE. TEXT status columns decode as String; integer
// columns (weekday/start_min/duration_min/amount_cents) as int (json_extract
// typing against a real-Postgres JSON payload).

import 'dart:math' as math;

/// RFC-4122 v4 UUID from a local RNG — the demo adds no `uuid` dependency. The
/// 122 random bits make collisions negligible; the write-back binds `pk` as the
/// row's UUID `id` column.
final _rng = math.Random();
String uuidV4() {
  final b = List<int>.generate(16, (_) => _rng.nextInt(256));
  b[6] = (b[6] & 0x0f) | 0x40; // version 4
  b[8] = (b[8] & 0x3f) | 0x80; // variant 10
  final h = b.map((x) => x.toRadixString(16).padLeft(2, '0')).join();
  return '${h.substring(0, 8)}-${h.substring(8, 12)}-${h.substring(12, 16)}-'
      '${h.substring(16, 20)}-${h.substring(20)}';
}

int _toInt(dynamic v) => v is int ? v : int.tryParse(v?.toString() ?? '') ?? 0;
String? _toStr(dynamic v) => v?.toString();

class Provider {
  const Provider({
    required this.id,
    required this.name,
    this.specialty,
    this.email,
    this.phone,
  });
  final String id;
  final String name;
  final String? specialty;
  final String? email;
  final String? phone;

  factory Provider.fromRow(Map<String, dynamic> r) => Provider(
        id: (r['_pk'] ?? r['id']).toString(),
        name: r['name'].toString(),
        specialty: _toStr(r['specialty']),
        email: _toStr(r['email']),
        phone: _toStr(r['phone']),
      );
}

class Client {
  const Client({
    required this.id,
    required this.name,
    this.email,
    this.phone,
    this.notes,
  });
  final String id;
  final String name;
  final String? email;
  final String? phone;
  final String? notes;

  factory Client.fromRow(Map<String, dynamic> r) => Client(
        id: (r['_pk'] ?? r['id']).toString(),
        name: r['name'].toString(),
        email: _toStr(r['email']),
        phone: _toStr(r['phone']),
        notes: _toStr(r['notes']),
      );
}

class Availability {
  const Availability({
    required this.id,
    required this.providerId,
    required this.weekday,
    required this.startMin,
    required this.endMin,
  });
  final String id;
  final String providerId;
  final int weekday;
  final int startMin;
  final int endMin;

  factory Availability.fromRow(Map<String, dynamic> r) => Availability(
        id: (r['_pk'] ?? r['id']).toString(),
        providerId: (r['provider_id'] ?? '').toString(),
        weekday: _toInt(r['weekday']),
        startMin: _toInt(r['start_min']),
        endMin: _toInt(r['end_min']),
      );

  static const _days = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
  String get day => _days[weekday.clamp(0, 6)];
  String get range => '${_hhmm(startMin)}–${_hhmm(endMin)}';
  static String _hhmm(int m) =>
      '${(m ~/ 60).toString().padLeft(2, '0')}:${(m % 60).toString().padLeft(2, '0')}';
}

class Appointment {
  const Appointment({
    required this.id,
    required this.providerId,
    required this.clientId,
    required this.startsAt,
    required this.durationMin,
    required this.status,
    this.notes,
  });
  final String id;
  final String providerId;
  final String clientId;
  final String startsAt;
  final int durationMin;
  final String status;
  final String? notes;

  factory Appointment.fromRow(Map<String, dynamic> r) => Appointment(
        id: (r['_pk'] ?? r['id']).toString(),
        providerId: (r['provider_id'] ?? '').toString(),
        clientId: (r['client_id'] ?? '').toString(),
        startsAt: (r['starts_at'] ?? '').toString(),
        durationMin: _toInt(r['duration_min']),
        status: (r['status'] ?? 'confirmed').toString(),
        notes: _toStr(r['notes']),
      );
}

class Invoice {
  const Invoice({
    required this.id,
    required this.appointmentId,
    required this.clientId,
    required this.amountCents,
    required this.status,
    this.issuedAt,
  });
  final String id;
  final String appointmentId;
  final String clientId;
  final int amountCents;
  final String status;
  final String? issuedAt;

  factory Invoice.fromRow(Map<String, dynamic> r) => Invoice(
        id: (r['_pk'] ?? r['id']).toString(),
        appointmentId: (r['appointment_id'] ?? '').toString(),
        clientId: (r['client_id'] ?? '').toString(),
        amountCents: _toInt(r['amount_cents']),
        status: (r['status'] ?? 'issued').toString(),
        issuedAt: _toStr(r['issued_at']),
      );

  String get amount => '\$${(amountCents / 100).toStringAsFixed(2)}';
}
