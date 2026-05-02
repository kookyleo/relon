import 'dart:io';
import 'dart:typed_data';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/cupertino.dart';
import 'package:pictorial_core/pictorial_core.dart';

abstract class FilePickerWrapper {
  Future<FilePickerResult?> pickFiles();
}

class FilePickerWrapperImpl implements FilePickerWrapper {
  @override
  Future<FilePickerResult?> pickFiles() {
    return FilePicker.platform.pickFiles();
  }
}

class LocalfileNative implements Localfile {
  late final File _file;
  final FilePickerWrapper filePickerWrapper;

  LocalfileNative({required this.filePickerWrapper});

  @override
  Future<void> select() async {
    try {
      FilePickerResult? result = await filePickerWrapper.pickFiles();
      if (result != null) {
        _file = File(result.files.single.path!);
      } else {
        // user abort, log it @todo!
      }
    } catch (e) {
      throw LocalFileException('$e #open failed');
    }
  }

  @override
  Future<Uint8List> read() async {
    assert(_file != null);
    try {
      return _file.readAsBytes();
    } catch (e) {
      throw LocalFileException('$e #read failed');
    }
  }

  @override
  Future<int> write(Uint8List data) async {
    assert(_file != null);
    try {
      await _file.writeAsBytes(data);
      return data.length;
    } catch (e) {
      throw LocalFileException('$e #write failed');
    }
  }

  @visibleForTesting
  File get file => _file;

  @visibleForTesting
  set file(File file) => _file = file;
}
