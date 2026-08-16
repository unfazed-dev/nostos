// Typed presentation models for the Nostos Provider Dashboard tables. Each
// `fromRow` decodes a row from the WS2 read-view (`SELECT * FROM <table>`):
// `_pk` is the stamped primary key; the rest are the json_extract'd payload
// columns projected by the view `SqliteStorage::apply_schema` materializes.
// Consumed by `NostosDatabase.collection<T>(table, fromRow: ...)` (ADR-0024).
//
// The generated `nostos.g.dart` model classes exist too (nostos gen), but THIS
// file is the presentation layer the UI consumes — it adds computed getters
// (formatted money, hours, rate labels) and parses the BIGINT-as-text columns
// (rate_cents, hours_min, *_rate_cents) into real `int`s for arithmetic.

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

int _toInt(dynamic v) {
  if (v is int) return v;
  if (v is num) return v.toInt();
  return int.tryParse(v?.toString() ?? '') ?? 0;
}

String? _toStr(dynamic v) => v?.toString();

/// Format an integer amount of cents as a USD string: 25000 → "$250.00".
String formatCents(int cents) {
  final dollars = cents ~/ 100;
  final remainingCents = cents % 100;
  return '\$$dollars.${remainingCents.toString().padLeft(2, '0')}';
}

/// Format an integer number of minutes as an hours string: 60 → "1.0h", 90 → "1.5h".
String formatHours(int minutes) {
  final hours = minutes / 60.0;
  return '${hours.toStringAsFixed(hours.truncateToDouble() == hours ? 1 : 2)}h';
}

/// The rate-type discriminator governing how a provider's invoices are
/// calculated (see BillingService).
enum RateType {
  hourly('Hourly'),
  flat('Flat fee'),
  subscription('Subscription');

  const RateType(this.label);
  final String label;

  static RateType fromString(String? s) {
    switch (s) {
      case 'flat':
        return RateType.flat;
      case 'subscription':
        return RateType.subscription;
      default:
        return RateType.hourly;
    }
  }
}

// ── Providers ───────────────────────────────────────────────────────────────

class Provider {
  const Provider({
    required this.id,
    required this.name,
    this.specialty,
    this.email,
    this.phone,
    this.rateType = RateType.hourly,
    this.hourlyRateCents = 0,
    this.flatRateCents = 0,
    this.subscriptionRateCents = 0,
    this.bio,
    this.avatarColor = '#2E6FDB',
  });

  final String id;
  final String name;
  final String? specialty;
  final String? email;
  final String? phone;
  final RateType rateType;
  final int hourlyRateCents;
  final int flatRateCents;
  final int subscriptionRateCents;
  final String? bio;
  final String avatarColor;

  factory Provider.fromRow(Map<String, dynamic> r) => Provider(
    id: (r['_pk'] ?? r['id']).toString(),
    name: (r['name'] ?? '').toString(),
    specialty: _toStr(r['specialty']),
    email: _toStr(r['email']),
    phone: _toStr(r['phone']),
    rateType: RateType.fromString(_toStr(r['rate_type'])),
    hourlyRateCents: _toInt(r['hourly_rate_cents']),
    flatRateCents: _toInt(r['flat_rate_cents']),
    subscriptionRateCents: _toInt(r['subscription_rate_cents']),
    bio: _toStr(r['bio']),
    avatarColor: _toStr(r['avatar_color']) ?? '#2E6FDB',
  );

  /// The rate (in cents) relevant to this provider's current rate type.
  int get activeRateCents => switch (rateType) {
    RateType.hourly => hourlyRateCents,
    RateType.flat => flatRateCents,
    RateType.subscription => subscriptionRateCents,
  };

  /// Human-readable summary of the provider's pricing, e.g. "$250/hr".
  String get rateLabel => switch (rateType) {
    RateType.hourly => '${formatCents(hourlyRateCents)}/hr',
    RateType.flat => '${formatCents(flatRateCents)}/visit',
    RateType.subscription => '${formatCents(subscriptionRateCents)}/mo',
  };

  /// Parse the avatar color hex to a 0xAARRGGBB int (default blue if invalid).
  int get avatarColorValue {
    final hex = avatarColor.replaceFirst('#', '');
    final val = int.tryParse(hex, radix: 16);
    if (val == null) return 0xFF2E6FDB;
    // If 6-digit RGB, prepend FF alpha.
    return hex.length == 6 ? 0xFF000000 | val : val;
  }

