// Shared cart/orders/updateSession defaults for test fakes.
//
// Mixed into per-file `_FakeAdapter`s so the shop-flow surface added to
// [SyncAdapter] doesn't force every test double to hand-roll it. Classes
// can still override any member with recording behaviour.
import 'dart:async';

import 'package:atlet/adapters/sync_adapter.dart';

mixin FakeCartOrdersDefaults implements SyncAdapter {
  final List<CartItemRow> fakeCart = [];
  final List<OrderRow> fakeOrders = [];
  final StreamController<List<CartItemRow>> _fakeCartCtrl =
      StreamController.broadcast();
  final StreamController<List<OrderRow>> _fakeOrdersCtrl =
      StreamController.broadcast();

  @override
  Future<void> updateSession(SessionRow s) async {}

  @override
  Future<void> addToCart(CartItemRow item) async {
    fakeCart.removeWhere((i) => i.id == item.id);
    fakeCart.add(item);
    _fakeCartCtrl.add(List.unmodifiable(fakeCart));
  }

  @override
  Future<void> removeCartItem(String id) async {
    fakeCart.removeWhere((i) => i.id == id);
    _fakeCartCtrl.add(List.unmodifiable(fakeCart));
  }

  @override
  Future<void> clearCart() async {
    fakeCart.clear();
    _fakeCartCtrl.add(const []);
  }

  @override
  Future<String> placeOrder(OrderRow o) async {
    fakeOrders.insert(0, o);
    fakeCart.clear();
    _fakeOrdersCtrl.add(List.unmodifiable(fakeOrders));
    _fakeCartCtrl.add(const []);
    return o.id;
  }

  @override
  Stream<List<CartItemRow>> watchCart() async* {
    yield List.unmodifiable(fakeCart);
    yield* _fakeCartCtrl.stream;
  }

  @override
  Stream<List<OrderRow>> watchOrders() async* {
    yield List.unmodifiable(fakeOrders);
    yield* _fakeOrdersCtrl.stream;
  }
}
