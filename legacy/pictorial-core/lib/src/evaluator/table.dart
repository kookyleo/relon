import 'package:pictorial_core/pictorial_core.dart';
import 'package:pictorial_core/src/evaluator/base/context.dart';
import 'package:pictorial_core/src/form/builder.dart';
import 'app.dart';
import 'base/expr.dart';
import 'base/table.dart';

const List<String> _LegalDecorators = ['value', 'user_input', 'trim', 'output_node', 'random_seed','get_steps','get_cfg',
  'get_sampler_name','prompt_factory','negative_prompt_factory','get_lora_name','text_human_exists','split_text_to_json'];

class PictorialTable extends Table {
  PictorialTable(String jsonString) : super(jsonString);

  void optimizeAndValidityCheck() {
    for (var item in (items as Map<PathString, Row>).values) {
      List<(String, List<Expr>)> decorators = [];
      for (var i = 0; i < item.decorators.length; i++) {
        // validity check: if find unstated decorator, throw error
        if (!_LegalDecorators.contains(item.decorators[i].$1)) {
          throw Exception('Unknown or invalid decorator: ${item.decorators[i].$1}');
        }
        decorators.add(item.decorators[i]);
        // optimize: if find decorator:value, others make no sense.
        // `user_input` has the similar effect to `value`
        if (item.decorators[i].$1 == 'value' || item.decorators[i].$1 == 'user_input') {
          item.decorators = decorators;
          break;
        }
      }
    }
  }

  // get all decorators in the table with the same name
  // return a list of (path, index of decorators)
  // eg. getSpecDecorators('user_input')
  // => [(//path/to/k, idx, [arg0, arg1]), ..]
  List<(PathString, int, List<Expr>)> getSpecDecorators(String name) {
    List<(PathString, int, List<Expr>)> specDecorators = [];
    for (var item in (items as Map<PathString, Row>).entries) {
      for (var i = 0; i < item.value.decorators.length; i++) {
        if (item.value.decorators[i].$1 == name) specDecorators.add((item.key, i, item.value.decorators[i].$2));
      }
    }
    return specDecorators;
  }

  // evaluate the table with the user form
  Future<void> evaluate(Map<FieldKey, dynamic> userInput, IApplication app) async {
    Context ctx = Context(app, this);
    userInput.forEach((key, value) => ctx.set(key.toString(), value));
    for (var row in (items as Map<PathString, Row>).values) {
      await row.evaluate(ctx);
    }
  }

  // standard workflow
  // 1. parse json string to table object ✅
  // 2. optimize and validity check ✅
  // 3. get all user_input decorators, ✅ then generate a form and show it to the user
  // 4. wait for the user to fill out and submit the form
  // 5. evaluate the table with the form
  // 6. do something with the result


}
