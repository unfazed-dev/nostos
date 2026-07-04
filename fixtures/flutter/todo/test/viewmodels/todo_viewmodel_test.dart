import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:mocktail/mocktail.dart';
import 'package:todo/domain/todo_repository.dart';
import 'package:todo/viewmodels/todo_viewmodel.dart';

class _MockTodoRepository extends Mock implements TodoRepository {}

void main() {
  late _MockTodoRepository todos;
  late StreamController<List<Todo>> stream;

  setUp(() {
    todos = _MockTodoRepository();
    stream = StreamController<List<Todo>>(sync: true);
    when(todos.watch).thenAnswer((_) => stream.stream);
  });

  tearDown(() => stream.close());

  TodoViewModel vm() => TodoViewModel(todos);

  test('initial state: empty todo list', () {
    final m = vm();
    expect(m.todos, isEmpty);
  });

  test('watch stream re-renders the list on each emission', () async {
    final m = vm();
    final first = [const Todo(id: '1', title: 'A')];

    stream.add(first);
    // microtask flush for the broadcast listener to deliver
    await Future<void>.delayed(Duration.zero);

    expect(m.todos, first);

    final second = [
      const Todo(id: '1', title: 'A'),
      const Todo(id: '2', title: 'B'),
    ];
    stream.add(second);
    await Future<void>.delayed(Duration.zero);

    expect(m.todos, second);
  });

  test('add delegates with a trimmed title', () async {
    when(() => todos.add(any())).thenAnswer((_) async {});

    final m = vm();
    await m.add('  buy milk  ');

    verify(() => todos.add('buy milk')).called(1);
  });

  test('add ignores an empty (or whitespace-only) title', () async {
    final m = vm();
    await m.add('   ');

    verifyNever(() => todos.add(any()));
  });

  test('toggle delegates by id', () async {
    when(() => todos.toggle(any())).thenAnswer((_) async {});

    final m = vm();
    await m.toggle('id-42');

    verify(() => todos.toggle('id-42')).called(1);
  });
}
