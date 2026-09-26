import 'package:flutter/material.dart';

import '../adapters/nostos_adapter.dart';
import '../adapters/sync_adapter.dart';
import '../design/tokens.dart';
import '../util/uuid.dart';

/// The administrator uses the same Nostos client and cloud database as buyers.
class AdminScreen extends StatelessWidget {
  const AdminScreen({super.key, required this.adapter});

  final NostosAdapter? adapter;

  @override
  Widget build(BuildContext context) {
    final adapter = this.adapter;
    if (adapter == null) {
      return const Center(child: CircularProgressIndicator());
    }
    return DefaultTabController(
      length: 3,
      child: Scaffold(
        backgroundColor: AtletTokens.bone,
        appBar: AppBar(
          title: const Text('Atlet admin'),
          bottom: const TabBar(
            tabs: [
              Tab(text: 'Catalog'),
              Tab(text: 'Orders'),
              Tab(text: 'Users'),
            ],
          ),
        ),
        body: TabBarView(
          children: [
            _Catalog(adapter: adapter),
            _Orders(adapter: adapter),
            _Users(adapter: adapter),
          ],
        ),
      ),
    );
  }
}

class _Catalog extends StatelessWidget {
  const _Catalog({required this.adapter});

  final NostosAdapter adapter;

  @override
  Widget build(BuildContext context) => StreamBuilder<List<ProductRow>>(
    stream: adapter.watchProducts(),
    builder: (context, snapshot) {
      if (!snapshot.hasData) {
        return const Center(child: CircularProgressIndicator());
      }
      final products = snapshot.data!;
      return Column(
        children: [
          Padding(
            padding: const EdgeInsets.all(16),
            child: FilledButton.icon(
              key: const Key('admin-add-product'),
              onPressed: () => _edit(context),
              icon: const Icon(Icons.add),
              label: const Text('Add product'),
            ),
          ),
          Expanded(
            child: products.isEmpty
                ? const Center(child: Text('No products yet'))
                : ListView.builder(
                    key: const Key('admin-product-list'),
                    itemCount: products.length,
                    itemBuilder: (context, index) {
                      final product = products[index];
                      return ListTile(
                        title: Text(product.name),
                        subtitle: Text(
                          '${product.category} · \$${(product.priceCents / 100).toStringAsFixed(2)}',
                        ),
                        trailing: IconButton(
                          tooltip: 'Edit ${product.name}',
                          icon: const Icon(Icons.edit_outlined),
                          onPressed: () => _edit(context, product),
                        ),
                      );
                    },
                  ),
          ),
        ],
      );
    },
  );

  Future<void> _edit(BuildContext context, [ProductRow? product]) async {
    var name = product?.name ?? '';
    var category = product?.category ?? 'Equipment';
    var price = product == null
        ? ''
        : (product.priceCents / 100).toStringAsFixed(2);
    var plantBased = product?.plantBased ?? false;
    await showDialog<void>(
      context: context,
      builder: (dialogContext) => StatefulBuilder(
        builder: (context, setDialogState) => AlertDialog(
          scrollable: true,
          title: Text(product == null ? 'Add product' : 'Edit product'),
          content: SizedBox(
            width: 360,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                TextFormField(
                  key: const Key('admin-product-name'),
                  initialValue: name,
                  onChanged: (value) => name = value,
                  decoration: const InputDecoration(labelText: 'Name'),
                ),
                TextFormField(
                  key: const Key('admin-product-category'),
                  initialValue: category,
                  onChanged: (value) => category = value,
                  decoration: const InputDecoration(labelText: 'Category'),
                ),
                TextFormField(
                  key: const Key('admin-product-price'),
                  initialValue: price,
                  onChanged: (value) => price = value,
                  keyboardType: const TextInputType.numberWithOptions(
                    decimal: true,
                  ),
                  decoration: const InputDecoration(
                    labelText: 'Price in dollars',
                  ),
                ),
                SwitchListTile(
                  title: const Text('Plant based'),
                  value: plantBased,
                  onChanged: (value) =>
                      setDialogState(() => plantBased = value),
                ),
              ],
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.pop(dialogContext),
              child: const Text('Cancel'),
            ),
            FilledButton(
              key: const Key('admin-save-product'),
              onPressed: () async {
                final amount = double.tryParse(price);
                if (name.trim().isEmpty ||
                    category.trim().isEmpty ||
                    amount == null ||
                    !amount.isFinite ||
                    amount < 0) {
                  ScaffoldMessenger.of(dialogContext).showSnackBar(
                    const SnackBar(
                      content: Text('Enter a name, category and valid price.'),
                    ),
                  );
                  return;
                }
                final cents = (amount * 100).round();
                try {
                  await adapter.saveProduct(
                    ProductRow(
                      id: product?.id ?? uuidV4(),
                      name: name.trim(),
                      category: category.trim(),
                      priceCents: cents,
                      plantBased: plantBased,
                      rating: product?.rating,
                      imageId: product?.imageId,
                      imageUrl: product?.imageUrl,
                    ),
                  );
                  if (dialogContext.mounted) Navigator.pop(dialogContext);
                } catch (error) {
                  if (dialogContext.mounted) {
                    ScaffoldMessenger.of(dialogContext).showSnackBar(
                      SnackBar(content: Text('Product save failed: $error')),
                    );
                  }
                }
              },
              child: const Text('Save'),
            ),
          ],
        ),
      ),
    );
  }
}

