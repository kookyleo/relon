import 'dart:typed_data';

import 'package:bson/bson.dart';

typedef Bson = Map<String, dynamic>;

Map<String, dynamic> deserialize(Uint8List dat) {
  return BsonCodec.deserialize(BsonBinary.from(dat));
}

Uint8List serialize(Bson dat) {
  return BsonCodec.serialize(dat).byteList;
}

extension Uint8ListExt on Uint8List {
  Map<String, dynamic> deserializeAsBson() {
    return BsonCodec.deserialize(BsonBinary.from(this));
  }
}

extension BsonExt on Bson {
  Uint8List serializeAsUint8List() {
    return BsonCodec.serialize(this).byteList;
  }
}
