import 'package:pictorial_core/pictorial_core.dart';

import 'table.dart';

abstract class IApplication {
  // Query values by certain keys
  Future<Xson> query(List<String> keys);

  // Apply a method with certain value to be decorated, the name and arguments of method, and context
  // Normally, it's used to apply a decorator to a value
  Future<Xson> apply(Xson value, String name, List<Xson> args, Context context);

  // Call a function with certain name and arguments, and context
  Future<Xson> call(String name, List<Expr> args, Context context);

  // infix operator expr
  Xson infixOpExpr(Xson lhs, String op, Xson rhs);

  // prefix operator expr
  Xson prefixOpExpr(String op, Xson rhs);

  // postfix operator expr
  Xson postfixOpExpr(Xson lhs, String op);

  // ternary operator expr
  Xson ternaryOpExpr(Xson cond, Xson exprT, Xson exprF);
}

abstract class Expr {
  factory Expr.fromXson(Xson x) {
    switch (x) {
      case XsonObject():
        if (x.isEsonObject()) return Expr.fromXsonObject(x);
        return ComplexObject.fromXson(x);
      case XsonArray():
        return ComplexArray.fromXson(x);
      case XsonStr():
      case XsonNumber():
      case XsonBool():
      case XsonNul():
      case XsonBytes():
      case XsonUint8List():
        return Literal(x);
      default:
        throw Exception("Unexpected type: ${x.runtimeType}");
    }
  }

  factory Expr.fromXsonObject(XsonObject o) {
    return switch ((o[r'$schema']! as XsonStr).value) {
      'DecoratedVal' => DecoratedVal.fromXsonObject(o),
      'PrefixOpExpr' => PrefixOpExpr.fromXsonObject(o),
      'InfixOpExpr' => InfixOpExpr.fromXsonObject(o),
      'PostfixOpExpr' => PostfixOpExpr.fromXsonObject(o),
      'TernaryOpExpr' => TernaryOpExpr.fromXsonObject(o),
      'FnCall' => FnCall.fromXsonObject(o),
      'RefSibling' => RefSibling.fromXsonObject(o),
      'RefUncle' => RefUncle.fromXsonObject(o),
      'RefRoot' => RefRoot.fromXsonObject(o),
      'Var' => Var.fromXsonObject(o),
      _ => throw Exception("Unexpected schema: ${(o[r'$schema']! as XsonStr).value}")
    };
  }

  String toString();

  toJson();

  Future<Xson> evaluate(Context context);
}

class Var implements Expr {
  late final List<String> _keys;

  Var(this._keys);

  Var.fromXsonObject(XsonObject xson) {
    // XsonArray(List<Xson>) -> List<String>
    _keys = ((xson['keys'] as XsonArray).value as List<Xson>).map((e) => (e as XsonStr).value).toList();
  }

  @override
  Future<Xson> evaluate(Context context) async {
    return await (context.app as IApplication).query(_keys);
  }

  @override
  String toString() => 'Var(${_keys.join(".")})';

  @override
  toJson() {
    return {
      r'$schema': 'Var',
      'keys': _keys,
    };
  }
}

class FnCall implements Expr {
  late final String _name;
  late final List<Expr> _args;

  String get name => _name;
  List<Expr> get args => _args;

  FnCall(this._name, this._args);

  FnCall.fromXsonObject(XsonObject xson) {
    _name = xson['name']?.value;
    _args = (xson['args'] as XsonArray).value.map<Expr>((e) => Expr.fromXson(e)).toList();
  }

  @override
  Future<Xson> evaluate(Context context) async {
    // == Move the evaluation of the arguments to the application layer ==
    // return await (context.app as IApplication)
    //     .call(_name, await Future.wait(_args.map((e) => e.evaluate(context)).toList()), context);
    return await (context.app as IApplication).call(_name, _args, context);
  }

  @override
  String toString() {
    return 'FnCall($_name, [${_args.join(", ")}])';
  }

  @override
  toJson() {
    return {
      r'$schema': 'FnCall',
      'name': _name,
      'args': _args.map((e) => e.toJson()).toList(),
    };
  }
}

class RefRoot implements Expr {
  late final List<String> _keys;

  RefRoot(this._keys);

  RefRoot.fromXsonObject(XsonObject xson) {
    assert(xson[r'$schema'] == XsonStr('RefRoot'));
    _keys = ((xson['keys'] as XsonArray).value as List<Xson>).map((e) => (e as XsonStr).value).toList();
  }

  @override
  Future<Xson> evaluate(Context context) async {
    PathString p = PathString("/").merge(PathString.fromList(_keys));
    Row? i = (context.table as Table).fetch(p);
    if (i == null) return XsonNul();
    context.p.push(p);
    Xson r = switch (i.valueState) {
      ValueState.Pending => await i.evaluate(context),
      ValueState.Evaluated || ValueState.Literal => i.value,
    };
    context.p.pop();
    return r;
  }