  /// The provider's initials for the avatar circle.
  String get initials {
    final parts = name.replaceAll(RegExp(r'^Dr\.\s*'), '').split(' ');
    if (parts.length >= 2) {
      return '${parts[0][0]}${parts[1][0]}'.toUpperCase();
    }
    return name.isNotEmpty ? name[0].toUpperCase() : '?';
  }

  Map<String, dynamic> toPayload() => {
    'id': id,
    'name': name,
    'specialty': specialty,
    'email': email,
    'phone': phone,
    'rate_type': rateType.name,
    'hourly_rate_cents': hourlyRateCents,
    'flat_rate_cents': flatRateCents,
    'subscription_rate_cents': subscriptionRateCents,
    'bio': bio,
    'avatar_color': avatarColor,
  };
}

// ── Clients ─────────────────────────────────────────────────────────────────

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
    name: (r['name'] ?? '').toString(),
    email: _toStr(r['email']),
    phone: _toStr(r['phone']),
    notes: _toStr(r['notes']),
  );

  String get initials {
    final parts = name.split(' ');
    if (parts.length >= 2) {
      return '${parts[0][0]}${parts[1][0]}'.toUpperCase();
    }
    return name.isNotEmpty ? name[0].toUpperCase() : '?';
  }
}

// ── Availabilities ──────────────────────────────────────────────────────────

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

  /// "Mon 09:00–12:00" — the full one-line summary.
  String get summary => '$day $range';

  static String _hhmm(int m) =>
      '${(m ~/ 60).toString().padLeft(2, '0')}:${(m % 60).toString().padLeft(2, '0')}';
}

// ── Appointments ────────────────────────────────────────────────────────────

/// Appointment status lifecycle: confirmed → completed | cancelled | no_show.
enum AppointmentStatus {
  confirmed('Confirmed'),
  completed('Completed'),
  cancelled('Cancelled'),
  noShow('No-show');

  const AppointmentStatus(this.label);
  final String label;

  static AppointmentStatus fromString(String? s) {
    switch (s) {
      case 'completed':
        return AppointmentStatus.completed;
      case 'cancelled':
        return AppointmentStatus.cancelled;
      case 'no_show':
        return AppointmentStatus.noShow;
      default:
        return AppointmentStatus.confirmed;
    }
  }
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
  final AppointmentStatus status;
  final String? notes;

  factory Appointment.fromRow(Map<String, dynamic> r) => Appointment(
    id: (r['_pk'] ?? r['id']).toString(),
    providerId: (r['provider_id'] ?? '').toString(),
    clientId: (r['client_id'] ?? '').toString(),
    startsAt: (r['starts_at'] ?? '').toString(),
    durationMin: _toInt(r['duration_min']),
    status: AppointmentStatus.fromString(_toStr(r['status'])),
    notes: _toStr(r['notes']),
  );

  /// Parse starts_at into a DateTime (null if unparseable).
  DateTime? get startsAtDate => DateTime.tryParse(startsAt);

  /// Formatted date+time: "Jul 22, 9:00 AM" (null if unparseable).
  String get formattedStart {
    final d = startsAtDate;
    if (d == null) return startsAt.isEmpty ? '(unscheduled)' : startsAt;
    final month = [
      'Jan',
      'Feb',
      'Mar',
      'Apr',
      'May',
      'Jun',
      'Jul',
      'Aug',
      'Sep',
      'Oct',
      'Nov',
      'Dec',
    ][d.month - 1];
    final hour = d.hour == 0 ? 12 : (d.hour > 12 ? d.hour - 12 : d.hour);
    final amPm = d.hour >= 12 ? 'PM' : 'AM';
    return '$month ${d.day}, $hour:${d.minute.toString().padLeft(2, '0')} $amPm';
  }
}

// ── Invoices ────────────────────────────────────────────────────────────────

/// Invoice status lifecycle: issued → paid | void | refunded.
enum InvoiceStatus {
  issued('Issued'),
  paid('Paid'),
  voided('Void'),
  refunded('Refunded');

  const InvoiceStatus(this.label);
  final String label;

  static InvoiceStatus fromString(String? s) {
    switch (s) {
      case 'paid':
        return InvoiceStatus.paid;
      case 'void':
        return InvoiceStatus.voided;
      case 'refunded':
        return InvoiceStatus.refunded;
      default:
        return InvoiceStatus.issued;
    }
  }
}

