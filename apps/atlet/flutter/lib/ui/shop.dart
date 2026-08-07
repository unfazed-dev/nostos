import 'dart:convert';

import 'package:flutter/material.dart';

import '../adapters/sync_adapter.dart';
import '../design/tokens.dart';
import 'connectivity_led.dart';
import '../util/uuid.dart';

/// Product image asset path for [ProductRow.imageUrl]. Seed data
/// (apps/atlet/supabase/seed/products_seed.sql) stores paths as
/// `design/img/pN-category.jpg`, relative to `apps/atlet/`. This app's
/// pubspec.yaml lives one level down at `apps/atlet/flutter/`, so the asset
/// key Flutter registers (declared as `../design/img/` in pubspec.yaml) is
/// the same string with a `../` prefix. No network fetch, no invented asset
/// files — only the six frozen images the seed already cycles through.
String? _assetPathFor(ProductRow p) {
  final url = p.imageUrl;
  return url == null ? null : '../$url';
}

String _titleCase(String s) => s.isEmpty ? s : s[0].toUpperCase() + s.substring(1);

String _money(int cents) => '\$${(cents / 100).toStringAsFixed(2)}';

/// Checkout math mirrors design/data.jsx: TAX_RATE 0.09, standard shipping
/// $5.00 waived at a $50 subtotal (`ship-free` threshold).
const double _taxRate = 0.09;
const int _shippingCents = 500;
const int _freeShippingThresholdCents = 5000;

int _taxFor(int subtotalCents) => (subtotalCents * _taxRate).round();
int _shippingFor(int subtotalCents) =>
    subtotalCents >= _freeShippingThresholdCents ? 0 : _shippingCents;

/// Shop: product grid rendered from [SyncAdapter.watchProducts] (1k seeded
/// rows) plus the cart/checkout write path (ADR-0029 collapsed writes):
/// product detail sheet -> addToCart upsert -> cart sheet with qty steppers
/// -> mock payment -> placeOrder + clearCart. Cart badge and lines come from
/// [SyncAdapter.watchCart], so remote edits to the same cart replay here too.
class ShopScreen extends StatelessWidget {
  const ShopScreen({super.key, required this.adapter});

  final SyncAdapter? adapter;

  @override
  Widget build(BuildContext context) {
    final adapter = this.adapter;
    if (adapter == null) {
      return const _StatusScaffold(
        child: _Message('No sync engine selected.\nOpen Settings to pick one.'),
      );
    }
    return StreamBuilder<List<ProductRow>>(
      stream: adapter.watchProducts(),
      builder: (context, snapshot) {
        if (!snapshot.hasData) {
          // Stream hasn't emitted yet — unknown, not empty. Rendering the
          // empty state here would flash "no products" on every cold start
          // before the first snapshot arrives (same gate as SessionDetail).
          return const _StatusScaffold(
            child: Center(
              child: CircularProgressIndicator(color: AtletTokens.accent),
            ),
          );
        }

        final products = snapshot.data!;
        final byId = {for (final p in products) p.id: p};
        return StreamBuilder<List<CartItemRow>>(
          stream: adapter.watchCart(),
          builder: (context, cartSnap) {
            final cart = cartSnap.data ?? const <CartItemRow>[];
            final cartCount = cart.fold<int>(0, (n, c) => n + c.qty);
            return Scaffold(
              key: const Key('shop-screen'),
              backgroundColor: AtletTokens.bone,
              appBar: AppBar(
                backgroundColor: AtletTokens.bone,
                elevation: 0,
                title: Text('Shop', style: TextStyle(color: AtletTokens.ink)),
                actions: const [ConnectivityLed()],
              ),
              floatingActionButton: cartCount == 0
                  ? null
                  : FloatingActionButton.extended(
                      key: const Key('cart-fab'),
                      backgroundColor: AtletTokens.accent,
                      foregroundColor: AtletTokens.textOnAccent,
                      icon: const Icon(Icons.shopping_bag_outlined),
                      label: Text('Cart · $cartCount'),
                      onPressed: () => _openCart(context, adapter, byId),
                    ),
              body: products.isEmpty
                  ? const _Message('No products yet.')
                  : GridView.builder(
                      key: const Key('shop-grid'),
                      padding: const EdgeInsets.fromLTRB(16, 8, 16, 24),
                      gridDelegate:
                          const SliverGridDelegateWithFixedCrossAxisCount(
                        crossAxisCount: 2,
                        mainAxisSpacing: 12,
                        crossAxisSpacing: 12,
                        childAspectRatio: 0.61,
                      ),
                      itemCount: products.length,
                      itemBuilder: (context, i) => _ProductCard(
                        product: products[i],
                        onTap: () =>
                            _openDetail(context, adapter, products[i]),
                      ),
                    ),
            );
          },
        );
      },
    );
  }

