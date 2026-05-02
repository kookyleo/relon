import 'dart:typed_data';

import 'package:pictorial_core/pictorial_core.dart' show PathString;

/// A **Compatible** handler to a directory
/// You can `extension` this type again, and use `as` to cast it in case of need.
/// for example:
/// ```dart
/// extension type DesktopDirectoryHandle._(DirectoryHandle _) {
///   path() => (_ as Directory).path;
/// }
/// var b = await workspace.rm(PathString('/foo/bar'));
/// print((b as DesktopDirectoryHandle).path());
/// ```
extension type WorkspaceDirectoryHandle._(Object _) {}

/// A **compatible** handler to a file
extension type WorkspaceFileHandle._(Object _) {}

/// The super type of both file and directory handlers
extension type WorkspaceEntityHandle._(Object _) {}

/// Workspace exception
extension type WorkspaceException._(Exception _) implements Exception {
  WorkspaceException(String message) : _ = Exception(message);
}

/// Workspace interface
/// Workspace is a virtual file system that provides a set of APIs to manage files and directories.
abstract class Workspace {
  // Open a workspace
  Future<Workspace> open();

  // Change directory
  Future<WorkspaceDirectoryHandle> cd(PathString path);

  // List files and directories, return a list of tuples, each tuple contains a name and a handler
  Future<List<(String, WorkspaceEntityHandle)>> ls([PathString? path]);

  // Check if a file or directory exists
  Future<bool> exist(PathString path);

  // Create a directory
  Future<WorkspaceDirectoryHandle> mkdir(PathString path);

  // Read data from a file
  Future<Uint8List> read(PathString path);

  // Write data to a file
  Future<int> write(PathString path, Uint8List data);

  // Remove a file or directory
  Future<void> rm(PathString path);

  // Close a workspace
  Future<Workspace> close();

  // Destroy a workspace
  Future<void> destroy();
}
