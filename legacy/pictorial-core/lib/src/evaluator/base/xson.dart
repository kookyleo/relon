import 'dart:convert';
import 'dart:typed_data';

import 'package:collection/collection.dart';

/// Xson, which resembles a JSON data structure but supports a greater variety of subtypes,
/// serves as the core data type for data expression within the add-on system.
abstract class Xson {
  // inner value of Xson object
  dynamic value;

  Xson();

  factory Xson.u8List(Uint8List value) => XsonUint8List(value);

  factory Xson.bytes(List<int> value) => XsonBytes(value);

  factory Xson.str(String value) => XsonStr(value);

  factory Xson.number(num value) => XsonNumber(value);

  factory Xson.bool(bool value) => XsonBool(value);

  factory Xson.nul() => XsonNul();

  factory Xson.object(Map<String, Xson> value) => XsonObject(value);

  factory Xson.array(List<Xson> value) => XsonArray(value);

  factory Xson.fromJsonStr(String jsonString) {
    if (jsonString == '') {
      return Xson.str('');
    }
    return fromDynamic(jsonDecode(jsonString));
  }

  static Xson fromDynamic(dynamic v) {
    if (v == null) return Xson.nul();
    if (v is String) return Xson.str(v);
    if (v is num) return Xson.number(v);
    if (v is bool) return Xson.bool(v);
    if (v is List) return Xson.array(v.asMap().entries.map((e) => fromDynamic(e.value)).toList());
    if (v is Map) return Xson.object(v.map((key, val) => MapEntry(key, fromDynamic(val))));

    throw ArgumentError("Unsupported type: ${v.runtimeType}, value: $v");
  }

  dynamic toJson();

  String toString() => jsonEncode(toJson());
}

/// XsonUint8List encapsulates an Uint8List for storing binary data, Usually used for bin data.
/// Note that can't created by Xson.fromDynamic or Xson.fromJsonStr
class XsonUint8List extends Xson {
  XsonUint8List(Uint8List value) : super() {
    this.value = value;
  }

  @override
  Uint8List get value => super.value as Uint8List;

  @override
  bool operator ==(Object other) {
    if (identical(this, other)) return true;
    if (other is XsonUint8List) return value.equals(other.value);
    if (other is XsonBytes) return value.equals(other.value);
    if (other is Uint8List) return value.equals(other);
    if (other is List<int>) return value.equals(other);
    return false;
  }

  // int get hashCode => value.hashCode;
  // Uint8List's hashCode based on its reference, not its content, so we replace it with ListEquality().hash(value)
  @override
  int get hashCode => const ListEquality().hash(value);

  @override
  dynamic toJson() {
    return value;
  }
}

/// XsonBytes encapsulates a List<Int> for storing binary data, Usually used for bin data.
/// Note that can't created by Xson.fromDynamic or Xson.fromJsonStr
class XsonBytes extends Xson {
  XsonBytes(List<int> bytes) : super() {
    value = bytes;
  }

  @override
  List<int> get value => super.value as List<int>;

  @override
  bool operator ==(Object other) {
    if (identical(this, other)) return true;
    if (other is XsonBytes) return value.equals(other.value);
    if (other is XsonUint8List) return value.equals(other.value);
    if (other is Uint8List) return value.equals(other);
    if (other is List<int>) return value.equals(other);
    return false;
  }

  @override
  int get hashCode => const ListEquality().hash(value);

  @override
  dynamic toJson() {
    return value;
  }
}

class XsonStr extends Xson {
  XsonStr(String str) : super() {
    value = str;
  }

  @override
  String get value => super.value as String;

  // ==
  @override
  bool operator ==(Object other) {
    var target = (other is XsonStr) ? other.value : other;
    if (target is String) return value == target;
    return false;
  }

  @override
  int get hashCode => value.hashCode;

  @override
  String toJson() => "${value}";
}

class XsonNumber extends Xson {
  XsonNumber(num number) : super() {
    value = number;
  }