  @override
  String toString() {
    return 'RefRoot(${_keys.join(".")})';
  }

  @override
  toJson() {
    return {
      r'$schema': 'RefRoot',
      'keys': _keys,
    };
  }
}

class RefUncle implements Expr {
  late final List<String> _keys;

  RefUncle(this._keys);

  RefUncle.fromXsonObject(XsonObject xson) {
    assert(xson[r'$schema'] == XsonStr('RefUncle'));
    _keys = ((xson['keys'] as XsonArray).value as List<Xson>).map((e) => (e as XsonStr).value).toList();
  }

  @override
  Future<Xson> evaluate(Context context) async {
    PathString p = context.p.current().merge(PathString.fromList(['..', '..', ..._keys]));
    Row? i = (context.table as Table).fetch(p);
    if (i == null) return XsonNul();
    context.p.push(p);
    Xson r = (i.valueState != ValueState.Pending) ? i.value : await i.evaluate(context);
    context.p.pop();
    return r;
  }

  @override
  String toString() {
    return 'RefUncle(${_keys.join(".")})';
  }

  @override
  toJson() {
    return {
      r'$schema': 'RefUncle',
      'keys': _keys,
    };
  }
}

class RefSibling implements Expr {
  late final List<String> _keys;

  RefSibling(this._keys);

  RefSibling.fromXsonObject(XsonObject xson) {
    assert(xson[r'$schema'] == XsonStr('RefSibling'));
    _keys = ((xson['keys'] as XsonArray).value as List<Xson>).map((e) => (e as XsonStr).value).toList();
  }

  @override
  Future<Xson> evaluate(Context context) async {
    PathString p = context.p.current().merge(PathString.fromList(['..', ..._keys]));
    Row? i = (context.table as Table).fetch(p);
    if (i == null) return XsonNul();
    context.p.push(p);
    Xson r = (i.valueState != ValueState.Pending) ? i.value : await i.evaluate(context);
    context.p.pop();
    return r;
  }

  @override
  String toString() {
    return 'RefSibling(${_keys.join(".")})';
  }

  @override
  toJson() {
    return {
      r'$schema': 'RefSibling',
      'keys': _keys,
    };
  }
}

class DecoratedVal implements Expr {
  late final String _name;
  late final List<Expr> _args;
  late final Expr _value;

  String get name => _name;

  List<Expr> get args => _args;

  DecoratedVal(this._name, this._args, this._value);

  DecoratedVal.fromXsonObject(XsonObject xson) {
    _name = xson['name']?.value;
    _args = (xson['args'] as XsonArray).value.map<Expr>((e) => Expr.fromXson(e)).toList();
    _value = Expr.fromXson(xson['decorated_val'] as Xson);
  }

  @override
  Future<Xson> evaluate(Context context) async {
    return await (context.app as IApplication).apply(
      await _value.evaluate(context),
      _name,
      await Future.wait(_args.map((e) => e.evaluate(context)).toList()),
      context,
    );
  }

  @override
  String toString() {
    return 'DecoratedVal($_name, [${_args.join(", ")}], $_value)';
  }

  @override
  toJson() {
    return {
      r'$schema': 'DecoratedVal',
      'name': _name,
      'args': _args.map((e) => e.toJson()).toList(),
      'decorated_val': _value.toJson(),
    };
  }
}

class PrefixOpExpr implements Expr {
  late final String _op;
  late final Expr _rhs;

  PrefixOpExpr(this._op, this._rhs);

  PrefixOpExpr.fromXsonObject(XsonObject xson) {
    _op = xson['op']?.value;
    _rhs = Expr.fromXson(xson['expr_r'] as Xson);
  }

  @override
  Future<Xson> evaluate(Context context) async {
    return (context.app as IApplication).prefixOpExpr(_op, await _rhs.evaluate(context));
  }

  @override
  String toString() {
    return 'PrefixOpExpr($_op, $_rhs)';
  }

  @override
  toJson() {
    return {
      r'$schema': 'PrefixOpExpr',
      'op': _op,
      'expr_r': _rhs.toJson(),
    };
  }
}

class InfixOpExpr implements Expr {
  late final Expr _lhs;
  late final String _op;
  late final Expr _rhs;

  InfixOpExpr(this._lhs, this._op, this._rhs);

  InfixOpExpr.fromXsonObject(XsonObject xson) {
    _lhs = Expr.fromXson(xson['expr_l'] as Xson);
    _op = xson['op']?.value;
    _rhs = Expr.fromXson(xson['expr_r'] as Xson);
  }

