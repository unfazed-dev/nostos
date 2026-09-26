import 'dart:async';
import 'dart:io';
import 'dart:typed_data';

import 'package:nostos_flutter/nostos_flutter.dart';

import '../bench/marks.dart';
import 'sync_adapter.dart';

const _appwriteTestDbSuffix = String.fromEnvironment('ATLET_TEST_DB_SUFFIX');

/// Nostos engine implementation of [SyncAdapter] for the Atlet pilot.
///
/// Wraps [NostosDatabase] (sdk/nostos_flutter/lib/src/nostos_database.dart).
/// Row mapping and the write payload are pure top-level functions below so
/// they're unit-testable without the native Rust bridge — see
/// nostos_adapter_test.dart.
class NostosAdapter implements SyncAdapter {
  /// Server mode: sync through a `nostos-server` `/sync` socket.
  NostosAdapter() : engine = 'nostos', _appwrite = false, _open = _openServer;

  /// Direct mode: sync straight with Supabase, no `nostos-server` anywhere
  /// (`docs/plans/direct-mode-sync-protocol.md`). Everything past `init()` is
  /// the same code — the mode only
  /// decides who is on the other end of the pull, so the adapter takes an
  /// opener instead of having a second copy of itself.
  ///
  /// [anonKey] is the project's publishable key, the only credential the app
  /// ships; RLS on the deployed schema is what actually gates the rows.
  NostosAdapter.direct({required String anonKey})
    : engine = 'nostos-direct',
      _appwrite = false,
      _open =
          (({
            required String supabaseUrl,
            required String accessToken,
            required String userId,
            required String dbDir,
          }) => openNostosDirect(
            supabaseUrl: supabaseUrl,
            anonKey: anonKey,
            accessToken: accessToken,
            userId: userId,
            dbDir: dbDir,
          ));

  /// Direct mode through the Appwrite Cloud sync Function (ADR-0050).
  NostosAdapter.appwrite({required String projectId})
    : engine = 'nostos-appwrite',
      _appwrite = true,
      _open =
          (({
            required String supabaseUrl,
            required String accessToken,
            required String userId,
            required String dbDir,
          }) => NostosDatabase.appwrite(
            endpoint: supabaseUrl,
            projectId: projectId,
            userId: userId,
            jwt: accessToken,
            schema: _schema,
            sqlitePath: _appwriteTestDbSuffix.isEmpty
                ? '$dbDir/nostos_appwrite.sqlite'
                : '$dbDir/nostos_appwrite_$_appwriteTestDbSuffix.sqlite',
          ));

  @override
  final String engine;
  final bool _appwrite;

  /// How this adapter opens its database — the one thing server and direct
  /// mode do differently.
  final Future<NostosDatabase> Function({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  })
  _open;

  /// nostos-server `/sync` endpoint for the Atlet local profile
  /// (docker-compose.atlet.yml binds nostos-server on 0.0.0.0:8080; `/sync`
  /// is NOSTOS_WS_PATH's default in crates/nostos-server/src/config.rs).
  static const String _nostosUrl = String.fromEnvironment(
    'NOSTOS_SYNC_URL',
    defaultValue: 'ws://localhost:8080/sync',
  );

