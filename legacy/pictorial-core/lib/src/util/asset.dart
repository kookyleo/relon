import 'dart:typed_data';

import 'package:flutter/services.dart' show rootBundle;

Future<ByteData> loadAsset(String path) async {
  return await rootBundle.load(path);
}

Future<Uint8List> loadAssetAsUint8List(String path) async {
  return (await loadAsset(path)).buffer.asUint8List();
}

Future<String> loadAssetAsString(String path) async {
  return await rootBundle.loadString(path);
}