  @override
  Future<Xson> evaluate(Context context) async {
    return (context.app as IApplication).infixOpExpr(
      await _lhs.evaluate(context),
      _op,
      await _rhs.evaluate(context),
    );
  }

  @override
  String toString() {
    return 'InfixOpExpr($_lhs, $_op, $_rhs)';
  }

  @override
  toJson() {
    return {
      r'$schema': 'InfixOpExpr',
      'expr_l': _lhs.toJson(),
      'op': _op,
      'expr_r': _rhs.toJson(),
    };
  }
}

class PostfixOpExpr implements Expr {
  late final Expr _lhs;
  late final String _op;

  PostfixOpExpr(this._lhs, this._op);

  PostfixOpExpr.fromXsonObject(XsonObject xson) {
    _lhs = Expr.fromXson(xson['expr_l'] as Xson);
    _op = xson['op']?.value;
  }

  @override
  Future<Xson> evaluate(Context context) async {
    return (context.app as IApplication).postfixOpExpr(await _lhs.evaluate(context), _op);
  }

  @override
  String toString() {
    return 'PostfixOpExpr($_lhs, $_op)';
  }

  @override
  toJson() {
    return {
      r'$schema': 'PostfixOpExpr',
      'expr_l': _lhs.toJson(),
      'op': _op,
    };
  }
}

class TernaryOpExpr implements Expr {
  late final Expr _cond;
  late final Expr _exprT;
  late final Expr _exprF;

  TernaryOpExpr(this._cond, this._exprT, this._exprF);

  TernaryOpExpr.fromXsonObject(XsonObject xson) {
    _cond = Expr.fromXson(xson['cond'] as Xson);
    _exprF = Expr.fromXson(xson['expr_f'] as Xson);
    _exprT = Expr.fromXson(xson['expr_t'] as Xson);
  }

  @override
  Future<Xson> evaluate(Context context) async {
    return (context.app as IApplication)
        .ternaryOpExpr(await _cond.evaluate(context), await _exprT.evaluate(context), await _exprF.evaluate(context));
  }

  @override
  String toString() {
    return 'TernaryOpExpr($_cond, $_exprT, $_exprF)';
  }

  @override
  toJson() {
    return {
      r'$schema': 'TernaryOpExpr',
      'cond': _cond.toJson(),
      'expr_t': _exprT.toJson(),
      'expr_f': _exprF.toJson(),
    };
  }
}

class Literal implements Expr {
  late final Xson _value;
  dynamic get value => _value.value;

  Literal(this._value);

  @override
  Future<Xson> evaluate(Context context) async => _value;

  @override
  String toString() {
    if (_value is XsonStr) {
      return 'Literal(${_value.value.replaceAll('"', '\"')})';
    } else {
      return 'Literal($_value)';
    }
  }

  @override
  bool operator ==(Object other) {
    if (identical(this, other)) return true;
    if (other is Literal && value == other.value) return true;
    return this.value == other;
  }

  @override
  int get hashCode => this.value.hashCode;

  @override
  toJson() {
    return {
      r'$schema': 'Literal',
      'value': _value.toJson(),
    };
  }
}

/// A complex object, such as a field in a Json that wraps a Map<String, Expr> Value,
/// In a sense, it is a structure that postpones the calculation process.
class ComplexObject implements Expr {
  late final Map<String, Expr> value;

  Expr? operator [](String k) => value[k];

  ComplexObject(this.value);

  factory ComplexObject.fromXson(XsonObject xson) {
    // xson => Map<String, Expr>
    return ComplexObject((xson.value as Map<String, Xson>).map((k, e) => MapEntry(k, Expr.fromXson(e))));
  }

  @override
  String toString() => 'ComplexObject($value)';

  @override
  Future<Xson> evaluate(Context context) async {
    Map<String, Xson> result = Map.fromEntries(
        await Future.wait(value.entries.map((e) async => MapEntry(e.key, await e.value.evaluate(context))).toList()));
    return Xson.object(result);
  }

  @override
  toJson() {
    return value.map((k, v) => MapEntry(k, v.toJson()));
  }
}

/// A complex array, such as a field in a Json that wraps a List<Expr> Value,
/// In a sense, it is a structure that postpones the calculation process.
class ComplexArray implements Expr {
  late final List<Expr> value;

  ComplexArray(this.value);

  factory ComplexArray.fromXson(XsonArray xson) {
    // xson => List<Json>
    return ComplexArray((xson.value as List<Xson>).map((e) => Expr.fromXson(e)).toList());
  }

  @override
  String toString() => 'ComplexArray($value)';

  @override
  Future<Xson> evaluate(Context context) {
    return Future.wait(value.map((e) => e.evaluate(context)).toList()).then((value) => Xson.array(value));
  }

  @override
  toJson() {
    return value.map((e) => e.toJson()).toList();
  }
}