  static Future<NostosDatabase> _openServer({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) => NostosDatabase.connect(
    url: _nostosUrl,
    token: accessToken,
    schema: _schema,
    sqlitePath: '$dbDir/nostos.sqlite',
  );

  // Created once, never recreated: the conformance test's `marks` listener
  // is attached before signOut() and must keep seeing emissions after a
  // second init(), so _deriver must outlive individual sync sessions.
  final MarkDeriver _deriver = MarkDeriver(Stopwatch()..start());

  NostosDatabase? _db;

  /// T6 attachments driver over the `product-images` bucket (migration 0012).
  /// Never started: catalog images are read-through via [Attachments.bytes],
  /// no queued transfers to pump. ponytail: `_imageCache` is the six seed
  /// images in memory so a 1k-cell grid never re-reads the blob store.
  Attachments? _attachments;
  final Map<String, Uint8List> _imageCache = <String, Uint8List>{};
  StreamSubscription<List<Map<String, dynamic>>>? _sessionsSub;
  StreamSubscription<List<ProductRow>>? _productsSub;
  StreamSubscription<List<UserProfileRow>>? _userProfilesSub;
  StreamSubscription<NostosConnectionState>? _connSub;
  StreamController<List<SessionRow>>? _sessionsController;
  StreamController<List<ProductRow>>? _productsController;
  StreamController<List<UserProfileRow>>? _userProfilesController;
  StreamController<bool>? _connectedController;
  StreamController<bool>? _accessRevokedController;
  bool _accessWasRevoked = false;

  // Latest values, replayed to late subscribers via replayLatest (see
  // sync_adapter.dart for the full failure mode: broadcast controllers do
  // not replay, and `products` emits exactly one snapshot, so a ShopScreen
  // built after an engine switch spun forever). The SDK's watch() replays
  // hot values; this layer must not discard that property.
  List<SessionRow>? _lastSessions;
  List<ProductRow>? _lastProducts;
  List<UserProfileRow>? _lastUserProfiles;
  List<CartItemRow>? _lastCart;
  List<OrderRow>? _lastOrders;
  List<OrderEventRow>? _lastOrderEvents;
  bool? _lastConnected;
  StreamController<List<CartItemRow>>? _cartController;
  StreamController<List<OrderRow>>? _ordersController;
  StreamController<List<OrderEventRow>>? _orderEventsController;
  StreamSubscription<dynamic>? _cartSub;
  StreamSubscription<dynamic>? _ordersSub;
  StreamSubscription<dynamic>? _orderEventsSub;

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
  bool? _pendingConnectivity;

  /// True after the database, subscriptions, and local watches are installed.
  bool get isReady => _ready;

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
    if (_appwrite) {
      _userProfilesController =
          StreamController<List<UserProfileRow>>.broadcast();
    }
    _cartController = StreamController<List<CartItemRow>>.broadcast();
    _ordersController = StreamController<List<OrderRow>>.broadcast();
    _orderEventsController = StreamController<List<OrderEventRow>>.broadcast();
    _connectedController = StreamController<bool>.broadcast();
    _accessWasRevoked = false;
    if (_appwrite) {
      _accessRevokedController = StreamController<bool>.broadcast();
    }

    final db = await _open(
      supabaseUrl: supabaseUrl,
      accessToken: accessToken,
      userId: userId,
      dbDir: dbDir,
    );
    _db = db;

    // Attach BEFORE subscribeTables(): Nostos.connectionState is a
    // non-replaying broadcast stream that is empty until the engine's
    // subscribe has run at least once — a listener attached after
    // subscribeTables() can miss the very first `connected` transition that
    // fires synchronously inside it, so `connected` would never emit true
    // until the next disconnect/resume cycle. See wireConnectionState below.
    _connSub = wireConnectionState(
      db.connectionState,
      (isConnected) {
        _lastConnected = isConnected;
        _connectedController?.add(isConnected);
      },
      onAccessRevoked: () {
        _accessWasRevoked = true;
        _accessRevokedController?.add(true);
      },
    );

    await db.subscribeTables([
      const NostosTableSub(name: 'sessions'),
      const NostosTableSub(name: 'products'),
      const NostosTableSub(name: 'cart_items'),
      const NostosTableSub(name: 'orders'),
      const NostosTableSub(name: 'order_events'),
      const NostosTableSub(name: 'attachments'),
      if (_appwrite) const NostosTableSub(name: 'user_profiles'),
    ]);

    if (!_appwrite) {
      _attachments = db.attachments(
        adapter: SupabaseStorageAdapter(bucket: 'product-images'),
        blobStore: LocalFileBlobStore(Directory('$dbDir/blobs')),
      );
    }

    // Typed collection handles (ADR-0032 T2): the taught surface for "table,
    // maybe filter, maybe order" reads. Injection-safe by construction.
    // (No sessions handle — its sort needs watchSql, see below.)
    final products = db.collection<ProductRow>(
      table: 'products',
      fromRow: productFromRow,
    );
    final cartItems = db.collection<CartItemRow>(
      table: 'cart_items',
      fromRow: cartItemFromRow,
    );
    final orders = db.collection<OrderRow>(
      table: 'orders',
      fromRow: orderFromRow,
    );
    final orderEvents = db.collection<OrderEventRow>(
      table: 'order_events',
      fromRow: orderEventFromRow,
    );

    // sessions: the sort needs `(server_committed_at IS NULL) DESC`, an
    // expression the structured `Order` (field+direction) can't express yet —
    // contract gap. Routed through the raw-SQL escape hatch [watchSql]; the
    // other three reads use typed collections. See ADR-0032 "Escape hatch".
    // Newest first: latest day on top; within a day, the just-added row
    // (server_committed_at still NULL until acked) sorts above older ones.
    _sessionsSub = db
        .watchSql(
          'SELECT * FROM sessions '
          'ORDER BY occurred_on DESC, '
          '(server_committed_at IS NULL) DESC, server_committed_at DESC',
        )
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

    if (_appwrite) {
      _userProfilesSub = db
          .collection<UserProfileRow>(
            table: 'user_profiles',
            fromRow: userProfileFromRow,
          )
          .watch(orderBy: [Order.asc('display_name')])
          .listen((items) {
            _lastUserProfiles = items;
            _userProfilesController?.add(items);
          });
    }

    _cartSub = cartItems.watch(orderBy: [Order.desc('added_at')]).listen((
      items,
    ) {
      _lastCart = items;
      _cartController?.add(items);
    });

    _ordersSub = orders.watch(orderBy: [Order.desc('created_at')]).listen((
      items,
    ) {
      _lastOrders = items;
      _ordersController?.add(items);
    });

    _orderEventsSub = orderEvents
        .watch(orderBy: [Order.desc('created_at')])
        .listen((items) {
          _lastOrderEvents = items;
          _orderEventsController?.add(items);
        });

    _ready = true;
    final pendingConnectivity = _pendingConnectivity;
    _pendingConnectivity = null;
    if (pendingConnectivity == false) {
      await setConnected(false);
    }
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

  /// Admin catalog write; cloud authorization is enforced by the provider.
  Future<void> saveProduct(ProductRow product) => _requireDb().write(
    table: 'products',
    op: 'upsert',
    pk: product.id,
    payload: {
      'id': product.id,
      'name': product.name,
      'category': product.category,
      'price_cents': product.priceCents,
      'rating': ?product.rating,
      'plant_based': product.plantBased,
      'image_url': ?product.imageUrl,
      'image_id': ?product.imageId,
    },
  );

  Future<void> deleteProduct(String id) =>
      _requireDb().write(table: 'products', op: 'delete', pk: id);

  Stream<List<UserProfileRow>> watchUserProfiles() => replayLatest(
    _requireController(
      _userProfilesController,
      'watchUserProfiles() requires Appwrite',
    ),
    () => _lastUserProfiles,
  );

  Future<void> saveUserProfile(UserProfileRow profile) => _requireDb().write(
    table: 'user_profiles',
    op: 'upsert',
    pk: profile.id,
    payload: {
      'id': profile.id,
      'user_id': profile.id,
      'display_name': profile.displayName,
      'active': profile.active,
    },
  );

  /// Admin fulfils a customer's order without changing its owner or totals.
  Future<void> setOrderStatus(OrderRow order, String status) =>
      _requireDb().write(
        table: 'orders',
        op: 'upsert',
        pk: order.id,
        payload: orderWritePayload(
          OrderRow(
            id: order.id,
            status: status,
            userId: order.userId,
            subtotalCents: order.subtotalCents,
            taxCents: order.taxCents,
            shippingCents: order.shippingCents,
            totalCents: order.totalCents,
            paymentRef: order.paymentRef,
            itemsJson: order.itemsJson,
            createdAt: order.createdAt,
          ),
        ),
      );

  @override
  Stream<List<CartItemRow>> watchCart() => replayLatest(
    _requireController(_cartController, 'watchCart() before init()'),
    () => _lastCart,
  );

  @override
  Stream<List<OrderEventRow>> watchOrderEvents() => replayLatest(
    _requireController(
      _orderEventsController,
      'watchOrderEvents() before init()',
    ),
    () => _lastOrderEvents,
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
  Future<Uint8List?> productImage(String imageId) async {
    final hit = _imageCache[imageId];
    if (hit != null) return hit;
    final bytes = await _attachments?.bytes(imageId);
    if (bytes != null) _imageCache[imageId] = bytes;
    return bytes;
  }

  @override
  Stream<bool> get connected => replayLatest(
    _requireController(_connectedController, 'connected before init()'),
    () => _lastConnected,
  );

  /// The Appwrite Function rejected this account as inactive; native SQLite
  /// rows and outbox have already been wiped (ADR-0050).
  Stream<bool> get accessRevoked => replayLatest(
    _requireController(
      _accessRevokedController,
      'Appwrite accessRevoked before init()',
    ),
    () => _accessWasRevoked,
  );

  @override
  Future<void> setConnected(bool up) async {
    // Startup race guard: the connectivity guard fires its initial platform
    // event while init() may still be mid-flight (db connected but
    // subscribeTables()/watch() wiring incomplete). resume()/disconnect()
    // during that window trips the engine's "watch() called before
    // subscribe()" invariant — drop the event instead; init() always
    // finishes in the connected state anyway.
    final db = _db;
    if (db == null || !_ready) {
      _pendingConnectivity = up;
      return;
    }
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
    _pendingConnectivity = null;
    await _sessionsSub?.cancel();
    await _productsSub?.cancel();
    await _userProfilesSub?.cancel();
    await _cartSub?.cancel();
    await _ordersSub?.cancel();
    await _orderEventsSub?.cancel();
    await _connSub?.cancel();
    _sessionsSub = null;
    _productsSub = null;
    _userProfilesSub = null;
    _cartSub = null;
    _ordersSub = null;
    _orderEventsSub = null;
    _connSub = null;

    await _db?.signOut(); // ADR-0029: full local wipe + client teardown
    _db = null;
    _attachments = null;
    _imageCache.clear();
    _accessToken = null;

    await _sessionsController?.close();
    await _productsController?.close();
    await _userProfilesController?.close();
    await _cartController?.close();
    await _ordersController?.close();
    await _orderEventsController?.close();
    await _connectedController?.close();
    await _accessRevokedController?.close();
    _sessionsController = null;
    _productsController = null;
    _userProfilesController = null;
    _cartController = null;
    _ordersController = null;
    _orderEventsController = null;
    _connectedController = null;
    _accessRevokedController = null;
    _accessWasRevoked = false;
    _lastSessions = null;
    _lastProducts = null;
    _lastUserProfiles = null;
    _lastCart = null;
    _lastOrders = null;
    _lastOrderEvents = null;
    _lastConnected = null;

    _deriver.reset();
    // spec/adapter.md item 4: signOut leaves no live engine session — the
    // caller re-runs init() to cold-sync from zero. _deriver survives (see
    // field comment) so marks resume once init() rebuilds the controllers.
  }

  NostosDatabase _requireDb() =>
      _db ?? (throw StateError('NostosAdapter.init() must be called first'));

  /// Exact durable outbox count for cloud acceptance checks and diagnostics.
  int get pendingWrites => _requireDb().currentStatus.pendingWrites;

  SyncStatus get syncStatus => _requireDb().currentStatus;

  /// Read the SQLite queue directly when an integration test must distinguish
  /// a slow status stream from a write that was already sent.
  Future<int> durablePendingWrites() async {
    final rows = await _requireDb().getAll(
      'SELECT COUNT(*) AS count FROM nostos_outbox WHERE dlq = 0',
    );
    return _asInt(rows.single['count']);
  }

  /// PILOT (ADR-0037): register this device's push token against the live
  /// engine — `POST /push-tokens` in server mode, the
  /// `nostos_register_push_token` RPC in direct mode, same JWT as
  /// the sync either way. Passthrough so callers never hold the SDK directly;
  /// the SDK's sign-out hook deregisters session-registered tokens
  /// automatically.
  Future<void> registerPushToken(String platform, String token) =>
      _requireDb().registerPushToken(platform, token);

  /// PILOT (ADR-0037): the access token the live session was opened with —
  /// what the push pilot persists for its background-isolate wake.
  String? get currentAccessToken => _accessToken;

  /// The signed-in user — the background wake rebuilds the direct-mode scope
  /// (`sub:<id>`) from it.
  String? get currentUserId => _userId;

  /// Swap the credential the live engine syncs with, without tearing it down.
  ///
  /// `NostosDatabase.direct` takes the token once; `NostosDatabase.supabase`
  /// would have wired the rotation itself, but direct mode is opened by URL
  /// and key, so the caller owns it. A Supabase JWT lives about an hour, and
  /// an engine still holding the dead one does not merely sync slowly: the
  /// doorbell counts an auth failure as fatal and ENDS its loop, so the device
  /// goes quiet for the rest of the session while the UI still reads "Online"
  /// (caught 2026-09-23 — an order sat at `shipped` for ten minutes).
  Future<void> setToken(String accessToken) async {
    _accessToken = accessToken;
    await _requireDb().setToken(accessToken);
  }

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
  void Function(bool isConnected) onConnected, {
  void Function()? onAccessRevoked,
}) => connectionState.listen((state) {
  onConnected(state == NostosConnectionState.connected);
  if (state == NostosConnectionState.accessRevoked) onAccessRevoked?.call();
});

/// Opens Atlet's direct-mode database. Shared by
/// [NostosAdapter.direct] and the push pilot's background wake, which must
/// land on the SAME SQLite file with the same scope.
Future<NostosDatabase> openNostosDirect({
  required String supabaseUrl,
  required String anonKey,
  required String accessToken,
  required String userId,
  required String dbDir,
}) => NostosDatabase.direct(
  supabaseUrl: supabaseUrl,
  anonKey: anonKey,
  // The scope the change-log trigger stamps, and the private Realtime
  // channel this device may join — see .nostos/direct.sql's
  // `nostos.current_scopes()`.
  scope: 'sub:$userId',
  token: accessToken,
  schema: _schema,
  sqlitePath: '$dbDir/nostos_direct.sqlite',
  // ADR-0049: product decision 2026-09-26 — keep this user's rows across
  // sign-out (the engine still wipes when a different `sub` signs in).
  keepLocalOnSignOut: true,
);

final NostosSchema _schema = NostosSchema(
  tables: [
    NostosTable(
      name: 'sessions',
      primaryKey: const ['id'],
      columns: [
        NostosColumn.text('id'),
        NostosColumn.text('title'),
        NostosColumn.text('type'),
        NostosColumn.integer('metric'),
        NostosColumn.text('unit'),
        NostosColumn.text('note'),
        NostosColumn.integer('streak'),
        NostosColumn.text('occurred_on'),
        NostosColumn.text('server_committed_at'),
      ],
    ),
    NostosTable(
      name: 'products',
      primaryKey: const ['id'],
      columns: [
        NostosColumn.text('id'),
        NostosColumn.text('name'),
        NostosColumn.text('category'),
        NostosColumn.integer('price_cents'),
        NostosColumn.real('rating'),
        NostosColumn.integer('plant_based'),
        NostosColumn.text('image_url'),
        NostosColumn.text('image_id'),
      ],
    ),
    NostosTable(
      name: 'user_profiles',
      primaryKey: const ['id'],
      columns: [
        NostosColumn.text('id'),
        NostosColumn.text('user_id'),
        NostosColumn.text('display_name'),
        NostosColumn.integer('active'),
      ],
    ),
    // T6 attachments metadata (ADR-0034): the product-images catalog, public
    // scope, read-only on the device — see migration 0012.
    NostosTable(
      name: AttachmentSchema.table,
      primaryKey: const ['id'],
      columns: [
        NostosColumn.text(AttachmentSchema.colId),
        NostosColumn.text(AttachmentSchema.colFilename),
        NostosColumn.integer(AttachmentSchema.colSize),
        NostosColumn.text(AttachmentSchema.colMediaType),
        NostosColumn.text(AttachmentSchema.colState),
        NostosColumn.integer(AttachmentSchema.colTimestamp),
      ],
    ),
    NostosTable(
      name: 'cart_items',
      primaryKey: const ['id'],
      columns: [
        NostosColumn.text('id'),
        NostosColumn.text('product_id'),
        NostosColumn.integer('qty'),
        NostosColumn.text('added_at'),
      ],
    ),
    NostosTable(
      name: 'order_events',
      primaryKey: const ['id'],
      columns: [
        NostosColumn.text('id'),
        NostosColumn.text('order_id'),
        NostosColumn.text('status'),
        NostosColumn.text('previous_status'),
        NostosColumn.text('note'),
        NostosColumn.text('created_at'),
      ],
    ),
    NostosTable(
      name: 'orders',
      primaryKey: const ['id'],
      columns: [
        NostosColumn.text('id'),
        NostosColumn.text('status'),
        NostosColumn.integer('subtotal_cents'),
        NostosColumn.integer('tax_cents'),
        NostosColumn.integer('shipping_cents'),
        NostosColumn.integer('total_cents'),
        NostosColumn.text('payment_ref'),
        NostosColumn.text('items_json'),
        NostosColumn.text('created_at'),
      ],
    ),
  ],
);

/// Maps a decoded `cart_items` row to [CartItemRow]. Top-level and pure —
/// same testability rationale as [sessionFromRow].
CartItemRow cartItemFromRow(Map<String, dynamic> row) => CartItemRow(
  id: row['id'] as String,
  productId: row['product_id'] as String,
  qty: _asInt(row['qty']),
  addedAt: DateTime.parse(row['added_at'] as String),
);

/// Maps a decoded `order_events` row to [OrderEventRow]. Read-only table —
/// there is no matching write payload; migration 0007's trigger is the only
/// writer.
OrderEventRow orderEventFromRow(Map<String, dynamic> row) => OrderEventRow(
  id: row['id'] as String,
  orderId: row['order_id'] as String,
  status: row['status'] as String,
  previousStatus: row['previous_status'] as String?,
  note: row['note'] as String?,
  createdAt: DateTime.parse(row['created_at'] as String),
);

/// Maps a decoded `orders` row to [OrderRow].
OrderRow orderFromRow(Map<String, dynamic> row) => OrderRow(
  id: row['id'] as String,
  status: row['status'] as String,
  userId: row['user_id'] as String?,
  subtotalCents: _asInt(row['subtotal_cents']),
  taxCents: _asInt(row['tax_cents']),
  shippingCents: _asInt(row['shipping_cents']),
  totalCents: _asInt(row['total_cents']),
  paymentRef: row['payment_ref'] as String?,
  itemsJson: row['items_json'] as String?,
  createdAt: DateTime.parse(row['created_at'] as String),
);

UserProfileRow userProfileFromRow(Map<String, dynamic> row) => UserProfileRow(
  id: row['user_id'] as String,
  displayName: row['display_name'] as String,
  active: _asBool(row['active']),
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
  'user_id': ?(o.userId ?? userId),
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
  imageId: row['image_id'] as String?,
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