class _Orders extends StatelessWidget {
  const _Orders({required this.adapter});

  final NostosAdapter adapter;

  @override
  Widget build(BuildContext context) => StreamBuilder<List<OrderRow>>(
    stream: adapter.watchOrders(),
    builder: (context, snapshot) {
      if (!snapshot.hasData) {
        return const Center(child: CircularProgressIndicator());
      }
      final orders = snapshot.data!;
      if (orders.isEmpty) return const Center(child: Text('No orders yet'));
      return ListView.builder(
        key: const Key('admin-order-list'),
        itemCount: orders.length,
        itemBuilder: (context, index) {
          final order = orders[index];
          return Card(
            margin: const EdgeInsets.fromLTRB(16, 8, 16, 0),
            child: ListTile(
              title: Text('Order ${order.id.substring(0, 8)}'),
              subtitle: Text(
                '${order.status} · ${order.userId ?? 'unknown customer'} · '
                '\$${(order.totalCents / 100).toStringAsFixed(2)}',
              ),
              trailing: switch (order.status) {
                'paid' => TextButton(
                  key: Key('admin-ship-${order.id}'),
                  onPressed: () => _advance(context, order, 'shipped'),
                  child: Semantics(
                    label: 'Ship order ${order.id}',
                    excludeSemantics: true,
                    child: const Text('Ship'),
                  ),
                ),
                'shipped' => TextButton(
                  key: Key('admin-deliver-${order.id}'),
                  onPressed: () => _advance(context, order, 'delivered'),
                  child: Semantics(
                    label: 'Deliver order ${order.id}',
                    excludeSemantics: true,
                    child: const Text('Deliver'),
                  ),
                ),
                _ => null,
              },
            ),
          );
        },
      );
    },
  );

  Future<void> _advance(
    BuildContext context,
    OrderRow order,
    String status,
  ) async {
    try {
      await adapter.setOrderStatus(order, status);
    } catch (error) {
      if (context.mounted) {
        ScaffoldMessenger.of(
          context,
        ).showSnackBar(SnackBar(content: Text('Order update failed: $error')));
      }
    }
  }
}

class _Users extends StatelessWidget {
  const _Users({required this.adapter});

  final NostosAdapter adapter;

  @override
  Widget build(BuildContext context) => StreamBuilder<List<UserProfileRow>>(
    stream: adapter.watchUserProfiles(),
    builder: (context, snapshot) {
      if (!snapshot.hasData) {
        return const Center(child: CircularProgressIndicator());
      }
      final profiles = snapshot.data!;
      if (profiles.isEmpty) {
        return const Center(child: Text('No user profiles yet'));
      }
      return ListView.builder(
        key: const Key('admin-user-list'),
        itemCount: profiles.length,
        itemBuilder: (context, index) {
          final profile = profiles[index];
          return Card(
            margin: const EdgeInsets.fromLTRB(16, 8, 16, 0),
            child: ListTile(
              title: Text(profile.displayName),
              subtitle: Text(
                '${profile.id} · ${profile.active ? 'Active' : 'Inactive'}',
              ),
              trailing: IconButton(
                key: Key('admin-edit-user-${profile.id}'),
                tooltip: 'Edit ${profile.displayName}',
                icon: const Icon(Icons.edit_outlined),
                onPressed: () => _edit(context, profile),
              ),
            ),
          );
        },
      );
    },
  );

  Future<void> _edit(BuildContext context, UserProfileRow profile) async {
    var name = profile.displayName;
    var active = profile.active;
    final isSelf = profile.id == adapter.currentUserId;
    await showDialog<void>(
      context: context,
      builder: (dialogContext) => StatefulBuilder(
        builder: (context, setDialogState) => AlertDialog(
          scrollable: true,
          title: const Text('Edit user'),
          content: SizedBox(
            width: 360,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(profile.id),
                TextFormField(
                  key: const Key('admin-user-name'),
                  initialValue: name,
                  onChanged: (value) => name = value,
                  decoration: const InputDecoration(labelText: 'Display name'),
                ),
                SwitchListTile(
                  key: const Key('admin-user-active'),
                  title: const Text('Can sync'),
                  subtitle: isSelf
                      ? const Text('You cannot disable your own access')
                      : null,
                  value: active,
                  onChanged: isSelf
                      ? null
                      : (value) => setDialogState(() => active = value),
                ),
              ],
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.of(dialogContext).pop(),
              child: const Text('Cancel'),
            ),
            FilledButton(
              key: const Key('admin-save-user'),
              onPressed: () async {
                if (name.trim().isEmpty) return;
                try {
                  await adapter.saveUserProfile(
                    UserProfileRow(
                      id: profile.id,
                      displayName: name.trim(),
                      active: active,
                    ),
                  );
                  if (dialogContext.mounted) Navigator.of(dialogContext).pop();
                } catch (error) {
                  if (context.mounted) {
                    ScaffoldMessenger.of(context).showSnackBar(
                      SnackBar(content: Text('User update failed: $error')),
                    );
                  }
                }
              },
              child: const Text('Save'),
            ),
          ],
        ),
      ),
    );
  }
}
