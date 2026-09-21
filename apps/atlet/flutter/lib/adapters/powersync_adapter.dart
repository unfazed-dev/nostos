import 'dart:async';
import 'dart:io';

import 'package:powersync/powersync.dart';
import 'package:supabase_flutter/supabase_flutter.dart';

import '../bench/marks.dart';
import 'sync_adapter.dart';

/// PowerSync engine implementation of [SyncAdapter] for the Atlet pilot.
///
/// Wraps [PowerSyncDatabase]. Row mapping and the write payload are pure
/// top-level functions below so they're unit-testable without a live sync
/// connection — see powersync_adapter_test.dart. Mirrors nostos_adapter.dart's
/// structure (same field/method shape) so the two adapters read as
/// apples-to-apples.
class PowerSyncAdapter implements SyncAdapter {
  @override
  final String engine = 'powersync';

  // Local PowerSync service (docker-compose.atlet.yml maps it to :8081).
  static const String _powerSyncUrl = String.fromEnvironment(
    'POWERSYNC_URL',
    defaultValue: 'http://localhost:8081',
  );

  // Created once, never recreated: see nostos_adapter.dart's field doc — the
  // conformance test's `marks` listener must keep seeing emissions across a
  // signOut()/init() cycle, so _deriver must outlive individual sync sessions.
  final MarkDeriver _deriver = MarkDeriver(Stopwatch()..start());

  PowerSyncDatabase? _db;
  String? _dbPath;
  String? _accessToken;
  StreamSubscription<List<SessionRow>>? _sessionsSub;
  StreamSubscription<List<ProductRow>>? _productsSub;
  StreamSubscription<bool>? _statusSub;
  StreamController<List<SessionRow>>? _sessionsController;
  StreamController<List<ProductRow>>? _productsController;
  StreamController<bool>? _connectedController;

  // Latest values, replayed to late subscribers via replayLatest — same
  // broadcast-no-replay bug as NostosAdapter (see sync_adapter.dart doc):
  // a screen built after an engine switch subscribes after the read-only
  // `products` table's single snapshot emission and spins forever.
  List<SessionRow>? _lastSessions;
  List<ProductRow>? _lastProducts;
  List<CartItemRow>? _lastCart;
  List<OrderRow>? _lastOrders;
  bool? _lastConnected;
  StreamController<List<CartItemRow>>? _cartController;
  StreamController<List<OrderRow>>? _ordersController;
  StreamSubscription<List<CartItemRow>>? _cartSub;
  StreamSubscription<List<OrderRow>>? _ordersSub;

