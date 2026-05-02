import 'package:flutter/cupertino.dart';

import '../pom/entity.dart';

// Shape uses Svg technology to encode and render
// https://pub.dev/packages/flutter_svg

class Shape extends Entity {
  late String _svg;
  late String _description;

  @override
  decode(Map<String, dynamic> bson) {
    _svg = bson['svg'];
    _description = bson['description'];
  }

  @override
  Map<String, dynamic> encode() {
    return {
      'svg': _svg,
      'description': _description,
    };
  }

  @override
  void from(Entity entity) {
    throw Exception('No one can transform to Shape for now');
  }

  @override
  IconData get icon => CupertinoIcons.square_on_circle;

  @override
  Widget render({editable = false}) {
    // TODO: implement render
    throw UnimplementedError();
    // SvgPicture.string(_svg);
  }
}
