import 'dart:typed_data';

class SessionRow {
  final String id;
  final String title;
  final String type;
  final int metric;
  final String unit;
  final String? note;
  final int streak;
  final DateTime occurredOn;
  final DateTime? serverCommittedAt;

  const SessionRow({
    required this.id,
    required this.title,
    required this.type,
    required this.metric,
    required this.unit,
    this.note,
    this.streak = 0,
    required this.occurredOn,
    this.serverCommittedAt,
  });
}

class ProductRow {
  final String id;
  final String name;
  final String category;
  final int priceCents;
  final double? rating;
  final bool plantBased;
  final String? imageUrl;

  /// `attachments.id` = the Supabase Storage object key (migration 0012).
  /// Null on rows seeded before images moved off the bundle.
  final String? imageId;

  const ProductRow({
    required this.id,
    required this.name,
    required this.category,
    required this.priceCents,
    this.rating,
    required this.plantBased,
    this.imageUrl,
    this.imageId,
  });
}

/// Public admin view of an Appwrite Auth account's synced profile.
class UserProfileRow {
  const UserProfileRow({
    required this.id,
    required this.displayName,
    required this.active,
  });

  final String id;
  final String displayName;
  final bool active;
}

class CartItemRow {
  final String id;
  final String productId;
  final int qty;
  final DateTime addedAt;

  const CartItemRow({
    required this.id,
    required this.productId,
    required this.qty,
    required this.addedAt,
  });
}

class OrderRow {
  final String id;
  final String
  status; // 'pending' | 'paid' | 'failed' | 'shipped' | 'delivered'
  final String? userId;
  final int subtotalCents;
  final int taxCents;
  final int shippingCents;
  final int totalCents;
  final String? paymentRef;
  final String? itemsJson; // snapshot of purchased lines, JSON text
  final DateTime createdAt;

  const OrderRow({
    required this.id,
    required this.status,
    this.userId,
    required this.subtotalCents,
    required this.taxCents,
    required this.shippingCents,
    required this.totalCents,
    this.paymentRef,
    this.itemsJson,
    required this.createdAt,
  });
}

/// One entry in the shop's history: an order reached a status at a time.
///
/// An [OrderRow] only carries its CURRENT status, so the sequence that led
/// there — and any notification the user missed along the way — lived nowhere.
/// Server-written (a trigger on `public.orders`, migration 0007), read-only on
/// the device.
class OrderEventRow {
  final String id;
  final String orderId;
  final String status;

  /// Null on the row that records the order's creation.
  final String? previousStatus;
  final String? note;
  final DateTime createdAt;

  const OrderEventRow({
    required this.id,
    required this.orderId,
    required this.status,
    this.previousStatus,
    this.note,
    required this.createdAt,
  });
}

enum MarkKind { localVisible, serverAcked, remoteVisible }

class SyncMark {
  final MarkKind kind;
  final String rowId;
  final Duration tMono; // from bench clock
  final DateTime? serverCommittedAt;

  const SyncMark(this.kind, this.rowId, this.tMono, {this.serverCommittedAt});
}

/// Wraps a broadcast [tail] so each new listener first receives the most
/// recent value from [latest] (if any), then live events. Adapters feed
/// their `db.watch(...)` streams into broadcast StreamControllers, which do
/// NOT replay: a listener attaching after an emission never sees it. For a
/// read-only table like `products` there is exactly one snapshot emission
/// (at init(), into the adapter's own central subscription) — so a
/// ShopScreen built after an engine switch subscribed too late and spun on
/// its loading state forever. Subscribes to [tail] BEFORE emitting the
/// cached value so no live event falls in a gap; a duplicate initial
/// snapshot is possible and harmless (idempotent list re-render).
/// Top-level (not a private adapter method) so the late-subscriber
/// regression is unit-testable without a live engine — same rationale as
/// `wireConnectionState` in nostos_adapter.dart.
Stream<T> replayLatest<T>(Stream<T> tail, T? Function() latest) =>
    Stream<T>.multi((controller) {
      final sub = tail.listen(
        controller.add,
        onError: controller.addError,
        onDone: controller.close,
      );
      final v = latest();
      if (v != null) {
        controller.add(v);
      }
      controller.onCancel = sub.cancel;
    });

abstract interface class SyncAdapter {
  String get engine; // 'nostos' | 'nostos-direct'
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  });
  Future<void> signOut(); // disconnect + delete local DB files (full wipe)
  Future<String> addSession(SessionRow s); // serverCommittedAt must be null
  Future<void> updateSession(SessionRow s); // upsert by existing id
  Future<void> deleteSession(String id);
  Stream<List<SessionRow>> watchSessions();
  Stream<List<ProductRow>> watchProducts();

  /// Bytes of a product image by [ProductRow.imageId], from the local blob
  /// cache or (online) the bucket. Null = not available yet; the UI falls
  /// back to the bundled asset.
  Future<Uint8List?> productImage(String imageId);

  // Shop flow — cart is per-user server state (tenant-scoped like sessions).
  Future<void> addToCart(CartItemRow item); // upsert (id may exist: qty edit)
  Future<void> removeCartItem(String id);
  Future<void> clearCart();
  Future<String> placeOrder(OrderRow o);
  Stream<List<CartItemRow>> watchCart();
  Stream<List<OrderRow>> watchOrders();

  /// Newest first. Server-written and read-only — there is no `addOrderEvent`.
  Stream<List<OrderEventRow>> watchOrderEvents();
  Stream<bool> get connected;
  Future<void> setConnected(bool up);
  Stream<SyncMark> get marks; // derived ONLY from watchSessions output
}
