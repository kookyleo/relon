import 'dart:convert';

import 'package:crypto/crypto.dart' as crypto;
import 'package:uuid/uuid.dart';

export 'util/asset.dart';
export 'util/bson.dart';
export 'util/logger.dart';
export 'util/path_string.dart';
export 'util/zip.dart';

String md5(String input) {
  return crypto.md5.convert(utf8.encode(input)).toString();
}

// v7 is time-based
// param dash: whether to include dashes in the returned UUID
String uuid([bool dash = true]) {
  return (!dash) ? const Uuid().v7().replaceAll('-', '') : const Uuid().v7();
}
