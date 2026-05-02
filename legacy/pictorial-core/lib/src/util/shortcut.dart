
/// A Shortcut key helper
/// The Shortcut(String) may be a single key, a key variable, their combination, their sequence or their combination of sequence.
/// for example, "Ctrl+P", "Ctrl, Ctrl", "Ctrl, $Option+p", etc.
class Shortcut {
  String? keys;

  Shortcut({this.keys});

  @override
  String toString() {
    return keys ?? '';
  }
}

// todo! implement the Shortcut parser, and the key variable replacement..