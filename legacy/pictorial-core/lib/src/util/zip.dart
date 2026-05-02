import 'dart:io';
import 'dart:typed_data';

import 'package:archive/archive_io.dart' if (kIsWeb) 'package:archive/archive.dart';
import 'package:pictorial_core/pictorial_core.dart' show PathString;
import 'package:path/path.dart';

/// Zip exception
extension type ZipExtension._(Exception _) implements Exception {
  ZipExtension(String message) : _ = Exception(message);
}

/// A **Compatible** handler to a zip file
class Zip {
// Unzip and recursively iterate over all files in the zip data
  static Future<void> unzip(Uint8List zipBytes, void Function(PathString path, Uint8List data) callback) async {
    try {
      final archive = ZipDecoder().decodeBytes(zipBytes);
      for (final file in archive) {
        callback(PathString(file.name), file.content);
      }
    } catch (e) {
      throw ZipExtension('$e #failed to unzip bytes');
    }
  }

// Zip all the data with the given paths to a zipped Uint8List
  static Future<Uint8List> zip(Map<PathString, Uint8List> data) {
    try {
      final archiveOut = Archive();
      for (final entry in data.entries) {
        archiveOut.addFile(ArchiveFile(entry.key.toString(), entry.value.length, entry.value));
      }
      return Future.value(ZipEncoder().encode(archiveOut) as Uint8List);
    } catch (e) {
      throw ZipExtension('$e #failed to zip data');
    }
  }

  static zipDir(Directory dir, String? file) async {
    await ZipFileEncoder().zipDirectoryAsync(dir, filename: file);
  }

  static Future<void> unZipToDir(File zipFile) async {
    var bytes = await zipFile.readAsBytes();
    var out = dirname(zipFile.path) + '/' + basenameWithoutExtension(zipFile.path);
    print('out_$out');

    var archive = ZipDecoder().decodeBytes(bytes);

    for (final file in archive) {
      final filename = file.name;

      // 跳过 __MACOSX 和隐藏文件
      if (filename.contains('__MACOSX') || filename.startsWith('.')) {
        continue;
      }

      final filePath = '$out/$filename';

      if (file.isFile) {
        // 处理文件
        final data = file.content as List<int>;
        File(filePath)
          ..createSync(recursive: true)
          ..writeAsBytesSync(data);
      } else {
        // 处理目录
        Directory(filePath).createSync(recursive: true);
      }
    }
  }

}
