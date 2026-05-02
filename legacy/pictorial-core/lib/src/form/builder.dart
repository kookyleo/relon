import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:pictorial_core/src/evaluator/table.dart';
import 'package:pictorial_core/src/form/validation.dart';

import '../../pictorial_core.dart';
import 'field.dart';
import '../evaluator/app.dart';
import '../evaluator/base/context.dart';
import '../evaluator/base/expr.dart';
import '../evaluator/base/xson.dart';
import '../evaluator/base/table.dart';

// UserInput json description,
// @see https://globalsphere.atlassian.net/wiki/x/zQBq
// example:
// {
//     "label": "Reference Picture",
//     "type": "IMAGE",
//     "validator": [
//         fileTypeIsOneOf(["png", "gif"]),
//         fileSizeBetween(1024, 1024*1024*20)
//     ]
// }
class UsrInputDesc {
  final String label;
  final String type;
  final List<FnCall> validator;

  UsrInputDesc(this.label, this.type, this.validator);

  factory UsrInputDesc.fromExpr(ComplexObject d) {
    return UsrInputDesc(
      (d['label']! as Literal).value,
      (d['type']! as Literal).value,
      (d['validator']! as ComplexArray).value.map<FnCall>((e) => e as FnCall).toList(),
    );
  }

  UsrInputProto toUsrInputProto() {
    return UsrInputProto.build(
      type: UsrInputType.values.firstWhere((e) => e.toString() == 'UsrInputType.$type'),
      key: label,
      required: true,
      label: label,
      validations: validator.map((e) => Validation.fromExpr(e)).toList(),
    );
  }
}

class Form {
  Map<FieldKey, UsrInputProto> _fields = {};
  Map<FieldKey, dynamic> _result = {};

  // the app can complete its desired form rendering based on these fields
  Map<FieldKey, UsrInputProto> get fields => _fields;

  // get the result
  Map<FieldKey, dynamic> get result => _result;

  // submit the form
  void submit(Map<FieldKey, dynamic> userInput) => _result = userInput;

  Form._(this._fields);

  // build from Table
  static Future<Form> createForm(PictorialTable table) async {
    table.optimizeAndValidityCheck();
    // (path, index of decorators, validator args)
    List<(PathString, int, List<Expr>)> formFields = table.getSpecDecorators('user_input');
    Context ctx = Context(App(), table);
    // simplify the user_input(args): make args as a list<Expr:Literal>
    for (int i = 0; i < formFields.length; i++) {
      assert(formFields[i].$3.length == 1); // the only argument of user_input is a user_input description
      assert(formFields[i].$3[0] is ComplexObject); // UserInput json description
      assert((formFields[i].$3[0] as ComplexObject)['validator'] is ComplexArray); // validator is a list of validator FnCall

      List<Expr> validators = ((formFields[i].$3[0] as ComplexObject)['validator'] as ComplexArray).value;
      for (int j = 0; j < validators.length; j++) {
        assert(validators[j] is FnCall); // validator is a list of validator FnCall
        List<Xson> vArgs = await Future.wait((validators[j] as FnCall).args.map((e) async => await e.evaluate(ctx)).toList());
        validators[j] = FnCall((validators[j] as FnCall).name, vArgs.map((e) => Expr.fromXson(e)).toList());
      }
    }
    // build the form
    Map<FieldKey, UsrInputProto> fields = Map.fromEntries(formFields.map((e) {
      return MapEntry(FieldKey(e.$1.toString(), e.$2), UsrInputDesc.fromExpr(e.$3[0] as ComplexObject).toUsrInputProto());
    }).toList());
    return Form._(fields);
  }

  @override
  String toString() {
    String out = '{"fields": [';
    for (var key in _fields.keys) {
      out += '{"$key": ${_fields[key]}}';
    }
    out += "]}";
    return out;
  }
}
