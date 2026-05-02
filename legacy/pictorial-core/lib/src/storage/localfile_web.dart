import 'dart:js_interop';
import 'dart:js_util';
import 'dart:typed_data';

import 'package:flutter/cupertino.dart';
import 'package:pictorial_core/pictorial_core_web.dart';
import 'package:web/web.dart';

abstract class OpenFilePickerWrapper {
  JSPromise<JSArray<FileSystemFileHandle>> openFilePicker();
}

class OpenFilePickerWrapperImpl implements OpenFilePickerWrapper {
  String startIn;
  List<(String, Map<String, List<String>>)> types;

  // bool multiple;

  // startIn is the default directory when the file picker is opened.
  // types is an array of objects that specify the types of files that the file picker should allow the user to select.
  OpenFilePickerWrapperImpl({required this.startIn, required this.types /* this.multiple = false */
      }) {
    // the startIn value must be one of the following strings: "desktop", "documents", "downloads", "music", "pictures", or "videos".
    assert(["desktop", "documents", "downloads", "music", "pictures", "videos"].contains(startIn));
    // types must be an array of objects, each of which must have a description property and an accept property.
    if (types.isEmpty) {
      types = [
        ("All Files", {'*/*': []})
      ];
    }
  }

  @override
  JSPromise<JSArray<FileSystemFileHandle>> openFilePicker() {
    return showOpenFilePicker(jsify({
      "startIn": startIn,
      "types": types.map((e) => jsify({"description": e.$1, "accept": e.$2})).toList(),
      "multiple": false, // this.multiple
    }));
  }
}

class LocalfileWeb implements Localfile {
  late final FileSystemFileHandle? _fileHandle;

  final OpenFilePickerWrapper openFilePickerWrapper;

  LocalfileWeb({required this.openFilePickerWrapper});

  @override
  Future<void> select() async {
    try {
      final [fh] = await promiseToFuture<List<dynamic>>(openFilePickerWrapper.openFilePicker());
      _fileHandle = fh;
    } catch (e) {
      if (e.toString().startsWith("AbortError")) {
        // todo! log as user abort
      } else {
        throw LocalFileException('$e #open failed');
      }
    }
  }

  @override
  Future<Uint8List> read() async {
    assert(_fileHandle != null);
    try {
      File file = await promiseToFuture<File>(_fileHandle!.getFile());
      return await file.bytes();
    } catch (e) {
      throw LocalFileException('$e #read failed');
    }
  }

  @override
  Future<int> write(Uint8List data) async {
    assert(_fileHandle != null);
    try {
      FileSystemWritableFileStream writableFileStream = await promiseToFuture(_fileHandle!.createWritable());
      await promiseToFuture(writableFileStream.write(data.toJS));
      await promiseToFuture(writableFileStream.close());
      return data.length;
    } catch (e) {
      throw LocalFileException('$e #write failed');
    }
  }

  @visibleForTesting
  FileSystemFileHandle? get fileHandle => _fileHandle;
}

// Supplement the missing methods in [package:web] library
extension FileExtension on File {
  // A Promise that fulfills with a Uint8Array object containing the blob data.
  // see more at: https://developer.mozilla.org/en-US/docs/Web/API/Blob/bytes
  // external JSPromise<JSUint8Array> bytes(); // ** Fuck chrome, it's firefox only!! **

  Future<Uint8List> bytes() async {
    JSArrayBuffer ab = await promiseToFuture(this.arrayBuffer());
    JSUint8Array uint8array = callConstructor(window['Uint8Array'], [ab]);
    return uint8array.toDart;
  }
}

// Open a file picker to let the user select one or more files
// It returns a Promise which resolves to an array of FileSystemFileHandle objects representing the selected files.
// The options parameter is an json object that specifies the options for the file picker.
// see more at: https://developer.mozilla.org/en-US/docs/Web/API/Window/showOpenFilePicker#options
@JS('window.showOpenFilePicker')
external JSPromise<JSArray<FileSystemFileHandle>> showOpenFilePicker([JSObject? options]);

// Open a file picker to let the user select a file to save
// It returns a Promise which resolves to a FileSystemFileHandle object representing the selected file.
// The options parameter is an json object that specifies the options for the file picker.
// see more at: https://developer.mozilla.org/en-US/docs/Web/API/Window/showSaveFilePicker#options
@JS('window.showSaveFilePicker')
external JSPromise<FileSystemFileHandle> showSaveFilePicker([JSObject? options]);

// Open a directory picker to let the user select a directory
// It returns a Promise which resolves to a FileSystemDirectoryHandle object representing the selected directory.
// The options parameter is an json object that specifies the options for the directory picker.
// see more at: https://developer.mozilla.org/en-US/docs/Web/API/Window/showDirectoryPicker#options
@JS('window.showDirectoryPicker')
external JSPromise<FileSystemDirectoryHandle> showDirectoryPicker([JSObject? options]);