  get value => super.value as num;

  // + - * /
  XsonNumber operator +(Object other) {
    var target = (other is XsonNumber) ? other.value : other;
    if (target is num) {
      return XsonNumber(value + target);
    }
    throw ArgumentError("Unsupported type: ${other.runtimeType}");
  }

  XsonNumber operator -(Object other) {
    var target = (other is XsonNumber) ? other.value : other;
    if (target is num) {
      return XsonNumber(value - target);
    }
    throw ArgumentError("Unsupported type: ${other.runtimeType}");
  }

  XsonNumber operator *(Object other) {
    var target = (other is XsonNumber) ? other.value : other;
    if (target is num) {
      return XsonNumber(value * target);
    }
    throw ArgumentError("Unsupported type: ${other.runtimeType}");
  }

  XsonNumber operator /(Object other) {
    var target = (other is XsonNumber) ? other.value : other;
    if (target is num) {
      return XsonNumber(value / target);
    }
    throw ArgumentError("Unsupported type: ${other.runtimeType}");
  }

  @override
  bool operator ==(Object other) {
    if (identical(this, other)) return true;
    if (other is XsonNumber) return value == other.value;
    if (other is num) return value == other;
    return false;
  }

  @override
  int get hashCode => value.hashCode;

  @override
  num toJson() => value;
}

class XsonBool extends Xson {
  XsonBool(bool boolean) : super() {
    value = boolean;
  }

  @override
  bool get value => super.value as bool;

  @override
  bool operator ==(Object other) {
    var target = (other is XsonBool) ? other.value : other;
    if (target is bool) {
      return value == target;
    }
    return false;
  }

  @override
  int get hashCode => value.hashCode;

  @override
  toJson() => value;
}

class XsonNul extends Xson {
  XsonNul();

  @override
  bool operator ==(Object other) => other is XsonNul;

  @override
  int get hashCode => 0;

  @override
  get value => null;

  @override
  toJson() => value;
}

class XsonObject extends Xson {
  XsonObject(Map<String, Xson> initialValue) : super() {
    value = initialValue;
  }

  get value => super.value as Map<String, Xson>;

  Xson? operator [](String key) => value[key];

  void operator []=(String key, Xson newValue) => value[key] = newValue;

  bool isEsonObject([String? tag]) {
    if (tag != null) {
      return (value as Map).containsKey(r'$schema') && (value as Map)[r'$schema'] == tag;
    }
    return (value as Map).containsKey(r'$schema');
  }

  @override
  bool operator ==(Object other) {
    var target = (other is XsonObject) ? other.value : other;
    if (target is Map) {
      if (value.length != target.length) return false;
      for (var key in value.keys) {
        if (!target.containsKey(key)) return false;
        if (value[key] != target[key]) return false;
      }
      return true;
    }
    return false;
  }

  @override
  int get hashCode => value.entries.fold(0, (sum, e) => sum + e.key.hashCode + e.value.hashCode);

  // Map<String, dynamic>
  toJson() {
    return (value as Map<String, Xson>).map((k, v) => MapEntry("${k}", v.toJson()));
  }
}

class XsonArray extends Xson {
  XsonArray(List<Xson> initialValue) : super() {
    value = initialValue;
  }

  Xson? operator [](int index) => value[index];

  void operator []=(int index, Xson newValue) {
    value[index] = newValue;
  }

  get value => super.value as List<Xson>;

  @override
  bool operator ==(Object other) {
    var target = (other is XsonArray) ? other.value : other;
    if (target is List) {
      if (value.length != target.length) return false;
      for (int i = 0; i < value.length; i++) {
        if (value[i] != target[i]) return false;
      }
      return true;
    }
    return false;
  }

  @override
  int get hashCode => value.fold(0, (sum, v) => sum + v.hashCode);

  @override
  toJson() {
    return (value as List<Xson>).map((e) => e.toJson()).toList();
  }
}
