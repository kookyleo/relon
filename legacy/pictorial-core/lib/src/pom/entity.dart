// We use Entity to express a specific and subdivided content type. such as Text, Shape, Person, etc..

import 'package:flutter/widgets.dart';

abstract class Entity {
  // Entities have a unique identifier.
  late String id;

  // Entity should be able to be converted between specific types and directions
  void from(Entity entity);

  // Entities should has a icon
  IconData get icon;

  // Entities should be rendered as flutter widgets, the editable parameter is used to determine whether it used for editing or not
  Widget render({editable = false});

  // Entities should be able to be loaded or decoded from a BSON document
  decode(Map<String, dynamic> bson);

  // Entities should be able to be dumped or encoded to a BSON document
  Map<String, dynamic> encode();
}
