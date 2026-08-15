import 'dart:async';

import 'package:nostos_flutter/nostos_flutter.dart';

import '../bench/marks.dart';
import 'sync_adapter.dart';

/// Nostos engine implementation of [SyncAdapter] for the Atlet pilot.
///
/// Wraps [NostosDatabase] (sdk/nostos_flutter/lib/src/nostos_database.dart).
/// Row mapping and the write payload are pure top-level functions below so
/// they're unit-testable without the native Rust bridge — see
/// nostos_adapter_test.dart.
class NostosAdapter implements SyncAdapter {
  @override
  final String engine = 'cairn';

  /// nostos-server `/sync` endpoint for the Atlet local profile
  /// (docker-compose.atlet.yml binds nostos-server on 0.0.0.0:8080; `/sync`
  /// is NOSTOS_WS_PATH's default in crates/nostos-server/src/main.rs).
  static const String _nostosUrl = String.fromEnvironment(
    'NOSTOS_SYNC_URL',
    defaultValue: 'ws://localhost:8080/sync',
  );

  // Created once, never recreated: the conformance test's `marks` listener
  // is attached before signOut() and must keep seeing emissions after a
  // second init(), so _deriver must outlive individual sync sessions.
  final MarkDeriver _deriver = MarkDeriver(Stopwatch()..start());

  NostosDatabase? _db;
  StreamSubscription<List<Map<String, dynamic>>>? _sessionsSub;
  StreamSubscription<List<ProductRow>>? _productsSub;
  StreamSubscription<NostosConnectionState>? _connSub;
  StreamController<List<SessionRow>>? _sessionsController;
  StreamController<List<ProductRow>>? _productsController;
  StreamController<bool>? _connectedController;

  // Latest values, replayed to late subscribers via replayLatest (see
  // sync_adapter.dart for the full failure mode: broadcast controllers do
  // not replay, and `products` emits exactly one snapshot, so a ShopScreen
  // built after an engine switch spun forever). The SDK's watch() replays
  // hot values; this layer must not discard that property.
  List<SessionRow>? _lastSessions;
  List<ProductRow>? _lastProducts;
  List<CartItemRow>? _lastCart;
  List<OrderRow>? _lastOrders;
  bool? _lastConnected;
  StreamController<List<CartItemRow>>? _cartController;
  StreamController<List<OrderRow>>? _ordersController;
  StreamSubscription<dynamic>? _cartSub;
  StreamSubscription<dynamic>? _ordersSub;

  /// Signed-in user id, stamped into cart/order write payloads because
  /// those tables are `user_id NOT NULL DEFAULT auth.uid()` and nostos-server
  /// writes over a direct PG connection where `auth.uid()` is NULL (tenant
  /// stamping is off — `products` is a global table on the same connection).
  String? _userId;

  /// Access token the live session was opened with. Kept only for the push
  /// pilot's background-isolate wake (see push/push_pilot.dart); cleared in
  /// signOut with everything else.
  String? _accessToken;

  /// True only between the end of a successful init() and signOut().
  /// setConnected() no-ops outside that window — see its comment.
  bool _ready = false;