  void _openDetail(BuildContext context, SyncAdapter adapter, ProductRow p) {
    showModalBottomSheet<void>(
      context: context,
      backgroundColor: AtletTokens.paper,
      isScrollControlled: true,
      shape: const RoundedRectangleBorder(
        borderRadius: BorderRadius.vertical(top: Radius.circular(20)),
      ),
      builder: (_) => _ProductDetailSheet(adapter: adapter, product: p),
    );
  }

  void _openCart(
    BuildContext context,
    SyncAdapter adapter,
    Map<String, ProductRow> byId,
  ) {
    showModalBottomSheet<void>(
      context: context,
      backgroundColor: AtletTokens.paper,
      isScrollControlled: true,
      shape: const RoundedRectangleBorder(
        borderRadius: BorderRadius.vertical(top: Radius.circular(20)),
      ),
      builder: (_) => _CartSheet(adapter: adapter, productsById: byId),
    );
  }
}

class _StatusScaffold extends StatelessWidget {
  const _StatusScaffold({required this.child});

  final Widget child;

  @override
  Widget build(BuildContext context) {
    return Scaffold(backgroundColor: AtletTokens.bone, body: child);
  }
}

class _Message extends StatelessWidget {
  const _Message(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    return Center(
      child: Text(
        text,
        textAlign: TextAlign.center,
        style: TextStyle(color: AtletTokens.ink3, fontSize: AtletTokens.body),
      ),
    );
  }
}

class _ProductCard extends StatelessWidget {
  const _ProductCard({required this.product, required this.onTap});

