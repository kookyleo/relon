import 'package:pictorial_core/src/pom/annotation.dart';

import 'entity.dart';

enum Type {
  Image,
  Text,
  Shape,
  // ...
  Adjustment, // Ref: Ps Adjustment Layer
}

enum BlendingMode {
  Normal,
  Multiply,
  Screen,
  Overlay,
  Darken,
  Lighten,
  ColorDodge,
  ColorBurn,
  HardLight,
  SoftLight,
  Difference,
  Exclusion,
  Hue,
  Saturation,
  Color,
  Luminosity,
  // ...
  // todo: prune
}

enum Binding {
  None,
  // ...
  // Ps. Layer Blending Modes, Effects...
  // Ps. Masking ..
}

class Layer {
  final String id;
  final Type type;
  int opacity = 100;  // Ref: Ps Fill
  bool visible = true;
  bool locked = false;
  List<Binding> bindings = [];
  List<Annotation> annotations =[];

  Layer(this.id, this.type);

  @override
  bool operator ==(Object other) {
    if (identical(this, other)) return true;
    return other is Layer && other.id == id;
  }

  @override
  int get hashCode => id.hashCode;

  // To Json
  Map<String, dynamic> toJson() {
    return {
      'id': id,
      'type': type.toString(),
      'opacity': opacity,
      'visible': visible,
      'locked': locked,
      'bindings': bindings.map((e) => e.toString()).toList(),
      'annotations': annotations.map((e) => e.toJson()).toList(),
    };
  }

  // From Json
  factory Layer.fromJson(Map<String, dynamic> data) {
    final layer = Layer(data['id'], Type.values.firstWhere((e) => e.toString() == data['type']));
    layer.opacity = data['opacity'];
    layer.visible = data['visible'];
    layer.locked = data['locked'];
    for (final binding in data['bindings']) {
      layer.bindings.add(Binding.values.firstWhere((e) => e.toString() == binding));
    }
    for (final annotation in data['annotations']) {
      layer.annotations.add(Annotation.fromJson(annotation));
    }
    return layer;
  }
}