class Invoice {
  const Invoice({
    required this.id,
    required this.appointmentId,
    required this.clientId,
    required this.amountCents,
    required this.status,
    this.providerId,
    this.lineType = RateType.hourly,
    this.rateCents = 0,
    this.hoursMin = 0,
    this.description,
    this.issuedAt,
    this.dueAt,
    this.paidAt,
  });

  final String id;
  final String appointmentId;
  final String clientId;
  final int amountCents;
  final InvoiceStatus status;
  final String? providerId;
  final RateType lineType;
  final int rateCents;
  final int hoursMin;
  final String? description;
  final String? issuedAt;
  final String? dueAt;
  final String? paidAt;

  factory Invoice.fromRow(Map<String, dynamic> r) => Invoice(
    id: (r['_pk'] ?? r['id']).toString(),
    appointmentId: (r['appointment_id'] ?? '').toString(),
    clientId: (r['client_id'] ?? '').toString(),
    amountCents: _toInt(r['amount_cents']),
    status: InvoiceStatus.fromString(_toStr(r['status'])),
    providerId: _toStr(r['provider_id']),
    lineType: RateType.fromString(_toStr(r['line_type'])),
    rateCents: _toInt(r['rate_cents']),
    hoursMin: _toInt(r['hours_min']),
    description: _toStr(r['description']),
    issuedAt: _toStr(r['issued_at']),
    dueAt: _toStr(r['due_at']),
    paidAt: _toStr(r['paid_at']),
  );

  String get amount => formatCents(amountCents);
  String get rateFormatted => formatCents(rateCents);
  String get hoursFormatted => formatHours(hoursMin);

  /// A human-readable breakdown of the billing line, e.g.
  /// "1.0 hr @ $250/hr" or "Flat fee" or "Monthly subscription".
  String get lineSummary => switch (lineType) {
    RateType.hourly => '$hoursFormatted @ ${formatCents(rateCents)}/hr',
    RateType.flat => 'Flat fee — ${formatCents(rateCents)}',
    RateType.subscription => 'Monthly — ${formatCents(rateCents)}',
  };
}

// ── Messages (realtime chat) ────────────────────────────────────────────────

/// Who sent a message in a provider↔client thread.
enum SenderType {
  provider('Provider'),
  client('Client');

  const SenderType(this.label);
  final String label;

  static SenderType fromString(String? s) =>
      s == 'client' ? SenderType.client : SenderType.provider;
}

class Message {
  const Message({
    required this.id,
    required this.providerId,
    required this.clientId,
    required this.senderType,
    required this.senderId,
    required this.body,
    required this.createdAt,
    this.readAt,
  });

  final String id;
  final String providerId;
  final String clientId;
  final SenderType senderType;
  final String senderId;
  final String body;
  final String createdAt;
  final String? readAt;

  factory Message.fromRow(Map<String, dynamic> r) => Message(
    id: (r['_pk'] ?? r['id']).toString(),
    providerId: (r['provider_id'] ?? '').toString(),
    clientId: (r['client_id'] ?? '').toString(),
    senderType: SenderType.fromString(_toStr(r['sender_type'])),
    senderId: (r['sender_id'] ?? '').toString(),
    body: (r['body'] ?? '').toString(),
    createdAt: (r['created_at'] ?? '').toString(),
    readAt: _toStr(r['read_at']),
  );

  bool get isFromProvider => senderType == SenderType.provider;
  bool get isRead => readAt != null;

  /// "3:05 PM" — short time for the chat bubble.
  String get timeLabel {
    final d = DateTime.tryParse(createdAt);
    if (d == null) return '';
    final hour = d.hour == 0 ? 12 : (d.hour > 12 ? d.hour - 12 : d.hour);
    final amPm = d.hour >= 12 ? 'PM' : 'AM';
    return '$hour:${d.minute.toString().padLeft(2, '0')} $amPm';
  }
}

/// A chat thread: the unique (provider_id, client_id) pair with its latest
/// message preview. Derived client-side from the messages watch stream.
class ChatThread {
  const ChatThread({
    required this.providerId,
    required this.clientId,
    required this.lastMessage,
    required this.messageCount,
  });

  final String providerId;
  final String clientId;
  final Message lastMessage;
  final int messageCount;

  /// A stable key for this thread (used by ListView keys + lookups).
  String get key => '$providerId:$clientId';
}