  final ProductRow product;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final assetPath = _assetPathFor(product);
    return Material(
      key: Key('product-card-${product.id}'),
      color: AtletTokens.paper,
      borderRadius: BorderRadius.circular(16),
      clipBehavior: Clip.antiAlias,
      child: InkWell(
        onTap: onTap,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            AspectRatio(
              aspectRatio: 1,
              child: assetPath == null
                  ? const _ImageFallback()
                  : Image.asset(
                      assetPath,
                      fit: BoxFit.cover,
                      errorBuilder: (context, error, stack) =>
                          const _ImageFallback(),
                    ),
            ),
            Expanded(
              child: Padding(
                padding: const EdgeInsets.all(12),
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: [
                    Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text(
                          _titleCase(product.category),
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: TextStyle(
                            fontSize: AtletTokens.footnote,
                            color: AtletTokens.ink3,
                          ),
                        ),
                        const SizedBox(height: 2),
                        Text(
                          product.name,
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: TextStyle(
                            fontSize: AtletTokens.body,
                            fontWeight: FontWeight.w600,
                            color: AtletTokens.ink,
                          ),
                        ),
                      ],
                    ),
                    Row(
                      mainAxisAlignment: MainAxisAlignment.spaceBetween,
                      children: [
                        Text(
                          _money(product.priceCents),
                          style: TextStyle(
                            fontSize: AtletTokens.footnote,
                            fontWeight: FontWeight.w600,
                            color: AtletTokens.ink,
                            fontFamily: AtletTokens.monoFamily,
                          ),
                        ),
                        if (product.plantBased)
                          Icon(Icons.eco_outlined,
                              size: 14, color: AtletTokens.good),
                      ],
                    ),
                  ],
                ),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _ImageFallback extends StatelessWidget {
  const _ImageFallback();

  @override
  Widget build(BuildContext context) {
    return Container(
      color: AtletTokens.bone2,
      alignment: Alignment.center,
      child: Icon(Icons.image_outlined, color: AtletTokens.ink3),
    );
  }
}

/// Product detail: image, meta, qty stepper, add-to-cart. Add is an upsert:
/// if the product already has a cart line, its qty is incremented on the
/// SAME row id (the adapter contract treats a known id as a qty edit).
class _ProductDetailSheet extends StatefulWidget {
  const _ProductDetailSheet({required this.adapter, required this.product});

  final SyncAdapter adapter;
  final ProductRow product;

  @override
  State<_ProductDetailSheet> createState() => _ProductDetailSheetState();
}

class _ProductDetailSheetState extends State<_ProductDetailSheet> {
  int _qty = 1;
  bool _adding = false;

  Future<void> _addToCart() async {
    setState(() => _adding = true);
    try {
      // Upsert against the current cart snapshot: one line per product.
      final existing = (await widget.adapter.watchCart().first)
          .where((c) => c.productId == widget.product.id)
          .toList();
      final line = existing.isEmpty
          ? CartItemRow(
              id: uuidV4(),
              productId: widget.product.id,
              qty: _qty,
              addedAt: DateTime.now().toUtc(),
            )
          : CartItemRow(
              id: existing.first.id,
              productId: widget.product.id,
              qty: existing.first.qty + _qty,
              addedAt: existing.first.addedAt,
            );
      await widget.adapter.addToCart(line);
      if (!mounted) return;
      Navigator.of(context).pop();
      ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('Added ${widget.product.name} to cart')),
      );
    } catch (e) {
      if (!mounted) return;
      setState(() => _adding = false);
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text('Add failed: $e')));
    }
  }

  @override
  Widget build(BuildContext context) {
    final p = widget.product;
    final assetPath = _assetPathFor(p);
    final rating = p.rating;
    return SafeArea(
      child: Padding(
        padding: const EdgeInsets.fromLTRB(20, 12, 20, 20),
        child: Column(
          key: const Key('product-detail'),
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            ClipRRect(
              borderRadius: BorderRadius.circular(16),
              child: AspectRatio(
                aspectRatio: 1.6,
                child: assetPath == null
                    ? const _ImageFallback()
                    : Image.asset(
                        assetPath,
                        fit: BoxFit.cover,
                        errorBuilder: (context, error, stack) =>
                            const _ImageFallback(),
                      ),
              ),
            ),
            const SizedBox(height: 16),
            Text(
              _titleCase(p.category),
              style: TextStyle(
                fontSize: AtletTokens.footnote,
                color: AtletTokens.ink3,
              ),
            ),
            const SizedBox(height: 4),
            Text(
              p.name,
              style: TextStyle(
                fontSize: AtletTokens.title2,
                fontWeight: FontWeight.w700,
                color: AtletTokens.ink,
              ),
            ),
            const SizedBox(height: 8),
            Row(
              children: [
                Text(
                  _money(p.priceCents),
                  style: TextStyle(
                    fontSize: AtletTokens.body,
                    fontWeight: FontWeight.w600,
                    color: AtletTokens.ink,
                    fontFamily: AtletTokens.monoFamily,
                  ),
                ),
                const SizedBox(width: 12),
                if (rating != null) ...[
                  Icon(Icons.star, size: 16, color: AtletTokens.warn),
                  const SizedBox(width: 2),
                  Text(
                    rating.toStringAsFixed(1),
                    style: TextStyle(
                      fontSize: AtletTokens.footnote,
                      color: AtletTokens.ink3,
                    ),
                  ),
                ],
                if (p.plantBased) ...[
                  const SizedBox(width: 12),
                  Icon(Icons.eco_outlined, size: 16, color: AtletTokens.good),
                  const SizedBox(width: 2),
                  Text(
                    'Plant based',
                    style: TextStyle(
                      fontSize: AtletTokens.footnote,
                      color: AtletTokens.good,
                    ),
                  ),
                ],
              ],
            ),
            const SizedBox(height: 20),
            Row(
              children: [
                _QtyStepper(
                  qty: _qty,
                  onChanged: (q) => setState(() => _qty = q),
                ),
                const SizedBox(width: 16),
                Expanded(
                  child: FilledButton(
                    key: const Key('add-to-cart'),
                    style: FilledButton.styleFrom(
                      backgroundColor: AtletTokens.accent,
                      foregroundColor: AtletTokens.textOnAccent,
                      padding: const EdgeInsets.symmetric(vertical: 14),
                    ),
                    onPressed: _adding ? null : _addToCart,
                    child: Text(_adding ? 'Adding…' : 'Add to cart'),
                  ),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}

class _QtyStepper extends StatelessWidget {
  const _QtyStepper({required this.qty, required this.onChanged});

  final int qty;
  final ValueChanged<int> onChanged;

  @override
  Widget build(BuildContext context) {
    return Container(
      decoration: BoxDecoration(
        border: Border.all(color: AtletTokens.rule),
        borderRadius: BorderRadius.circular(12),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          IconButton(
            icon: const Icon(Icons.remove, size: 18),
            color: AtletTokens.ink,
            onPressed: qty > 1 ? () => onChanged(qty - 1) : null,
          ),
          Text(
            '$qty',
            style: TextStyle(
              fontSize: AtletTokens.body,
              fontWeight: FontWeight.w600,
              color: AtletTokens.ink,
              fontFamily: AtletTokens.monoFamily,
            ),
          ),
          IconButton(
            icon: const Icon(Icons.add, size: 18),
            color: AtletTokens.ink,
            onPressed: () => onChanged(qty + 1),
          ),
        ],
      ),
    );
  }
}

/// Cart sheet: live lines from watchCart with qty steppers (qty 0 removes
/// the line), subtotal, and checkout entry.
class _CartSheet extends StatelessWidget {
  const _CartSheet({required this.adapter, required this.productsById});

  final SyncAdapter adapter;
  final Map<String, ProductRow> productsById;

  @override
  Widget build(BuildContext context) {
    return StreamBuilder<List<CartItemRow>>(
      stream: adapter.watchCart(),
      builder: (context, snap) {
        final cart = snap.data ?? const <CartItemRow>[];
        final subtotal = cart.fold<int>(
          0,
          (n, c) => n + (productsById[c.productId]?.priceCents ?? 0) * c.qty,
        );
        return SafeArea(
          child: Padding(
            padding: const EdgeInsets.fromLTRB(20, 12, 20, 20),
            child: Column(
              key: const Key('cart-sheet'),
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  'Cart',
                  style: TextStyle(
                    fontSize: AtletTokens.title2,
                    fontWeight: FontWeight.w700,
                    color: AtletTokens.ink,
                  ),
                ),
                const SizedBox(height: 12),
                if (cart.isEmpty)
                  Padding(
                    padding: const EdgeInsets.symmetric(vertical: 24),
                    child: Center(
                      child: Text(
                        'Cart is empty.',
                        style: TextStyle(
                          color: AtletTokens.ink3,
                          fontSize: AtletTokens.body,
                        ),
                      ),
                    ),
                  )
                else
                  Flexible(
                    child: ListView.separated(
                      shrinkWrap: true,
                      itemCount: cart.length,
                      separatorBuilder: (context, index) =>
                          Divider(color: AtletTokens.rule, height: 1),
                      itemBuilder: (context, i) {
                        final line = cart[i];
                        final product = productsById[line.productId];
                        return _CartLine(
                          line: line,
                          product: product,
                          onQty: (q) async {
                            if (q <= 0) {
                              await adapter.removeCartItem(line.id);
                            } else {
                              await adapter.addToCart(CartItemRow(
                                id: line.id,
                                productId: line.productId,
                                qty: q,
                                addedAt: line.addedAt,
                              ));
                            }
                          },
                        );
                      },
                    ),
                  ),
                const SizedBox(height: 16),
                Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  children: [
                    Text(
                      'Subtotal',
                      style: TextStyle(
                        fontSize: AtletTokens.body,
                        color: AtletTokens.ink3,
                      ),
                    ),
                    Text(
                      _money(subtotal),
                      key: const Key('cart-subtotal'),
                      style: TextStyle(
                        fontSize: AtletTokens.body,
                        fontWeight: FontWeight.w700,
                        color: AtletTokens.ink,
                        fontFamily: AtletTokens.monoFamily,
                      ),
                    ),
                  ],
                ),
                const SizedBox(height: 12),
                SizedBox(
                  width: double.infinity,
                  child: FilledButton(
                    key: const Key('checkout-button'),
                    style: FilledButton.styleFrom(
                      backgroundColor: AtletTokens.accent,
                      foregroundColor: AtletTokens.textOnAccent,
                      padding: const EdgeInsets.symmetric(vertical: 14),
                    ),
                    onPressed: cart.isEmpty
                        ? null
                        : () {
                            Navigator.of(context).pop();
                            showModalBottomSheet<void>(
                              context: context,
                              backgroundColor: AtletTokens.paper,
                              isScrollControlled: true,
                              shape: const RoundedRectangleBorder(
                                borderRadius: BorderRadius.vertical(
                                    top: Radius.circular(20)),
                              ),
                              builder: (_) => _CheckoutSheet(
                                adapter: adapter,
                                cart: cart,
                                productsById: productsById,
                              ),
                            );
                          },
                    child: const Text('Checkout'),
                  ),
                ),
              ],
            ),
          ),
        );
      },
    );
  }
}

class _CartLine extends StatelessWidget {
  const _CartLine({
    required this.line,
    required this.product,
    required this.onQty,
  });

  final CartItemRow line;
  final ProductRow? product;
  final ValueChanged<int> onQty;

  @override
  Widget build(BuildContext context) {
    final p = product;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 8),
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  p?.name ?? 'Unknown product',
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    fontSize: AtletTokens.body,
                    fontWeight: FontWeight.w600,
                    color: AtletTokens.ink,
                  ),
                ),
                const SizedBox(height: 2),
                Text(
                  p == null ? '—' : _money(p.priceCents * line.qty),
                  style: TextStyle(
                    fontSize: AtletTokens.footnote,
                    color: AtletTokens.ink3,
                    fontFamily: AtletTokens.monoFamily,
                  ),
                ),
              ],
            ),
          ),
          _QtyStepperCompact(qty: line.qty, onChanged: onQty),
        ],
      ),
    );
  }
}

