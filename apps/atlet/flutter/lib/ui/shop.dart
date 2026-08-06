import 'package:flutter/material.dart';

import '../adapters/sync_adapter.dart';
import '../design/tokens.dart';

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

String _priceFor(ProductRow p) => (p.priceCents / 100).toStringAsFixed(2);

/// Shop: bulk read-only product grid rendered exclusively from
/// [SyncAdapter.watchProducts] (1k seeded rows). This is the cold-sync /
/// stress fixture — no per-row write path exists, so unlike [TrainingHome]
/// there is nothing to echo back; the grid just needs to render the full
/// snapshot lazily (`GridView.builder`) without per-row jank.
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
        return Scaffold(
          key: const Key('shop-screen'),
          backgroundColor: AtletTokens.bone,
          appBar: AppBar(
            backgroundColor: AtletTokens.bone,
            elevation: 0,
            title: Text('Shop', style: TextStyle(color: AtletTokens.ink)),
          ),
          body: products.isEmpty
              ? const _Message('No products yet.')
              : GridView.builder(
                  key: const Key('shop-grid'),
                  padding: const EdgeInsets.fromLTRB(16, 8, 16, 24),
                  gridDelegate: const SliverGridDelegateWithFixedCrossAxisCount(
                    crossAxisCount: 2,
                    mainAxisSpacing: 12,
                    crossAxisSpacing: 12,
                    childAspectRatio: 0.61,
                  ),
                  itemCount: products.length,
                  itemBuilder: (context, i) => _ProductCard(product: products[i]),
                ),
        );
      },
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
  const _ProductCard({required this.product});

  final ProductRow product;

  @override
  Widget build(BuildContext context) {
    final assetPath = _assetPathFor(product);
    return Material(
      key: Key('product-card-${product.id}'),
      color: AtletTokens.paper,
      borderRadius: BorderRadius.circular(16),
      clipBehavior: Clip.antiAlias,
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
                    errorBuilder: (context, error, stack) => const _ImageFallback(),
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
                        '\$${_priceFor(product)}',
                        style: TextStyle(
                          fontSize: AtletTokens.footnote,
                          fontWeight: FontWeight.w600,
                          color: AtletTokens.ink,
                          fontFamily: AtletTokens.monoFamily,
                        ),
                      ),
                      if (product.plantBased)
                        Icon(Icons.eco_outlined, size: 14, color: AtletTokens.good),
                    ],
                  ),
                ],
              ),
            ),
          ),
        ],
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
