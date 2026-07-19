// Billing calculation engine — pure functions, no I/O.
//
// Computes invoice amounts from a provider's rate type + an appointment's
// duration, and builds the full invoice payload with a RATE SNAPSHOT captured
// at issue time. The snapshot is the canonical billing pattern (confirmed via
// research: DBA-SE "modeling invoicing systems", dev.to multi-tenant billing):
// a provider changing their rate later never re-prices a historical invoice,
// because the rate + line type + hours are frozen in the invoice row.
//
// All arithmetic is integer cents × integer minutes — no floats, no NUMERIC.
// This sidesteps the write-back's INT4/NUMERIC bind limitation entirely
// (write_back.rs binds i64→INT8; BIGINT columns match natively).

import '../models.dart';

/// The result of a billing calculation: the amount + the snapshot details to
/// persist on the invoice row.
class BillingResult {
  const BillingResult({
    required this.amountCents,
    required this.lineType,
    required this.rateCents,
    required this.hoursMin,
  });

  final int amountCents;
  final RateType lineType;
  final int rateCents; // snapshot of the provider's active rate at issue time
  final int hoursMin; // minutes billed (from appointment duration)

  String get description => switch (lineType) {
        RateType.hourly =>
          'Consultation — ${formatHours(hoursMin)} @ ${formatCents(rateCents)}/hr',
        RateType.flat => 'Service — flat fee ${formatCents(rateCents)}',
        RateType.subscription =>
          'Subscription — ${formatCents(rateCents)}/mo',
      };
}

class BillingService {
  const BillingService._();

  /// Calculate the invoice amount (in cents) for an appointment based on the
  /// provider's rate type and the appointment's duration.
  ///
  /// - hourly: prorated — (duration_min × hourly_rate_cents) / 60, rounded.
  /// - flat: fixed, duration-independent.
  /// - subscription: fixed recurring (the appointment is covered by the sub).
  static int calculateAmount({
    required Provider provider,
    required int durationMinutes,
  }) {
    switch (provider.rateType) {
      case RateType.hourly:
        // Prorate: minutes × cents-per-hour / 60. Round to nearest cent.
        // e.g. 60min × 25000¢/hr / 60 = 25000¢ = $250.00
        return (durationMinutes * provider.hourlyRateCents / 60).round();
      case RateType.flat:
        return provider.flatRateCents;
      case RateType.subscription:
        return provider.subscriptionRateCents;
    }
  }

  /// Build the full BillingResult (amount + rate snapshot) for an appointment.
  /// The snapshot captures the provider's rate at issue time so the invoice
  /// is immutable to later rate changes.
  static BillingResult calculate({
    required Provider provider,
    required int durationMinutes,
  }) {
    final amount = calculateAmount(
      provider: provider,
      durationMinutes: durationMinutes,
    );
    return BillingResult(
      amountCents: amount,
      lineType: provider.rateType,
      rateCents: provider.activeRateCents,
      hoursMin: provider.rateType == RateType.hourly ? durationMinutes : 0,
    );
  }

  /// Build the full invoice write payload (for `db.write(table: 'invoices', ...)`),
  /// including the rate snapshot fields. The caller supplies the appointment +
  /// client + provider IDs; the billing engine fills in the money + line detail.
  static Map<String, dynamic> buildInvoicePayload({
    required String appointmentId,
    required String clientId,
    required String providerId,
    required BillingResult billing,
    DateTime? issuedAt,
  }) {
    final now = DateTime.now().toUtc();
    return <String, dynamic>{
      'appointment_id': appointmentId,
      'client_id': clientId,
      'provider_id': providerId,
      'amount_cents': billing.amountCents,
      'line_type': billing.lineType.name,
      'rate_cents': billing.rateCents,
      'hours_min': billing.hoursMin,
      'description': billing.description,
      'status': 'issued',
      'issued_at': (issuedAt ?? now).toIso8601String(),
      'created_at': now.toIso8601String(),
    };
  }
}