  @override
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) async {
    _userId = userId;
    _accessToken = accessToken;
    _sessionsController = StreamController<List<SessionRow>>.broadcast();
    _productsController = StreamController<List<ProductRow>>.broadcast();
    _cartController = StreamController<List<CartItemRow>>.broadcast();
    _ordersController = StreamController<List<OrderRow>>.broadcast();
    _connectedController = StreamController<bool>.broadcast();

    final db = await NostosDatabase.connect(
      url: _nostosUrl,
      token: accessToken,
      schema: _schema,
      sqlitePath: '$dbDir/cairn.sqlite',
    );
    _db = db;

    // Attach BEFORE subscribeTables(): Nostos.connectionState is a
    // non-replaying broadcast stream that is empty until the engine's
    // subscribe has run at least once — a listener attached after
    // subscribeTables() can miss the very first `connected` transition that
    // fires synchronously inside it, so `connected` would never emit true
    // until the next disconnect/resume cycle. See wireConnectionState below.
    _connSub = wireConnectionState(db.connectionState, (isConnected) {
      _lastConnected = isConnected;
      _connectedController?.add(isConnected);
    });

    await db.subscribeTables(const [
      NostosTableSub(name: 'sessions'),
      NostosTableSub(name: 'products'),
      NostosTableSub(name: 'cart_items'),
      NostosTableSub(name: 'orders'),
    ]);

    // Typed collection handles (ADR-0032 T2): the taught surface for "table,
    // maybe filter, maybe order" reads. Injection-safe by construction.
    // (No sessions handle — its sort needs watchSql, see below.)
    final products = db.collection<ProductRow>(
        table: 'products', fromRow: productFromRow);
    final cartItems = db.collection<CartItemRow>(
        table: 'cart_items', fromRow: cartItemFromRow);
    final orders =
        db.collection<OrderRow>(table: 'orders', fromRow: orderFromRow);

    // sessions: the sort needs `(server_committed_at IS NULL) DESC`, an
    // expression the structured `Order` (field+direction) can't express yet —
    // contract gap. Routed through the raw-SQL escape hatch [watchSql]; the
    // other three reads use typed collections. See ADR-0032 "Escape hatch".
    // Newest first: latest day on top; within a day, the just-added row
    // (server_committed_at still NULL until acked) sorts above older ones.
    _sessionsSub = db
        .watchSql('SELECT * FROM sessions '
            'ORDER BY occurred_on DESC, '
            '(server_committed_at IS NULL) DESC, server_committed_at DESC')
        .listen((rows) {
      final items = rows.map(sessionFromRow).toList(growable: false);
      _deriver.onEmission(items);
      _lastSessions = items;
      _sessionsController?.add(items);
    });

    _productsSub = products.watch().listen((items) {
      _lastProducts = items;
      _productsController?.add(items);
    });

    _cartSub = cartItems.watch(orderBy: [Order.desc('added_at')]).listen((items) {
      _lastCart = items;
      _cartController?.add(items);
    });

    _ordersSub = orders.watch(orderBy: [Order.desc('created_at')]).listen((items) {
      _lastOrders = items;
      _ordersController?.add(items);
    });

    _ready = true;
  }

  @override
  Future<String> addSession(SessionRow s) async {
    assert(s.serverCommittedAt == null, 'serverCommittedAt must be null');
    _deriver.localIds.add(s.id); // before write(): see class doc on ordering
    await _requireDb().write(
      table: 'sessions',
      op: 'upsert',
      pk: s.id,
      payload: sessionWritePayload(s),
    );
    return s.id;
  }

  @override
  Future<void> updateSession(SessionRow s) => _requireDb().write(
        table: 'sessions',
        op: 'upsert',
        pk: s.id,
        payload: sessionWritePayload(s),
      );

  @override
  Future<void> deleteSession(String id) =>
      _requireDb().write(table: 'sessions', op: 'delete', pk: id);

  @override
  Future<void> addToCart(CartItemRow item) => _requireDb().write(
        table: 'cart_items',
        op: 'upsert',
        pk: item.id,
        payload: cartItemWritePayload(item, userId: _userId),
      );

  @override
  Future<void> removeCartItem(String id) =>
      _requireDb().write(table: 'cart_items', op: 'delete', pk: id);

  @override
  Future<void> clearCart() async {
    final items = _lastCart ?? const <CartItemRow>[];
    for (final item in items) {
      await removeCartItem(item.id);
    }
  }

  @override
  Future<String> placeOrder(OrderRow o) async {
    await _requireDb().write(
      table: 'orders',
      op: 'upsert',
      pk: o.id,
      payload: orderWritePayload(o, userId: _userId),
    );
    await clearCart();
    return o.id;
  }

  @override
  Stream<List<CartItemRow>> watchCart() => replayLatest(
      _requireController(_cartController, 'watchCart() before init()'),
      () => _lastCart);

  @override
  Stream<List<OrderRow>> watchOrders() => replayLatest(
      _requireController(_ordersController, 'watchOrders() before init()'),
      () => _lastOrders);

  @override
  Stream<List<SessionRow>> watchSessions() => replayLatest(
      _requireController(_sessionsController, 'watchSessions() before init()'),
      () => _lastSessions);

  @override
  Stream<List<ProductRow>> watchProducts() => replayLatest(
      _requireController(_productsController, 'watchProducts() before init()'),
      () => _lastProducts);

  @override
  Stream<bool> get connected => replayLatest(
      _requireController(_connectedController, 'connected before init()'),
      () => _lastConnected);

  @override
  Future<void> setConnected(bool up) async {
    // Startup race guard: the connectivity guard fires its initial platform
    // event while init() may still be mid-flight (db connected but
    // subscribeTables()/watch() wiring incomplete). resume()/disconnect()
    // during that window trips the engine's "watch() called before
    // subscribe()" invariant — drop the event instead; init() always
    // finishes in the connected state anyway.
    final db = _db;
    if (db == null || !_ready) return;
    if (up) {
      // ponytail: NostosDatabase.resumeSync() is fire-and-forget (no Future) —
      // setConnected(true) does not itself await a reconnect. Record this in
      // RunRecord if the bench needs a reconnect-observed timestamp instead.
      db.resumeSync();
    } else {
      await db.pauseSync();
      // Reflect offline in the UI immediately: disconnect() tears the socket
      // down client-side, but the engine's connectionState stream only emits
      // on *observed* transport transitions, which can lag a forced local
      // disconnect. The wire listener re-emits truth on reconnect.
      _lastConnected = false;
      _connectedController?.add(false);
    }
  }

  @override
  Stream<SyncMark> get marks => _deriver.marks;

  @override
  Future<void> signOut() async {
    _ready = false;
    await _sessionsSub?.cancel();
    await _productsSub?.cancel();
    await _cartSub?.cancel();
    await _ordersSub?.cancel();
    await _connSub?.cancel();
    _sessionsSub = null;
    _productsSub = null;
    _cartSub = null;
    _ordersSub = null;
    _connSub = null;

    await _db?.signOut(); // ADR-0029: full local wipe + client teardown
    _db = null;
    _accessToken = null;

    await _sessionsController?.close();
    await _productsController?.close();
    await _cartController?.close();
    await _ordersController?.close();
    await _connectedController?.close();
    _sessionsController = null;
    _productsController = null;
    _cartController = null;
    _ordersController = null;
    _connectedController = null;
    _lastSessions = null;
    _lastProducts = null;
    _lastCart = null;
    _lastOrders = null;
    _lastConnected = null;

    _deriver.reset();
    // spec/adapter.md item 4: signOut leaves no live engine session — the
    // caller re-runs init() to cold-sync from zero. _deriver survives (see
    // field comment) so marks resume once init() rebuilds the controllers.
  }

  NostosDatabase _requireDb() =>
      _db ?? (throw StateError('NostosAdapter.init() must be called first'));

  /// PILOT (ADR-0037): register this device's push token against the live
  /// engine's REST surface (`POST /push-tokens`, same JWT as `/sync`).
  /// Passthrough so callers never hold the SDK directly; the SDK's sign-out
  /// hook deregisters session-registered tokens automatically.
  Future<void> registerPushToken(String platform, String token) =>
      _requireDb().registerPushToken(platform, token);

  /// PILOT (ADR-0037): the access token the live session was opened with —
  /// what the push pilot persists for its background-isolate wake.
  String? get currentAccessToken => _accessToken;

  Stream<T> _requireController<T>(StreamController<T>? c, String what) =>
      (c ?? (throw StateError('NostosAdapter: $what'))).stream;
}

