import 'dart:async';
import 'dart:io';
import 'dart:js_interop';
import 'dart:js_util';
import 'dart:typed_data';

import 'package:pictorial_core/pictorial_core.dart';
import 'package:web/web.dart';

class WorkspaceWeb implements Workspace {
  late final String spaceId;
  late FileSystemDirectoryHandle? _root;
  late FileSystemDirectoryHandle? _current;
  late PathString? _currentPath;

  WorkspaceWeb(String id) {
    spaceId = '__PICTORIAL_SPACE_$id';
  }

  // current path + required path => abstract path
  PathString _getAbsPath(PathString required) {
    return _currentPath!.merge(required);
  }

  // get specific directory handle
  // the param absPath must be a valid abstract dir path
  Future<FileSystemDirectoryHandle> _getDirectoryHandle(PathString absPath, {create = false}) async {
    FileSystemDirectoryHandle cur = _root!;
    List<String> path = absPath.explode();
    path.removeAt(0); // remove the first '/'
    for (String p in path) {
      cur = await promiseToFuture(cur.getDirectoryHandle(p, FileSystemGetDirectoryOptions(create: create)));
    }
    return cur;
  }

  @override
  Future<Workspace> open() async {
    try {
      FileSystemDirectoryHandle s = await promiseToFuture(window.navigator.storage.getDirectory());
      _current = _root = await promiseToFuture(s.getDirectoryHandle(spaceId, FileSystemGetDirectoryOptions(create: true)));
      _currentPath = PathString('/');
      return this;
    } catch (e) {
      throw WorkspaceException('$e #failed to open workspace');
    }
  }

  @override
  Future<WorkspaceDirectoryHandle> cd(PathString path) async {
    assert(_root != null);
    try {
      PathString p = _getAbsPath(path);
      _currentPath = p;
      _current = await _getDirectoryHandle(p);
      return _current! as WorkspaceDirectoryHandle;
    } catch (e) {
      throw WorkspaceException('$e #failed to change directory');
    }
  }

  @override
  Future<List<(String, WorkspaceEntityHandle)>> ls([PathString? path]) async {
    assert(_root != null);
    try {
      PathString p = path != null ? _getAbsPath(path!) : _currentPath!;
      (PathString, String?) s = p.split();
      FileSystemDirectoryHandle parent = await _getDirectoryHandle(s.$1);
      List<(String, FileSystemHandle)> entities = await parent.ls();
      // list the root directory
      if (s.$2 == null) {
        return entities.map((e) => (e.$1, e.$2 as WorkspaceEntityHandle)).toList();
      }
      // else
      for (var e in entities) {
        if (e.$1 == s.$2) {
          if (e.$2.kind == 'directory') {
            return (await (e.$2 as FileSystemDirectoryHandle).ls()).map((e) => (e.$1, e.$2 as WorkspaceEntityHandle)).toList();
          } else if (e.$2.kind == 'file') {
            return [(e.$1, e.$2 as WorkspaceEntityHandle)];
          }
        }
      }
      // not found
      throw PathNotFoundException(p.value, const OSError('Invalid path'));
    } catch (e) {
      throw WorkspaceException('$e #failed to list directory');
    }
  }

  @override
  Future<bool> exist(PathString path) async {
    assert(_root != null);
    try {
      PathString p = _getAbsPath(path);
      (PathString, String?) s = p.split();
      if (s.$2 == null) return true; // root directory always exists
      FileSystemDirectoryHandle parent = await _getDirectoryHandle(s.$1);
      List<(String, FileSystemHandle)> entities = await parent.ls();
      for (var e in entities) {
        if (e.$1 == s.$2) return true;
      }
      return false;
    } catch (e) {
      if (e.toString().startsWith('NotFoundError') /*|| e.toString().startsWith('TypeMismatchError')*/) {
        return false;
      }
      throw WorkspaceException('$e #failed to check existence');
    }
  }

