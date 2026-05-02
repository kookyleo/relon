import 'dart:convert';

import '../../pictorial_core.dart';
import '../eson/system.dart';
import '../eson/user.dart';
import 'base/expr.dart';

class App implements IApplication {
  Future<Xson> _usrInput(List<Expr> args, Context context) async {
    assert(args.length == 2); // the 1st argument is the original value of the field, the 2rd one is the user_input description
    assert(args[1] is ComplexObject);

    // at present.. the user form data is in the context:extraSpace, which is a Map<FieldKey, dynamic>,
    // the args is a list of Expr, which is the description of the user form,
    // but it's need not to be evaluated here, it should be evaluated in the form.submit()
    // so we just return the user form value here, the key is stored in the context.p.current()
    return Xson.fromDynamic(context.get(context.p.current() as String)!);
  }

  @override
  Future<Xson> call(String name, List<Expr> args, Context context) async {
    switch (name) {
      case "user_input":
        return _usrInput(args, context);
      case "trim":
        return XsonStr(((await args[0].evaluate(context)).value as String).trim());
      case "value":
        var result = await args[1].evaluate(context);
        if (result is XsonNul) {
          result = await args[0].evaluate(context);
        }
        return result;
      case "img2b64":
        return XsonStr(base64Encode((await args[0].evaluate(context)).value as List<int>));
      case "random_seed":
        return XsonNumber(randomSeed((await args[1].evaluate(context)).value, (await args[2].evaluate(context)).value));
      case "get_steps":
        return XsonNumber(getSteps());
      case "get_cfg":
        return XsonNumber(getCfg());
      case "get_sampler_name":
        return XsonStr(getSamplerName());
      case "get_lora_name":
        return XsonStr(getLoraName((await args[1].evaluate(context)).value));
      case "prompt_factory":
        return XsonStr(promptFactory(
            prompt: ((await args[1].evaluate(context)).value),
            style: ((await args[2].evaluate(context)).value),
            taskType: ((await args[3].evaluate(context)).value),
            rolePrompt: ((await args[4].evaluate(context)).value)));
      case "negative_prompt_factory":
        return XsonStr(await negativePromptFactory(((await args[1].evaluate(context)).value)));
      case "output_node":
        return XsonObject((await args[0].evaluate(context)).value);
      case "text_human_exists":
        return XsonStr(textHumanExists((await args[1].evaluate(context)).value));
      case "split_text_to_json":
        return XsonStr(splitTextToJson((await args[1].evaluate(context)).value, (await args[2].evaluate(context)).value,
            (await args[3].evaluate(context)).value));
      default:
        throw Exception("Unknown function: $name");
    }
  }

  @override
  Future<Xson> query(List<String> keys) {
    // TODO : implement query
    throw UnimplementedError();
  }

  @override
  Future<Xson> apply(Xson value, String name, List args, Context context) {
    // TODO: implement apply
    throw UnimplementedError();
  }

  @override
  infixOpExpr(Xson lhs, String op, Xson rhs) {
    if (lhs is XsonNumber && rhs is XsonNumber) {
      return switch (op) {
        "*" => XsonNumber(lhs.value * rhs.value),
        "/" => XsonNumber(lhs.value / rhs.value),
        "+" => XsonNumber(lhs.value + rhs.value),
        "-" => XsonNumber(lhs.value - rhs.value),
        "==" => XsonBool(lhs.value == rhs.value),
        "!=" => XsonBool(lhs.value != rhs.value),
        ">" => XsonBool(lhs.value > rhs.value),
        "<" => XsonBool(lhs.value < rhs.value),
        ">=" => XsonBool(lhs.value >= rhs.value),
        "<=" => XsonBool(lhs.value <= rhs.value),
        "&&" => XsonBool(lhs.value != 0 && rhs.value != 0),
        "||" => XsonBool(lhs.value != 0 || rhs.value != 0),
        _ => throw Exception("Undefined operator: $op"),
      };
    }

    // TODO: implement infixOpExpr
    throw UnimplementedError();
  }

  @override
  postfixOpExpr(lhs, String op) {
    // TODO: implement postfixOpExpr
    throw UnimplementedError();
  }

  @override
  prefixOpExpr(String op, rhs) {
    // TODO: implement prefixOpExpr
    throw UnimplementedError();
  }

  @override
  ternaryOpExpr(cond, exprT, exprF) {
    // TODO: implement ternaryOpExpr
    throw UnimplementedError();
  }
}
