// todo: reduce as much as needed
import 'package:pictorial_core/src/evaluator/base/expr.dart';
import 'package:pictorial_core/src/form/validation.dart';
import 'package:pictorial_core/src/service/service.dart';

// todo: -> UpperCase
enum UsrInputType {
  TEXT,
  NUMBER,
  DATE,
  TIME,
  DATETIME,
  EMAIL,
  PHONE,
  URL,
  PASSWORD,
  FILE,
  IMAGE,
  video,
  audio,
  color,
  select,
  multiSelect,
  radio,
  checkbox,
  slider,
  range,
  rating,
  starRating,
  emojiRating,
  likeDislike,
  smileyRating,
  thumbsRating,
  heartRating,
  upDownRating,
}

abstract class UsrInputProto {
  final String key;
  final bool required;
  final String? label;
  final List<Validation> validations;

  UsrInputProto({
    required this.key,
    required this.required,
    this.label,
    this.validations = const [],
  });

  factory UsrInputProto.build({
    required UsrInputType type,
    required String key,
    required bool required,
    String? label,
    List<Validation> validations = const [],
  }) {
    return switch (type) {
      UsrInputType.NUMBER => NumberUsrInputProto(
          key: key,
          required: required,
          label: label,
          validations: validations,
        ),
      UsrInputType.IMAGE => ImageUsrInputProto(
          key: key,
          required: required,
          label: label,
          validations: validations,
        ),
    UsrInputType.TEXT => TextUsrInputProto(
          key: key,
          required: required,
          label: label,
          validations: validations,
    ),
      _ => throw Exception('Unknown UsrInputType: $type'),
    };
  }
}

class TextUsrInputProto extends UsrInputProto {
  TextUsrInputProto({
    required String key,
    required bool required,
    String? label,
    List<Validation> validations = const [],
  }) : super(
          key: key,
          required: required,
          label: label,
          validations: validations,
        );

  @override
  String toString() {
    return 'TextUsrInputProto("key": $key, "required": $required, "label": $label, "validations": $validations)';
  }
}

class ImageUsrInputProto extends UsrInputProto {
  ImageUsrInputProto({
    required String key,
    required bool required,
    String? label,
    List<Validation> validations = const [],
  }) : super(
          key: key,
          required: required,
          label: label,
          validations: validations,
        );

  @override
  String toString() {
    return 'ImageUsrInputProto("key": $key, "required": $required, "label": $label, "validations": $validations)';
  }
}

// example of a UsrInputProto class
class NumberUsrInputProto extends UsrInputProto {
  NumberUsrInputProto({
    required String key,
    required bool required,
    String? label,
    List<Validation> validations = const [],
  }) : super(
          key: key,
          required: required,
          label: label,
          validations: validations,
        );

  @override
  String toString() {
    return 'NumberUsrInputProto("key": $key, "required": $required, "label": $label, "validations": $validations)';
  }
}

// ComfyAPI form and output
// todo!
class CdfSvcOutput implements SvcOutput {}

class CdfSvcInput implements SvcInput {}