/// Subscribes [onConnected] to [connectionState] and returns the
/// subscription. Pulled out to a top-level function purely so the
/// listen-before-subscribe ordering is unit-testable without a live
/// NostosDatabase — see nostos_adapter_test.dart's "surfaces a transition
/// fired synchronously by the caller" regression test, which reproduces the
/// bug this guards against: a broadcast stream that starts emitting only
/// once some `subscribe()` call runs, and does not replay to a listener that
/// attaches afterward.
StreamSubscription<NostosConnectionState> wireConnectionState(
  Stream<NostosConnectionState> connectionState,
  void Function(bool isConnected) onConnected,
) =>
    connectionState.listen((state) {
      onConnected(state == NostosConnectionState.connected);
    });

final NostosSchema _schema = NostosSchema(tables: [
  NostosTable(name: 'sessions', primaryKey: const ['id'], columns: [
    NostosColumn.text('id'),
    NostosColumn.text('title'),
    NostosColumn.text('type'),
    NostosColumn.integer('metric'),
    NostosColumn.text('unit'),
    NostosColumn.text('note'),
    NostosColumn.integer('streak'),
    NostosColumn.text('occurred_on'),
    NostosColumn.text('server_committed_at'),
  ]),
  NostosTable(name: 'products', primaryKey: const ['id'], columns: [
    NostosColumn.text('id'),
    NostosColumn.text('name'),
    NostosColumn.text('category'),
    NostosColumn.integer('price_cents'),
    NostosColumn.real('rating'),
    NostosColumn.integer('plant_based'),
    NostosColumn.text('image_url'),
  ]),
  NostosTable(name: 'cart_items', primaryKey: const ['id'], columns: [
    NostosColumn.text('id'),
    NostosColumn.text('product_id'),
    NostosColumn.integer('qty'),
    NostosColumn.text('added_at'),
  ]),
  NostosTable(name: 'orders', primaryKey: const ['id'], columns: [
    NostosColumn.text('id'),
    NostosColumn.text('status'),
    NostosColumn.integer('subtotal_cents'),
    NostosColumn.integer('tax_cents'),
    NostosColumn.integer('shipping_cents'),
    NostosColumn.integer('total_cents'),
    NostosColumn.text('payment_ref'),
    NostosColumn.text('items_json'),
    NostosColumn.text('created_at'),
  ]),
]);

