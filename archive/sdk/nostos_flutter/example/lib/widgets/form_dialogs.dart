// Reusable form dialog scaffolding for create/edit flows.
//
// The booking app has several create/edit dialogs (provider, client,
// appointment, invoice, message). They share the same AlertDialog shell +
// Save/Cancel actions, so this file factors that out. Each dialog builds its
// own field list and returns a typed result on Save.

import 'package:flutter/material.dart';

/// A labeled text field inside a form dialog.
class DialogField {
  const DialogField({
    required this.key,
    required this.label,
    this.initial,
    this.keyboardType,
    this.maxLines = 1,
  });
  final String key;
  final String label;
  final String? initial;
  final TextInputType? keyboardType;
  final int maxLines;
}

/// Show a generic text-form dialog. Returns a map of key→entered-string
/// (only non-empty fields), or null on cancel.
Future<Map<String, String>?> showFormDialog(
  BuildContext context, {
  required String title,
  required List<DialogField> fields,
  String saveLabel = 'Save',
}) async {
  final controllers = {
    for (final f in fields)
      f.key: TextEditingController(text: f.initial ?? ''),
  };
  final result = await showDialog<Map<String, String>>(
    context: context,
    builder: (ctx) => AlertDialog(
      title: Text(title),
      content: SingleChildScrollView(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            for (final f in fields) ...[
              TextField(
                controller: controllers[f.key],
                decoration: InputDecoration(labelText: f.label),
                keyboardType: f.keyboardType,
                maxLines: f.maxLines,
              ),
              const SizedBox(height: 4),
            ],
          ],
        ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.pop(ctx, null),
          child: const Text('Cancel'),
        ),
        FilledButton(
          onPressed: () {
            final m = <String, String>{};
            for (final f in fields) {
              final v = controllers[f.key]!.text.trim();
              if (v.isNotEmpty) m[f.key] = v;
            }
            Navigator.pop(ctx, m);
          },
          child: Text(saveLabel),
        ),
      ],
    ),
  );
  for (final c in controllers.values) {
    c.dispose();
  }
  return result;
}

/// A loading spinner dialog (used while fetching provider/client lists for
/// dropdowns). Dismiss by popping with the result.
Widget loadingDialog() => const AlertDialog(
      content: SizedBox(
        height: 48,
        width: 48,
        child: Center(child: CircularProgressIndicator()),
      ),
    );
