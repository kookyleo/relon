import 'dart:typed_data';

import 'package:flutter/material.dart';

import '../pom/entity.dart';
import '../util/logger.dart';
import 'entities.dart';

class Scene extends Entity {
  String name;
  String? textDescription;
  Uint8List? image;

  Scene(this.name, {this.textDescription, this.image});

  @override
  decode(Map<String, dynamic> bson) {
    name = bson['name'];
    textDescription = bson['textDescription'];
    image = bson['image'];
  }

  @override
  Map<String, dynamic> encode() {
    return {
      'name': name,
      'textDescription': textDescription,
      'image': image,
    };
  }

  @override
  void from(Entity entity) {
    switch (entity.runtimeType) {
      case Unspecified u:
        warn('Some additional information may be needed when transforming Unspecified to Scene');
        textDescription = u.textDescription;
        image = u.image;
        break;
      case Person p:
        warn('Transform Person to Scene will lose information');
        textDescription = p.textDescription;
        image = p.referenceImage as Uint8List?;
        break;

      case Txt _:
      case Shape _:
      case Scene _:
      default:
        throw Exception('Cannot transform to Scene from ${entity.runtimeType}');
    }
  }

  @override
  IconData get icon => Icons.image_outlined;

  @override
  Widget render({editable = false}) {
    return Image.memory(image!);
  }
}