/// Maps a decoded `cart_items` row to [CartItemRow]. Top-level and pure —
/// same testability rationale as [sessionFromRow].
CartItemRow cartItemFromRow(Map<String, dynamic> row) => CartItemRow(
      id: row['id'] as String,
      productId: row['product_id'] as String,
      qty: _asInt(row['qty']),
      addedAt: DateTime.parse(row['added_at'] as String),
    );

/// Maps a decoded `orders` row to [OrderRow].
OrderRow orderFromRow(Map<String, dynamic> row) => OrderRow(
      id: row['id'] as String,
      status: row['status'] as String,
      subtotalCents: _asInt(row['subtotal_cents']),
      taxCents: _asInt(row['tax_cents']),
      shippingCents: _asInt(row['shipping_cents']),
      totalCents: _asInt(row['total_cents']),
      paymentRef: row['payment_ref'] as String?,
      itemsJson: row['items_json'] as String?,
      createdAt: DateTime.parse(row['created_at'] as String),
    );

/// Write payload for a cart upsert (snake_case wire keys, like
/// [sessionWritePayload]).
///
/// Includes `user_id` explicitly: cart_items is `user_id NOT NULL DEFAULT
/// auth.uid()`, and nostos-server's PgWriteBack runs on a direct Postgres
/// connection where `auth.uid()` is NULL — omitting it fails NOT NULL and
/// the write is rejected (tenant stamping is off; products is global).
Map<String, dynamic> cartItemWritePayload(CartItemRow c, {String? userId}) => {
      'id': c.id,
      'user_id': ?userId,
      'product_id': c.productId,
      'qty': c.qty,
      'added_at': c.addedAt.toUtc().toIso8601String(),
    };

