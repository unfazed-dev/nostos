// Widget tests for lib/ui/shop.dart (task-13). Same fake-adapter/flush
// pattern as training_ui_test.dart: proves the grid renders exclusively
// from watchProducts() emissions, gates the null-first-frame correctly
// (unknown, not empty), and survives the full 1k-row seed shape without
// per-row jank — GridView.builder only builds what's on/near screen.

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:atlet/adapters/sync_adapter.dart';
import 'package:atlet/ui/shop.dart';

class _FakeAdapter implements SyncAdapter {
  final _controller = StreamController<List<ProductRow>>.broadcast();

  @override
  String get engine => 'fake';

  @override
  Future<void> init({
    required String supabaseUrl,
    required String accessToken,
    required String userId,
    required String dbDir,
  }) async {}

  @override
  Future<void> signOut() async {}

  @override
  Future<String> addSession(SessionRow s) async => s.id;

  @override
  Future<void> deleteSession(String id) async {}

  @override
  Stream<List<SessionRow>> watchSessions() => const Stream.empty();

  @override
  Stream<List<ProductRow>> watchProducts() => _controller.stream;

  @override
  Stream<bool> get connected => const Stream.empty();

  @override
  Future<void> setConnected(bool up) async {}

  @override
  Stream<SyncMark> get marks => const Stream.empty();

  void push(List<ProductRow> rows) => _controller.add(rows);

  void dispose() => _controller.close();
}

ProductRow _fixture({String id = 'p1', String name = 'Whey Isolate'}) => ProductRow(
      id: id,
      name: name,
      category: 'protein',
      priceCents: 3499,
      rating: 4.6,
      plantBased: false,
      imageUrl: 'design/img/p1-protein.jpg',
    );

void main() {
  testWidgets('shop grid renders only after watchProducts emits', (tester) async {
    final adapter = _FakeAdapter();
    addTearDown(adapter.dispose);

    await tester.pumpWidget(MaterialApp(home: ShopScreen(adapter: adapter)));
    await tester.pump();
    // No emission yet — must show a loading state, not an empty-products
    // message (the two are different facts and must render differently).
    expect(find.byType(CircularProgressIndicator), findsOneWidget);
    expect(find.text('Whey Isolate'), findsNothing);
    expect(find.text('No products yet.'), findsNothing);

    adapter.push([_fixture()]);
    await tester.pumpAndSettle();

    expect(find.byType(CircularProgressIndicator), findsNothing);
    expect(find.text('Whey Isolate'), findsOneWidget);
    expect(find.text('\$34.99'), findsOneWidget);
  });

  testWidgets('empty emission renders the empty state, not the loader', (tester) async {
    final adapter = _FakeAdapter();
    addTearDown(adapter.dispose);

    await tester.pumpWidget(MaterialApp(home: ShopScreen(adapter: adapter)));
    adapter.push(const []);
    await tester.pumpAndSettle();

    expect(find.text('No products yet.'), findsOneWidget);
    expect(find.byType(CircularProgressIndicator), findsNothing);
  });

  testWidgets('null adapter shows the no-engine message', (tester) async {
    await tester.pumpWidget(const MaterialApp(home: ShopScreen(adapter: null)));
    await tester.pump();
    expect(find.textContaining('No sync engine selected'), findsOneWidget);
  });

  testWidgets('grid handles the full 1k-row seed without throwing', (tester) async {
    final adapter = _FakeAdapter();
    addTearDown(adapter.dispose);

    final rows = [for (var i = 0; i < 1000; i++) _fixture(id: 'p$i', name: 'Product $i')];

    await tester.pumpWidget(MaterialApp(home: ShopScreen(adapter: adapter)));
    adapter.push(rows);
    await tester.pumpAndSettle();

    expect(tester.takeException(), isNull);
    expect(find.byKey(const Key('shop-grid')), findsOneWidget);
    expect(find.text('Product 0'), findsOneWidget);
    // GridView.builder is lazy: only near-viewport rows are built, so the
    // tail of a 1k-row list must not be in the tree yet.
    expect(find.text('Product 999'), findsNothing);
  });
}