  @override
  Future<WorkspaceDirectoryHandle> mkdir(PathString path) async {
    assert(_root != null);
    try {
      FileSystemDirectoryHandle h = await _getDirectoryHandle(_getAbsPath(path), create: true);
      return h as WorkspaceDirectoryHandle;
    } catch (e) {
      throw WorkspaceException('$e #failed to create directory');
    }
  }

  @override
  Future<Uint8List> read(PathString path) async {
    assert(_root != null);
    try {
      PathString p = _getAbsPath(path);
      (PathString, String?) s = p.split();
      FileSystemDirectoryHandle parent = await _getDirectoryHandle(s.$1);
      FileSystemFileHandle last = await promiseToFuture(parent.getFileHandle(s.$2!));
      File f = await promiseToFuture(last.getFile());
      JSArrayBuffer buffer = await promiseToFuture(f.arrayBuffer());
      Uint8List u8l = Uint8List.view(buffer as ByteBuffer);
      return u8l;
    } catch (e) {
      throw WorkspaceException('$e #failed to read file');
    }
  }

  @override
  Future<int> write(PathString path, Uint8List data) async {
    assert(_root != null);
    try {
      PathString p = _getAbsPath(path);
      (PathString, String?) s = p.split();
      FileSystemDirectoryHandle parent = await _getDirectoryHandle(s.$1, create: true);
      FileSystemFileHandle last = await promiseToFuture(parent.getFileHandle(s.$2!, FileSystemGetFileOptions(create: true)));
      FileSystemWritableFileStream writer = await promiseToFuture(last.createWritable());
      await promiseToFuture(writer.write(data as Blob));
      await promiseToFuture(writer.close());
      return data.length;
    } catch (e) {
      throw WorkspaceException('$e #failed to write file');
    }
  }

  @override
  Future<void> rm(PathString path) async {
    assert(_root != null);
    try {
      PathString p = _getAbsPath(path);
      (PathString, String?) s = p.split();
      if (s.$2 == null) {
        throw WorkspaceException('Cannot remove root directory directly, use destroy() instead');
      }
      FileSystemDirectoryHandle parent = await _getDirectoryHandle(s.$1);
      List<(String, FileSystemHandle)> entities = await parent.ls();
      for (var e in entities) {
        if (e.$1 == s.$2) {
          return await promiseToFuture(parent.removeEntry(s.$2!, FileSystemRemoveOptions(recursive: true)));
        }
      }
      throw PathNotFoundException(p.value, const OSError('Invalid path'));
    } catch (e) {
      throw WorkspaceException('$e #failed to remove file or directory');
    }
  }

  @override
  Future<Workspace> close() async {
    assert(_root != null);
    _root = null;
    return this;
  }

  // destroy the workspace
  @override
  Future<void> destroy() async {
    assert(_root != null);
    FileSystemDirectoryHandle s = await promiseToFuture(window.navigator.storage.getDirectory());
    return await promiseToFuture(s.removeEntry(spaceId, FileSystemRemoveOptions(recursive: true)));
  }
}

// === Patch for FileSystemDirectoryHandle ===
extension on FileSystemDirectoryHandle {
  // I don't know why the following 3 methods are not built into package:web, can't understand ..
  @JS("keys")
  external JsAsyncIterator<JSAny> keys();

  @JS("values")
  external JsAsyncIterator<FileSystemHandle> values();

  @JS("entries")
  external JsAsyncIterator<FileSystemHandle> entries();

  // list files and directories in a directory
  Future<List<(String, FileSystemHandle)>> ls() async {
    List<(String, FileSystemHandle)> list = [];
    await for (final JSAny e in entries().asStream()) {
      List<dynamic> r = e as List<dynamic>;
      list.add((r[0] as String, r[1] as FileSystemHandle));
    }
    return list;
  }
}

extension type JsAsyncIteratorState<T extends JSAny>._(JSObject _) implements JSObject {
  external bool get done;

  external T get value;
}

extension type JsAsyncIterator<T extends JSAny>._(JSObject _) implements JSObject {
  external JSPromise<JsAsyncIteratorState<T>> next();

  Stream<T> asStream() async* {
    while (true) {
      final result = await next().toDart;
      if (result.done) break;
      yield result.value;
    }
  }
}