/// Write payload for an order insert. Includes `user_id` for the same
/// reason as [cartItemWritePayload].
Map<String, dynamic> orderWritePayload(OrderRow o, {String? userId}) => {
      'id': o.id,
      'user_id': ?userId,
      'status': o.status,
      'subtotal_cents': o.subtotalCents,
      'tax_cents': o.taxCents,
      'shipping_cents': o.shippingCents,
      'total_cents': o.totalCents,
      'payment_ref': o.paymentRef,
      'items_json': o.itemsJson,
      'created_at': o.createdAt.toUtc().toIso8601String(),
    };

/// Maps a decoded row from a sessions read (the escape-hatch `watchSql` path,
/// since the structured `Order` can't express the `(server_committed_at IS
/// NULL) DESC` sort) to [SessionRow]. Top-level and pure so it's testable
/// without the FFI bridge — see nostos_adapter_test.dart.
SessionRow sessionFromRow(Map<String, dynamic> row) => SessionRow(
      id: row['id'] as String,
      title: row['title'] as String,
      type: row['type'] as String,
      metric: _asInt(row['metric']),
      unit: row['unit'] as String,
      note: row['note'] as String?,
      streak: row['streak'] == null ? 0 : _asInt(row['streak']),
      occurredOn: DateTime.parse(row['occurred_on'] as String),
      serverCommittedAt: _asDateTimeOrNull(row['server_committed_at']),
    );

ProductRow productFromRow(Map<String, dynamic> row) => ProductRow(
      id: row['id'] as String,
      name: row['name'] as String,
      category: row['category'] as String,
      priceCents: _asInt(row['price_cents']),
      rating: _asDoubleOrNull(row['rating']),
      plantBased: _asBool(row['plant_based']),
      imageUrl: row['image_url'] as String?,
    );

/// Write image for `addSession`. Omits `server_committed_at` — Postgres's
/// `default now()` is the clock authority for the serverAcked mark; sending
/// an explicit null would overwrite that default and the mark would never
/// fire. Omits `user_id` — nostos-server stamps the tenant column from the
/// JWT server-side (NOSTOS_TENANT_COLUMN=user_id, write_back.rs's
/// stamp_tenant_column), overwriting whatever the client sends.
Map<String, dynamic> sessionWritePayload(SessionRow s) => {
      'id': s.id,
      'title': s.title,
      'type': s.type,
      'metric': s.metric,
      'unit': s.unit,
      if (s.note != null) 'note': s.note,
      'streak': s.streak,
      'occurred_on': _dateOnly(s.occurredOn),
    };

String _dateOnly(DateTime d) =>
    '${d.year.toString().padLeft(4, '0')}-${d.month.toString().padLeft(2, '0')}-${d.day.toString().padLeft(2, '0')}';

int _asInt(Object? v) => switch (v) {
      int i => i,
      num n => n.toInt(),
      String s => int.parse(s),
      _ => throw ArgumentError('expected int, got $v (${v.runtimeType})'),
    };

double? _asDoubleOrNull(Object? v) => switch (v) {
      null => null,
      double d => d,
      num n => n.toDouble(),
      String s => double.parse(s),
      _ => throw ArgumentError('expected double?, got $v (${v.runtimeType})'),
    };

bool _asBool(Object? v) => switch (v) {
      bool b => b,
      int i => i != 0,
      num n => n != 0,
      String s => s == 'true' || s == '1',
      _ => throw ArgumentError('expected bool, got $v (${v.runtimeType})'),
    };

DateTime? _asDateTimeOrNull(Object? v) => switch (v) {
      null => null,
      String s => DateTime.parse(s),
      _ => throw ArgumentError('expected DateTime?, got $v (${v.runtimeType})'),
    };
