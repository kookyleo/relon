


import '../evaluator/base/expr.dart';

abstract class Validation {
  bool validate(dynamic input);

  String text();

  Validation();

  factory Validation.fromExpr(FnCall f) {
    return switch (f.name) {
      'largerThan' => LargerThan((f.args[0] as Literal).value as num),
      'fileTypeIsOneOf' =>
          FileTypeIsOneOf((f.args[0] as ComplexArray).value.map((e) => (e as Literal).value.toString()).toList()),
      'fileSizeBetween' => FileSizeBetween((f.args[0] as Literal).value as num, (f.args[1] as Literal).value as num),
      _ => throw Exception('Unknown validation function: ${f.name}'),
    };
  }
}

class FileSizeBetween extends Validation {
  final num min;
  final num max;

  FileSizeBetween(this.min, this.max);

  @override
  bool validate(dynamic input) {
    return input > min && input < max;
  }

  @override
  String text() {
    return 'File size must be between $min and $max';
  }

  @override
  String toString() {
    return 'FileSizeBetween(min: $min, max: $max)';
  }
}

class FileTypeIsOneOf extends Validation {
  final List<String> mimeTypes;

  FileTypeIsOneOf(this.mimeTypes);

  @override
  bool validate(dynamic input) {
    return mimeTypes.contains(input);
  }

  @override
  String text() {
    return 'File must be one of the following types: ${mimeTypes.join(', ')}';
  }

  @override
  String toString() {
    return 'FileTypeIsOneOf("mimeTypes": $mimeTypes)';
  }
}

// example of a validation class
class LargerThan extends Validation {
  final num value;

  LargerThan(this.value);

  @override
  bool validate(dynamic input) {
    return input > value;
  }

  @override
  String text() {
    return 'Value must be larger than $value';
  }

  @override
  String toString() {
    return 'LargerThan("value": $value)';
  }
}
