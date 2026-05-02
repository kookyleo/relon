import 'dart:typed_data';

import 'package:flutter/cupertino.dart';
import 'package:pictorial_core/pictorial_core.dart';

import '../pom/entity.dart';
import 'entities.dart';

class Unspecified extends Entity {
  String? textDescription;
  Uint8List? image;

  Unspecified({this.textDescription, this.image});

  @override
  decode(Map<String, dynamic> bson) {
    textDescription = bson['textDescription'];
    image = bson['image'];
  }

  @override
  Map<String, dynamic> encode() {
    return {
      'textDescription': textDescription,
      'image': image,
    };
  }

  @override
  void from(Entity entity) {
    switch (entity.runtimeType) {
      case Scene s:
        warn('Transform Scene to Unspecified will lose information');
        textDescription = s.textDescription;
        image = s.image;
        break;
      case Person p:
        warn('Transform Person to Unspecified will lose information');
        textDescription = p.textDescription;
        image = p.referenceImage as Uint8List?;
        break;

      case Unspecified _:
      case Shape _:
      case Txt _:
      default:
        throw Exception('Cannot transform to Unspecified from ${entity.runtimeType}');
    }
  }

  @override
  IconData get icon => CupertinoIcons.question_square;

  @override
  Widget render({editable = false}) {
    return Image.memory(image!);
  }
}