/// Compact stepper for cart lines: minus at qty 1 deletes the line.
class _QtyStepperCompact extends StatelessWidget {
  const _QtyStepperCompact({required this.qty, required this.onChanged});

  final int qty;
  final ValueChanged<int> onChanged;

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        IconButton(
          icon: Icon(qty == 1 ? Icons.delete_outline : Icons.remove, size: 18),
          color: qty == 1 ? AtletTokens.accent2 : AtletTokens.ink,
          onPressed: () => onChanged(qty - 1),
        ),
        Text(
          '$qty',
          style: TextStyle(
            fontSize: AtletTokens.body,
            fontWeight: FontWeight.w600,
            color: AtletTokens.ink,
            fontFamily: AtletTokens.monoFamily,
          ),
        ),
        IconButton(
          icon: const Icon(Icons.add, size: 18),
          color: AtletTokens.ink,
          onPressed: () => onChanged(qty + 1),
        ),
      ],
    );
  }
}

/// Checkout: order summary + mock card. "Pay" simulates a short processing
/// delay then writes the order through the adapter (placeOrder) and clears
/// the cart — both are nostos-synced writes; no real payment rail exists.
class _CheckoutSheet extends StatefulWidget {
  const _CheckoutSheet({
    required this.adapter,
    required this.cart,
    required this.productsById,
  });

