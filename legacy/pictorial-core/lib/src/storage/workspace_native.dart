import 'dart:io';
import 'dart:typed_data';

import 'package:pictorial_core/pictorial_core.dart';

class WorkspaceNative extends Workspace {
  late final String spaceId;
  late Directory? _root;
  late Directory? _current;

  WorkspaceNative(String id) {
    spaceId = '__PICTORIAL_SPACE_$id';
  }

  // Auto complete the real full path, providing the user a fake root
  PathString _chroot(PathString path) {
    if (path.value.startsWith('/')) {
      return PathString('${_root?.path}${path.value}');
    }
    return path;
  }

  @override
  Future<Workspace> open() async {
    String p = '${Directory.systemTemp.path}/$spaceId';
    try {
      switch ((await Directory(p).stat()).type) {
        case FileSystemEntityType.notFound:
          // create it if directory $p not exists
          _current = _root = await Directory(p).create();
          return this;
        case FileSystemEntityType.directory:
          // use it if directory $p exists
          _current = _root = Directory(p);
          return this;
        default:
          // throw exception if $p exist but is not a directory
          throw Exception('Specified path exist but is not a directory');
      }
    } catch (e) {
      throw WorkspaceException('$e #failed to open workspace');
    }
  }

  @override
  Future<WorkspaceDirectoryHandle> cd(PathString path) async {
    assert(_root != null); // must open first
    try {
      PathString p = PathString(_current!.path).merge(_chroot(path));
      switch ((await Directory(p.value).stat()).type) {
        case FileSystemEntityType.directory:
          _current = Directory(p.value);
          return _current as WorkspaceDirectoryHandle;
        default:
          throw PathNotFoundException(p.value, const OSError('Invalid path'));
      }
    } catch (e) {
      throw WorkspaceException('$e #failed to change directory');
    }
  }

  @override
  Future<Workspace> close() async {
    _root = null;
    _current = null;
    return this;
  }

  @override
  Future<WorkspaceDirectoryHandle> destroy() async {
    assert(_root != null);
    try {
      return (await _root!.delete(recursive: true)) as WorkspaceDirectoryHandle;
    } catch (e) {
      throw WorkspaceException('$e #failed to destroy workspace');
    }
  }

  @override
  Future<bool> exist(PathString path) async {
    assert(_root != null); // must open first
    try {
      PathString p = PathString(_current!.path).merge(_chroot(path));
      return (await FileSystemEntity.type(p.value)) != FileSystemEntityType.notFound;
    } catch (e) {
      throw WorkspaceException('$e #failed to check existence');
    }
  }

  @override
  Future<List<(String, WorkspaceEntityHandle)>> ls([PathString? path]) async {
    assert(_root != null); // must open first
    try {
      PathString p = path != null ? PathString(_current!.path).merge(_chroot(path)) : PathString(_current!.path);
      // check if the path is a directory or a file
      switch ((await FileSystemEntity.type(p.value))) {
        case FileSystemEntityType.file:
          return [(p.value.split('/').last, File(p.value) as WorkspaceEntityHandle)];
        case FileSystemEntityType.directory:
          return (await Directory(p.value).list().toList())
              .map((e) => (e.path.split('/').last, e as WorkspaceEntityHandle))
              .toList();
        default:
          throw PathNotFoundException(p.value, const OSError('Invalid path'));
      }
    } catch (e) {
      throw WorkspaceException('$e #failed to list directory');
    }
  }

  @override
  Future<WorkspaceDirectoryHandle> mkdir(PathString path) async {
    assert(_root != null); // must open first
    try {
      PathString p = PathString(_current!.path).merge(_chroot(path));
      return await Directory(p.value).create() as WorkspaceDirectoryHandle;
    } catch (e) {
      throw WorkspaceException('$e #failed to create directory');
    }
  }

  @override
  Future<Uint8List> read(PathString path) async {
    assert(_root != null); // must open first
    try {
      PathString p = PathString(_current!.path).merge(_chroot(path));
      final file = File(p.value);
      return await file.readAsBytes();
    } catch (e) {
      throw WorkspaceException('$e #failed to read file');
    }
  }

  @override
  Future<WorkspaceEntityHandle> rm(PathString path) async {
    assert(_root != null); // must open first
    try {
      PathString p = PathString(_current!.path).merge(_chroot(path));
      switch (await FileSystemEntity.type(p.value)) {
        case FileSystemEntityType.directory:
          return (await Directory(p.value).delete(recursive: true)) as WorkspaceEntityHandle;
        case FileSystemEntityType.file:
          return (await File(p.value).delete()) as WorkspaceEntityHandle;
        default:
          throw PathNotFoundException(path.value, const OSError('Invalid path'));
      }
    } catch (e) {
      throw WorkspaceException('$e #failed to remove file or directory');
    }
  }

  @override
  Future<int> write(PathString path, Uint8List data) async {
    assert(_root != null); // must open first
    try {
      PathString p = PathString(_current!.path).merge(_chroot(path));
      final file = File(p.value);
      await file.writeAsBytes(data);
      return data.length;
    } catch (e) {
      throw WorkspaceException('$e #failed to write file');
    }
  }
}
