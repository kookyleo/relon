import 'dart:typed_data';

/// Workspace exception
extension type LocalFileException._(Exception _) implements Exception {
  LocalFileException(String message) : _ = Exception(message);
}

/// Operation on a local file
abstract class Localfile {
  // Select a file (by file picker)
  Future<void> select();

  // Read data from the file that has been opened
  Future<Uint8List> read();

  // Write data to the file that has been opened
  Future<int> write(Uint8List data);
}
