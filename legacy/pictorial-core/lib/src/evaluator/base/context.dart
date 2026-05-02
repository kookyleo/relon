import 'package:flutter/foundation.dart';
import 'package:pictorial_core/pictorial_core.dart';
import 'expr.dart';
import 'table.dart';

class PositionStack {
  List<PathString> _path = [];

  PositionStack(PathString init) {
    _path.add(init);
  }

  void push(PathString p) => _path.add(p);
  PathString pop() => _path.removeLast();
  PathString current() => _path.last;
}

class Context {
  IApplication _app;
  get app => _app;

  Table _table;
  get table => _table;

  Map<String, dynamic> _extraSpace = {};
  void set(String key, dynamic value) => _extraSpace[key] = value;
  get(String key) => _extraSpace[key];

  // where am i
  PositionStack p = PositionStack(PathString('/'));

  Context(this._app, this._table);

  @visibleForTesting
  debug({String? filter = ''}) {
    return switch (filter) {
      'table' => Log.debug('table: ${_table}'),
      'app' => Log.debug('app: ${_app}'),
      'path' => Log.debug('current p: ${p._path}'),
      'extraSpace' => Log.debug('extra space: ${_extraSpace}'),
      _ => Log.debug('current p: ${p._path}\napp: ${_app}\nextra space: ${_extraSpace}\ntable: ${_table}'),
    };
  }
}
