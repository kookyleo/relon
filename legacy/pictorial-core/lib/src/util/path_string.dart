/// **PathString** is a string that represents a path.
///
/// PathString can be created from a string, eg. `PathString('/foo/bar')`
/// or from a list of parts, eg. `PathString.fromList(['foo', 'bar'])`
/// it also can be converted to a normalized string. eg. `PathString('/foo/bar').value => '/foo/bar'`
///
/// The string can be started with `/` or `.` or `..` or just a name,
/// eg. `/foo/bar`, `../baz`, `./baz`, `/baz`, `baz`.. etc. all are valid
///
/// PathString can be split to dir(path) part and last part
/// eg. `PathString('/foo/bar').split() => (PathString('/foo'), 'bar')`
///
/// PathString can be merged with another PathString
/// eg. `PathString('/foo/bar').merge(PathString('baz')) => PathString('/foo/bar/baz')`
/// `PathString('/foo/bar').merge(PathString('../baz')) => PathString('/foo/baz')`
/// `PathString('/foo/bar').merge(PathString('../../baz')) => PathString('/baz')`
/// `PathString('/foo/bar').merge(PathString('../../../baz')) => throw an exception: "Invalid path"`
/// `PathString('/foo/bar').merge(PathString('/baz')) => PathString('/baz')`
/// `PathString('/foo/bar').merge(PathString('/foo/baz')) => PathString('/foo/baz')`
/// `PathString('/foo/bar').merge(PathString('./baz')) => PathString('/foo/bar/baz')`
/// `PathString('/foo/bar').merge(PathString('.')) => PathString('/foo/bar')`
/// `PathString('/foo/bar').merge(PathString('..')) => PathString('/foo')`
/// `PathString('/foo/bar').merge(PathString('')) => PathString('/foo/bar')`
/// `PathString('/foo/bar').merge(PathString('/')) => PathString('/')`
///
/// PathString can be merged with a string using operator /,
/// eg. `PathString('/foo/bar') / 'baz' => PathString('/foo/bar/baz')`
/// (in this case, the second part should not start with `.` or `/` or contain space, otherwise, an exception will be thrown)
///
/// PathString can be exploded to a list of parts
/// eg. `PathString('/foo/bar').explode() => ['foo', 'bar']`
///
extension type PathString._(String _) {
  PathString(this._);

  PathString.fromList(List<String> parts) : _ = parts.join('/');

  // get normalized value
  String get value {
    return explode().join('/');
  }

  // split to dir(path) part and last part
  (PathString, String?) split() {
    List<String> parts = explode();
    if (parts.length > 1) {
      return (PathString.fromList(parts.sublist(0, parts.length - 1)), parts.last);
    } else if (parts.length == 1) {
      return (PathString('/'), null);
    } else {
      throw Exception('Invalid path');
    }
  }

  // merge two paths
  PathString merge(PathString other) {
    List<String> first = explode();
    List<String> second = other.explode();
    if (second.isEmpty) {
      return PathString.fromList(first);
    }
    if (second.first == '/') {
      // if the second path is started with '/', then it's an absolute path, so the first path should be ignored
      first = [];
    }
    List<String> r = [];
    for (String p in [...first, ...second]) {
      if (p == '.') {
        // do nothing
      } else if (p == '..') {
        if ((r.length == 1 && r.first == '/') || r.isEmpty) {
          // overflows the root
          throw Exception('Invalid path');
        }
        r.removeLast();
      } else if (p.isEmpty || p.startsWith('.') || p.contains(' ')) {
        // check if the part is legal: not empty and not started with '.' and not contains ' '
        throw Exception('Invalid path');
      } else {
        r.add(p);
      }
    }
    return PathString.fromList(r);
  }

  // operator / is used to merge two paths
  PathString operator /(String second) {
    if (second.startsWith('.') || second.startsWith('/') || second.contains(' ')) {
      throw Exception('Invalid path');
    }
    return merge(PathString(second));
  }

  // split path by '/', and remove empty parts
  // eg. "/foo/bar" => ["foo", "bar"], "//foo" => ["/", "foo"], "./foo" => [".", "foo"]
  // Note that if the first character is "/", then it should be retained
  List<String> explode() {
    List<String> r = _.startsWith('/') ? ['/'] : [];
    for (String p in _.split('/')) {
      if (p.isNotEmpty) {
        r.add(p);
      }
    }
    return r;
  }
}
