// Json(with eson $schema) => Table
// Table is helper class for Evaluation

import 'dart:convert';

import 'package:pictorial_core/pictorial_core.dart';
import 'expr.dart';
import 'context.dart';
import 'xson.dart';

enum ValueState { Evaluated, Pending, Literal }

// Decoration = [@Decorator(), @Decorator(..), ...] => Value
class Decoration {
  // List<decorator name, [decorator args]>
  late List<(String, List<Expr>)> _decorators = [];

  // late List<(String, List<Json>)> _decorators = [];
  late Xson val;

  void _push(String name, List<Xson> args) {
    List<Expr> exprLst = [];
    for (var arg in args) {
      exprLst.add(Expr.fromXson(arg));
    }
    _decorators.add((name, exprLst));
  }

  Xson _parse(XsonObject v) {
    if (v.isEsonObject('DecoratedVal')) {
      _push(v['name']?.value, v['args']?.value);
      if (v['decorated_val'] is XsonObject) {
        return _parse(v['decorated_val'] as XsonObject);
      }
      return v['decorated_val'] as Xson;
    }
    return v;
  }

  // @value(1) @value(2) k: v; => Decoration([{}, {}], v)
  Decoration.fromEsonObject(XsonObject eson) {
    assert(eson.isEsonObject());
    val = _parse(eson);
  }

  List<(String, List<Expr>)> get decorators => _decorators;
}

// the key of the form field
class FieldKey {
  final String tableK;
  final int _idx;
  FieldKey(this.tableK, this._idx);

  @override
  bool operator ==(Object other) {
    return identical(this, other) || other is FieldKey && tableK == other.tableK && hashCode == other.hashCode;
  }

  @override
  int get hashCode => _idx.hashCode + tableK.hashCode;

  @override
  String toString() => '$tableK:$_idx';
}

class Row {
  // (path, jsonRefValue, decoration, valueState),
  PathString path;
  Xson value;
  List<(String, List<Expr>)> decorators;
  ValueState _valueState;

  ValueState get valueState => _valueState;

  Row(this.path, this.value, this.decorators, this._valueState);

  // evaluate the jsonValue with decorators, update the value and state back, then return the value
  Future<Xson> evaluate(Context context) async {
    Xson v = value;
    // only evaluate the value when it's pending
    if (_valueState == ValueState.Pending) {
      for (var j = decorators.length - 1; j >= 0; j--) {
        String name = decorators[j].$1;

        // == Move the evaluation of the arguments to the application layer ==
        // List<Json> args = await Future.wait(decorators[j].$2.map((e) async => await e.evaluate(context)).toList());
        // v = await (context.app as IApplication).call(name, [v, ...args], context);

        context.p.push(PathString(FieldKey(path.toString(), j).toString()));
        v = await (context.app as IApplication).call(name, [Literal(v), ...decorators[j].$2], context);
        context.p.pop();
      }
      // update the value and state
      value = v;
      _valueState = ValueState.Evaluated;
    }
    return v;
  }

  Map<String, dynamic> toJson() {
    return {
      "path": path.toString(),
      "val": value.toJson(),
      "decorators": decorators.map((e) => [e.$1, e.$2.map((e) => e.toJson()).toList()]).toList(),
      "state": _valueState.toString().split('.').last
    };
  }

  @override
  String toString() {
    String ds = "[" + decorators.map((e) => '["${e.$1}", [${e.$2.map((e) => '"$e"').join(", ")}]]').join(", ") + "]";
    return '{"path": "$path", "val": $value, "decorators": $ds, "state": "${_valueState.toString().split('.').last}"}';
  }
}

class Table {
  Map<PathString, Row> _items = {};

  Row? fetch(PathString path) => _items[path];

  get items => _items;

  Table(String jsonString) {
    Xson xson = Xson.fromJsonStr(jsonString);

    // Recursively traverse the Json object, generate a map<PathString path, Json Point> table by the way.
    traverse(Xson json, PathString layer) {
      if (json is XsonObject) {
        debug('traversing JsonObject $layer: $json');
        if (json.isEsonObject("DecoratedVal")) {
          Decoration decoration = Decoration.fromEsonObject(json);
          // push(layer, decoration.val, decoration.decorators, ValueState.PENDING);
          _items.putIfAbsent(layer, () => Row(layer, decoration.val, decoration.decorators, ValueState.Pending));
        } else {
          // push(layer, json, [], ValueState.LITERAL);
          _items.putIfAbsent(layer, () => Row(layer, json, [], ValueState.Literal));
          json.value.forEach((k, v) => traverse(v, layer.merge(PathString(k))));
        }
      } else if (json is XsonArray) {
        debug('traversing JsonArray $layer: $json');
        warn('eson annotation is also supported in List elements, but here not implemented yet'); // todo
        int i = 0;
        for (var element in json.value) {
          i += 1;
          PathString p = layer.merge(PathString(i.toString()));
          // push(p, element, [], ValueState.LITERAL);
          _items.putIfAbsent(p, () => Row(p, element, [], ValueState.Literal));
          traverse(element, p);
        }
      } else {
        debug('traversing else $layer: $json');
        // push(layer, json, [], ValueState.LITERAL);
        _items.putIfAbsent(layer, () => Row(layer, json, [], ValueState.Literal));
      }
    }

    traverse(xson, PathString('/'));
  }

  @override
  String toString() {
    String out = "{";
    out += _items.entries.map((e) => '"${e.key}": ${e.value}').join(", ");
    out += "}";
    return out;
  }
}
