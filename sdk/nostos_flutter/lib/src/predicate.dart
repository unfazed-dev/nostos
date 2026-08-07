/// Structured query predicates — the typed `where`/`orderBy` the unified API
/// (ADR-0032) passes to `Collection<T>.watch` / `getAll` / `count` / `exists`.
///
/// `where` is **data, not strings**. This mirrors `nostos-domain`'s `Predicate`
/// (ADR-0012) and kills the SQL-injection foot-gun the old string-fragment
/// `where:`/`orderBy:` carried: column names are validated to a strict
/// identifier grammar and values are emitted as safe SQLite literals, so
/// nothing the caller supplies is ever spliced raw into the compiled SQL.
///
/// Operators v1 (per the ratified contract): `eq, neq, lt, lte, gt, gte,
/// inList, isNull, notNull` plus the combinators `and, or, not`. A fluent
/// per-language builder is deliberately rejected (9 codegen surfaces for zero
/// added expressiveness); `Where.eq(...)`, `Where.and([...])` is the whole API.
library;

import 'package:meta/meta.dart';

/// Validates a column / identifier against the SQLite-safe grammar.
///
/// Allows schema-qualified `table.col` (one dot, each side an ident) because
/// `cairn_data`-backed views sometimes need it. Anything outside this set
/// (`'`, `"`, `;`, whitespace, `-`, …) is rejected with [ArgumentError] — the
/// only way an attacker-controlled string reaches `toSql()` is through a column
/// name, so this is the injection boundary.
void _checkIdent(String name, {String what = 'column'}) {
  final ok = RegExp(r'^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)?$')
      .hasMatch(name);
  if (!ok) {
    throw ArgumentError.value(
      name,
      what,
      'must be a bare identifier matching [A-Za-z_][A-Za-z0-9_]* '
      '(optionally one dot-separated pair); got',
    );
  }
}

/// Renders a Dart value as a safe SQLite literal. The query path is
/// parameter-less today (the engine `query` FFI takes one SQL string), so
/// values are inlined as literals rather than bound. This is the second half
/// of the injection boundary — every value goes through here.
String _literal(Object v) {
  if (v is int || v is double) return v.toString();
  if (v is bool) return v ? '1' : '0'; // JSON1 + the WS2 views store bools as 0/1.
  if (v is String) return "'${v.replaceAll("'", "''")}'";
  throw ArgumentError.value(
    v.runtimeType,
    'value',
    'Where value must be an int, double, bool, or String (got)',
  );
}

/// A structured boolean tree compiled to a SQL `WHERE` fragment.
///
/// Build leaves with the static constructors ([Where.eq], [Where.gt], …) and
/// combine with [Where.and], [Where.or], [Where.not]. Never subclass this
/// yourself — the v1 operator set is closed; a new operator is a contract
/// change (ADR-0032), not an app-level extension point.
@immutable
sealed class Where {
  const Where();

  // ── Comparison leaves ──────────────────────────────────────────────────
  static Where eq(String column, Object value) => _Compare(column, '=', value);
  static Where neq(String column, Object value) => _Compare(column, '!=', value);
  static Where lt(String column, Object value) => _Compare(column, '<', value);
  static Where lte(String column, Object value) => _Compare(column, '<=', value);
  static Where gt(String column, Object value) => _Compare(column, '>', value);
  static Where gte(String column, Object value) => _Compare(column, '>=', value);

  /// `column IN (v1, v2, …)`. An empty [values] is rejected — `IN ()` is not
  /// valid SQLite, and a caller reaching for it almost certainly meant
  /// `Where.eq` against a pre-checked scalar or an always-false guard they
  /// should make explicit.
  static Where inList(String column, List<Object> values) {
    if (values.isEmpty) {
      throw ArgumentError.value(
        values,
        'values',
        'Where.inList requires a non-empty list; an empty IN () is invalid SQL',
      );
    }
    return _InList(column, List<Object>.unmodifiable(values));
  }

  static Where isNull(String column) => _NullCheck(column, 'IS NULL');
  static Where notNull(String column) => _NullCheck(column, 'IS NOT NULL');

  // ── Combinators ────────────────────────────────────────────────────────
  /// AND of [parts]. An empty list is rejected — write `Where.eq('1', 1)` for
  /// an always-true predicate, or simply omit `where:`.
  static Where and(List<Where> parts) {
    if (parts.isEmpty) {
      throw ArgumentError.value(
        parts,
        'parts',
        'Where.and requires a non-empty list',
      );
    }
    return _Junction('AND', List<Where>.unmodifiable(parts));
  }

  /// OR of [parts]. An empty list is rejected.
  static Where or(List<Where> parts) {
    if (parts.isEmpty) {
      throw ArgumentError.value(
        parts,
        'parts',
        'Where.or requires a non-empty list',
      );
    }
    return _Junction('OR', List<Where>.unmodifiable(parts));
  }

  /// Logical NOT of [inner]. Parenthesized so it composes without surprise.
  static Where not(Where inner) => _Not(inner);

  /// Compile to a SQL `WHERE`-clause fragment (no leading `WHERE` keyword).
  /// Implementations must keep this side-effect-free and total over the
  /// constructor-validated state.
  String toSql();
}

class _Compare extends Where {
  const _Compare(this.column, this.op, this.value);
  final String column;
  final String op;
  final Object value;

  @override
  String toSql() {
    _checkIdent(column);
    return '$column $op ${_literal(value)}';
  }
}

class _InList extends Where {
  const _InList(this.column, this.values);
  final String column;
  final List<Object> values;

  @override
  String toSql() {
    _checkIdent(column);
    final rendered = values.map(_literal).join(', ');
    return '$column IN ($rendered)';
  }
}

class _NullCheck extends Where {
  const _NullCheck(this.column, this.suffix);
  final String column;
  final String suffix;

  @override
  String toSql() {
    _checkIdent(column);
    return '$column $suffix';
  }
}

class _Junction extends Where {
  const _Junction(this.op, this.parts);
  final String op; // 'AND' | 'OR'
  final List<Where> parts;

  @override
  String toSql() {
    final rendered = parts.map((p) {
      final s = p.toSql();
      // Wrap combinator children so AND/OR precedence can't surprise anyone:
      // `a AND (b OR c)` must not flatten to `a AND b OR c`.
      return (p is _Junction || p is _Not) ? '($s)' : s;
    }).join(' $op ');
    return '($rendered)';
  }
}

class _Not extends Where {
  const _Not(this.inner);
  final Where inner;

  @override
  String toSql() {
    final s = inner.toSql();
    return '(NOT $s)';
  }
}

/// One `ORDER BY` term: a column + direction. Pass a list to
/// `Collection.watch(orderBy: ...)`; first entry sorts first.
@immutable
class Order {
  const Order._(this.field, this.descending);
  final String field;
  final bool descending;

  /// Ascending by [field] (SQL `ASC`, the default).
  factory Order.asc(String field) => Order._(field, false);

  /// Descending by [field] (SQL `DESC`).
  factory Order.desc(String field) => Order._(field, true);

  String toSql() {
    _checkIdent(field, what: 'field');
    return descending ? '$field DESC' : '$field ASC';
  }
}