  final SyncAdapter adapter;
  final List<CartItemRow> cart;
  final Map<String, ProductRow> productsById;

  @override
  State<_CheckoutSheet> createState() => _CheckoutSheetState();
}

class _CheckoutSheetState extends State<_CheckoutSheet> {
  bool _paying = false;
  String? _orderId;

  int get _subtotal => widget.cart.fold<int>(
        0,
        (n, c) =>
            n + (widget.productsById[c.productId]?.priceCents ?? 0) * c.qty,
      );

  Future<void> _pay() async {
    setState(() => _paying = true);
    try {
      // Mock processing delay — this is a demo payment, not a rail.
      await Future<void>.delayed(const Duration(milliseconds: 900));
      final subtotal = _subtotal;
      final tax = _taxFor(subtotal);
      final shipping = _shippingFor(subtotal);
      final items = [
        for (final c in widget.cart)
          {
            'product_id': c.productId,
            'name': widget.productsById[c.productId]?.name,
            'qty': c.qty,
            'price_cents':
                widget.productsById[c.productId]?.priceCents ?? 0,
          },
      ];
      final orderId = await widget.adapter.placeOrder(OrderRow(
        id: uuidV4(),
        status: 'paid',
        subtotalCents: subtotal,
        taxCents: tax,
        shippingCents: shipping,
        totalCents: subtotal + tax + shipping,
        paymentRef: 'demo-visa-4242',
        itemsJson: jsonEncode(items),
        createdAt: DateTime.now().toUtc(),
      ));
      await widget.adapter.clearCart();
      if (!mounted) return;
      setState(() {
        _paying = false;
        _orderId = orderId;
      });
    } catch (e) {
      if (!mounted) return;
      setState(() => _paying = false);
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text('Payment failed: $e')));
    }
  }

  @override
  Widget build(BuildContext context) {
    final subtotal = _subtotal;
    final tax = _taxFor(subtotal);
    final shipping = _shippingFor(subtotal);
    final total = subtotal + tax + shipping;
    final orderId = _orderId;

    return SafeArea(
      child: Padding(
        padding: const EdgeInsets.fromLTRB(20, 12, 20, 20),
        child: orderId != null
            ? Column(
                key: const Key('order-confirmation'),
                mainAxisSize: MainAxisSize.min,
                children: [
                  Icon(Icons.check_circle_outline,
                      size: 48, color: AtletTokens.good),
                  const SizedBox(height: 12),
                  Text(
                    'Order placed',
                    style: TextStyle(
                      fontSize: AtletTokens.title2,
                      fontWeight: FontWeight.w700,
                      color: AtletTokens.ink,
                    ),
                  ),
                  const SizedBox(height: 4),
                  Text(
                    '${_money(total)} · ${orderId.substring(0, 8)}',
                    style: TextStyle(
                      fontSize: AtletTokens.footnote,
                      color: AtletTokens.ink3,
                      fontFamily: AtletTokens.monoFamily,
                    ),
                  ),
                  const SizedBox(height: 16),
                  SizedBox(
                    width: double.infinity,
                    child: FilledButton(
                      key: const Key('order-done'),
                      style: FilledButton.styleFrom(
                        backgroundColor: AtletTokens.accent,
                        foregroundColor: AtletTokens.textOnAccent,
                        padding: const EdgeInsets.symmetric(vertical: 14),
                      ),
                      onPressed: () => Navigator.of(context).pop(),
                      child: const Text('Done'),
                    ),
                  ),
                ],
              )
            : Column(
                key: const Key('checkout-sheet'),
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    'Checkout',
                    style: TextStyle(
                      fontSize: AtletTokens.title2,
                      fontWeight: FontWeight.w700,
                      color: AtletTokens.ink,
                    ),
                  ),
                  const SizedBox(height: 16),
                  _SummaryRow('Subtotal', _money(subtotal)),
                  _SummaryRow(
                    'Shipping',
                    shipping == 0 ? 'Free' : _money(shipping),
                  ),
                  _SummaryRow('Tax (9%)', _money(tax)),
                  Divider(color: AtletTokens.rule, height: 24),
                  _SummaryRow('Total', _money(total), emphasized: true),
                  const SizedBox(height: 20),
                  Container(
                    padding: const EdgeInsets.all(14),
                    decoration: BoxDecoration(
                      border: Border.all(color: AtletTokens.rule),
                      borderRadius: BorderRadius.circular(12),
                    ),
                    child: Row(
                      children: [
                        Icon(Icons.credit_card, color: AtletTokens.ink3),
                        const SizedBox(width: 12),
                        Text(
                          'Visa •••• 4242 (demo)',
                          style: TextStyle(
                            fontSize: AtletTokens.body,
                            color: AtletTokens.ink,
                            fontFamily: AtletTokens.monoFamily,
                          ),
                        ),
                      ],
                    ),
                  ),
                  const SizedBox(height: 16),
                  SizedBox(
                    width: double.infinity,
                    child: FilledButton(
                      key: const Key('pay-button'),
                      style: FilledButton.styleFrom(
                        backgroundColor: AtletTokens.accent,
                        foregroundColor: AtletTokens.textOnAccent,
                        padding: const EdgeInsets.symmetric(vertical: 14),
                      ),
                      onPressed: _paying ? null : _pay,
                      child: Text(
                          _paying ? 'Processing…' : 'Pay ${_money(total)}'),
                    ),
                  ),
                ],
              ),
      ),
    );
  }
}

class _SummaryRow extends StatelessWidget {
  const _SummaryRow(this.label, this.value, {this.emphasized = false});

  final String label;
  final String value;
  final bool emphasized;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 3),
      child: Row(
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        children: [
          Text(
            label,
            style: TextStyle(
              fontSize: AtletTokens.body,
              color: emphasized ? AtletTokens.ink : AtletTokens.ink3,
              fontWeight: emphasized ? FontWeight.w700 : FontWeight.w400,
            ),
          ),
          Text(
            value,
            style: TextStyle(
              fontSize: AtletTokens.body,
              color: AtletTokens.ink,
              fontWeight: emphasized ? FontWeight.w700 : FontWeight.w600,
              fontFamily: AtletTokens.monoFamily,
            ),
          ),
        ],
      ),
    );
  }
}