  @override
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) async {
    _sessionsController = StreamController<List<SessionRow>>.broadcast();
    _productsController = StreamController<List<ProductRow>>.broadcast();
    _cartController = StreamController<List<CartItemRow>>.broadcast();
    _ordersController = StreamController<List<OrderRow>>.broadcast();
    _connectedController = StreamController<bool>.broadcast();
    _accessToken = accessToken;

    _dbPath = '$dbDir/powersync.db';
    final db = PowerSyncDatabase(schema: _schema, path: _dbPath!);
    _db = db;
    await db.initialize();

    // Attach BEFORE connect(): db.statusStream is a broadcast stream and the
    // sync client flips `connected` internally as soon as connect() starts
    // its work — same non-replay hazard NostosAdapter guards against (see
    // nostos_adapter.dart's wireConnectionState doc). A listener attached
    // after the awaited connect() call below can miss that first transition.
    _statusSub = wireConnected(
      db.statusStream.map((status) => status.connected),
      (isConnected) {
        _lastConnected = isConnected;
        _connectedController?.add(isConnected);
      },
    );

    await db.connect(connector: _SupabaseConnector(accessToken));

    _sessionsSub = db
        .watch(
          'SELECT * FROM sessions '
          'ORDER BY occurred_on DESC, '
          '(server_committed_at IS NULL) DESC, server_committed_at DESC',
        )
        .map((rows) => rows.map(sessionFromRow).toList(growable: false))
        .listen((sessions) {
          _deriver.onEmission(sessions);
          _lastSessions = sessions;
          _sessionsController?.add(sessions);
        });

    _productsSub = db
        .watch('SELECT * FROM products')
        .map((rows) => rows.map(productFromRow).toList(growable: false))
        .listen((products) {
          _lastProducts = products;
          _productsController?.add(products);
        });

    _cartSub = db
        .watch('SELECT * FROM cart_items ORDER BY added_at DESC')
        .map((rows) => rows.map(psCartItemFromRow).toList(growable: false))
        .listen((items) {
          _lastCart = items;
          _cartController?.add(items);
        });

    _ordersSub = db
        .watch('SELECT * FROM orders ORDER BY created_at DESC')
        .map((rows) => rows.map(psOrderFromRow).toList(growable: false))
        .listen((orders) {
          _lastOrders = orders;
          _ordersController?.add(orders);
        });
  }

  @override
  Future<String> addSession(SessionRow s) async {
    assert(s.serverCommittedAt == null, 'serverCommittedAt must be null');
    _deriver.localIds.add(s.id); // before write(): see class doc on ordering
    final payload = sessionWritePayload(s);
    final columns = payload.keys.toList(growable: false);
    await _requireDb().execute(
      'INSERT INTO sessions (${columns.join(', ')}) '
      'VALUES (${List.filled(columns.length, '?').join(', ')})',
      payload.values.toList(growable: false),
    );
    return s.id;
  }

  @override
  Future<void> updateSession(SessionRow s) async {
    final payload = sessionWritePayload(s);
    final sets = payload.keys
        .where((k) => k != 'id')
        .map((k) => '$k = ?')
        .join(', ');
    final args = [
      for (final k in payload.keys.where((k) => k != 'id')) payload[k],
      s.id,
    ];
    await _requireDb().execute('UPDATE sessions SET $sets WHERE id = ?', args);
  }

  @override
  Future<void> deleteSession(String id) =>
      _requireDb().execute('DELETE FROM sessions WHERE id = ?', [id]);

  @override
  Future<void> addToCart(CartItemRow item) => _requireDb().execute(
    'INSERT INTO cart_items (id, product_id, qty, added_at) '
    'VALUES (?, ?, ?, ?) '
    'ON CONFLICT(id) DO UPDATE SET qty = excluded.qty',
    [item.id, item.productId, item.qty, item.addedAt.toUtc().toIso8601String()],
  );

  @override
  Future<void> removeCartItem(String id) =>
      _requireDb().execute('DELETE FROM cart_items WHERE id = ?', [id]);

  @override
  Future<void> clearCart() async {
    final items = _lastCart ?? const <CartItemRow>[];
    for (final item in items) {
      await removeCartItem(item.id);
    }
  }

  @override
  Future<String> placeOrder(OrderRow o) async {
    await _requireDb().execute(
      'INSERT INTO orders (id, status, subtotal_cents, tax_cents, '
      'shipping_cents, total_cents, payment_ref, items_json, created_at) '
      'VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)',
      [
        o.id,
        o.status,
        o.subtotalCents,
        o.taxCents,
        o.shippingCents,
        o.totalCents,
        o.paymentRef,
        o.itemsJson,
        o.createdAt.toUtc().toIso8601String(),
      ],
    );
    await clearCart();
    return o.id;
  }

  @override
  Stream<List<CartItemRow>> watchCart() => replayLatest(
    _requireController(_cartController, 'watchCart() before init()'),
    () => _lastCart,
  );

  @override
  Stream<List<OrderRow>> watchOrders() => replayLatest(
    _requireController(_ordersController, 'watchOrders() before init()'),
    () => _lastOrders,
  );

  @override
  Stream<List<SessionRow>> watchSessions() => replayLatest(
    _requireController(_sessionsController, 'watchSessions() before init()'),
    () => _lastSessions,
  );

  @override
  Stream<List<ProductRow>> watchProducts() => replayLatest(
    _requireController(_productsController, 'watchProducts() before init()'),
    () => _lastProducts,
  );

  @override
  Stream<bool> get connected => replayLatest(
    _requireController(_connectedController, 'connected before init()'),
    () => _lastConnected,
  );

  @override
  Future<void> setConnected(bool up) async {
    final db = _requireDb();
    if (up) {
      // ponytail: unlike NostosDatabase.resume() (fire-and-forget), PowerSync
      // has no bare "resume" — disconnect() fully tears down the sync client,
      // so resuming means calling connect() again with a connector. Token is
      // the one captured at init()/last setConnected(true) (signOut() clears
      // it, so a stale adapter can't reconnect after wipe).
      await db.connect(connector: _SupabaseConnector(_requireAccessToken()));
    } else {
      await db.disconnect();
    }
  }

  @override
  Stream<SyncMark> get marks => _deriver.marks;

  @override
  Future<void> signOut() async {
    await _sessionsSub?.cancel();
    await _productsSub?.cancel();
    await _cartSub?.cancel();
    await _ordersSub?.cancel();
    await _statusSub?.cancel();
    _sessionsSub = null;
    _productsSub = null;
    _cartSub = null;
    _ordersSub = null;
    _statusSub = null;

    final db = _db;
    if (db != null) {
      await db.disconnectAndClear();
      await db.close();
    }
    _db = null;
    _accessToken = null;

    // spec/adapter.md item 4: signOut deletes local DB files (full wipe).
    // disconnectAndClear() only empties the synced tables — the sqlite file
    // stays on disk (per powersync_db_mixin.dart's doc: "the database can
    // still be queried after this is called, but the tables would be
    // empty") — so delete it and its WAL/SHM/journal sidecars explicitly to
    // match NostosAdapter's db.signOut() full-wipe contract (ADR-0029).
    final path = _dbPath;
    if (path != null) {
      for (final suffix in const ['', '-wal', '-shm', '-journal']) {
        final f = File('$path$suffix');
        if (f.existsSync()) await f.delete();
      }
    }
    _dbPath = null;

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
    _lastCart = null;
    _lastOrders = null;
    _lastSessions = null;
    _lastProducts = null;
    _lastConnected = null;

    _deriver.reset();
    // spec/adapter.md item 4: signOut leaves no live engine session — the
    // caller re-runs init() to cold-sync from zero. _deriver survives (see
    // field comment) so marks resume once init() rebuilds the controllers.
  }

  PowerSyncDatabase _requireDb() =>
      _db ?? (throw StateError('PowerSyncAdapter.init() must be called first'));

  String _requireAccessToken() =>
      _accessToken ??
      (throw StateError('PowerSyncAdapter.setConnected(true) before init()'));

  Stream<T> _requireController<T>(StreamController<T>? c, String what) =>
      (c ?? (throw StateError('PowerSyncAdapter: $what'))).stream;
}

