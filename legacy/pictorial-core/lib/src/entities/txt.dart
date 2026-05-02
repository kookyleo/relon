import 'package:flutter/cupertino.dart';

import '../pom/entity.dart';

class Txt extends Entity {
  final int defaultFontSize = 14;
  final String defaultFontColor = "#000000";
  final String defaultFontWeight = "normal";

  late List<TextSpan> _textSpans;

  Txt(Map<String, dynamic> bson) {
    this._textSpans = decode(bson);
  }

  @override
  void from(Entity entity) {
    throw Exception('No one can transform to Txt for now');
  }

  @override
  IconData get icon => CupertinoIcons.textformat;

  @override
  Widget render({editable = false}) {
    // TODO: implement render
    throw UnimplementedError();
  }

  @override
  decode(Map<String, dynamic> bson) {
    List<dynamic> textSpansJson = bson['textSpans'];
    this._textSpans = textSpansJson.map((spanJson) {
      return TextSpan(
        text: spanJson['text'],
        style: TextStyle(
          fontSize: spanJson['style']['fontSize'],
          color: Color(int.parse(spanJson['style']['color'].replaceFirst('#', '0xff'))),
          fontWeight: spanJson['style'].containsKey('fontWeight')
              ? FontWeight.values.firstWhere((weight) => weight.toString() == spanJson['style']['fontWeight'])
              : FontWeight.normal,
        ),
      );
    }).toList();
  }

  @override
  Map<String, dynamic> encode() {
    List<Map<String, dynamic>> spans = [];
    for (TextSpan span in this._textSpans) {
      Map<String, dynamic> spanJson = {
        "text": span.text,
        "style": {
          "fontSize": span.style?.fontSize ?? defaultFontSize,
          "color": span.style?.color?.value.toRadixString(16) ?? defaultFontColor,
          "fontWeight": span.style?.fontWeight?.toString() ?? defaultFontWeight,
        }
      };
      spans.add(spanJson);
    }
    return {"textSpans": spans};
  }
}