/// Uploads the local CRUD queue to Supabase via PostgREST. Errors are left
/// to propagate (not caught): per [PowerSyncDatabase.connect]'s doc, "the
/// connection is automatically re-opened if it fails for any reason", and
/// leaving the transaction un-completed here is what makes it retry rather
/// than silently dropping a write.
class _SupabaseConnector extends PowerSyncBackendConnector {
  _SupabaseConnector(this._accessToken);

  final String _accessToken;

  @override
  Future<PowerSyncCredentials?> fetchCredentials() async =>
      PowerSyncCredentials(
        endpoint: PowerSyncAdapter._powerSyncUrl,
        token: _accessToken,
      );

  @override
  Future<void> uploadData(PowerSyncDatabase database) async {
    final tx = await database.getNextCrudTransaction();
    if (tx == null) return;

    final supabase = Supabase.instance.client;
    for (final op in tx.crud) {
      final table = supabase.from(op.table);
      switch (op.op) {
        case UpdateType.put:
          await table.upsert({'id': op.id, ...?op.opData});
        case UpdateType.patch:
          await table.update(op.opData ?? const {}).eq('id', op.id);
        case UpdateType.delete:
          await table.delete().eq('id', op.id);
      }
    }
    await tx.complete();
  }
}

/// Subscribes [onConnected] to a `connected` bool stream and returns the
/// subscription. Extracted to a top-level function (mirrors
/// nostos_adapter.dart's wireConnectionState) purely so the
/// listen-before-connect ordering is unit-testable without a live
/// PowerSyncDatabase — see powersync_adapter_test.dart.
StreamSubscription<bool> wireConnected(
  Stream<bool> connectedStream,
  void Function(bool isConnected) onConnected,
) => connectedStream.listen(onConnected);

final Schema _schema = Schema([
  Table('sessions', [
    Column.text('title'),
    Column.text('type'),
    Column.integer('metric'),
    Column.text('unit'),
    Column.text('note'),
    Column.integer('streak'),
    Column.text('occurred_on'),
    Column.text('server_committed_at'),
    Column.text('user_id'),
  ]),
  Table('products', [
    Column.text('name'),
    Column.text('category'),
    Column.integer('price_cents'),
    Column.real('rating'),
    Column.integer('plant_based'),
    Column.text('image_url'),
  ]),
  Table('cart_items', [
    Column.text('product_id'),
    Column.integer('qty'),
    Column.text('added_at'),
    Column.text('user_id'),
  ]),
  Table('orders', [
    Column.text('status'),
    Column.integer('subtotal_cents'),
    Column.integer('tax_cents'),
    Column.integer('shipping_cents'),
    Column.integer('total_cents'),
    Column.text('payment_ref'),
    Column.text('items_json'),
    Column.text('created_at'),
    Column.text('user_id'),
  ]),
]);

/// Maps a `cart_items` row to [CartItemRow] — pure, mirrors the nostos
/// adapter's cartItemFromRow.
CartItemRow psCartItemFromRow(Map<String, dynamic> row) => CartItemRow(
  id: row['id'] as String,
  productId: row['product_id'] as String,
  qty: (row['qty'] as num).toInt(),
  addedAt: DateTime.parse(row['added_at'] as String),
);

/// Maps an `orders` row to [OrderRow].
OrderRow psOrderFromRow(Map<String, dynamic> row) => OrderRow(
  id: row['id'] as String,
  status: row['status'] as String,
  subtotalCents: (row['subtotal_cents'] as num).toInt(),
  taxCents: (row['tax_cents'] as num).toInt(),
  shippingCents: (row['shipping_cents'] as num).toInt(),
  totalCents: (row['total_cents'] as num).toInt(),
  paymentRef: row['payment_ref'] as String?,
  itemsJson: row['items_json'] as String?,
  createdAt: DateTime.parse(row['created_at'] as String),
);

/// Maps a row from `db.watch('SELECT * FROM sessions')` to [SessionRow].
/// Top-level and pure so it's testable without a live sync connection — see
/// powersync_adapter_test.dart. [row] accepts `Map<String, dynamic>` (a
/// PowerSync `Row` implements that interface) so plain map literals work in
/// tests too.
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

/// Local write image for `addSession`. Omits `server_committed_at` — the
/// server's `default now()` is the clock authority for the serverAcked mark
/// (an explicit null would overwrite that default once uploaded). Omits
/// `user_id` — Postgres's `default auth.uid()` stamps the tenant column
/// server-side from the Supabase upsert's auth context, overwriting whatever
/// the client sends (mirrors nostos_adapter.dart's sessionWritePayload).
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
