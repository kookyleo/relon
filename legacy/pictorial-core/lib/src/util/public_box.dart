/// A simple shared storage for storing key-value pairs.
class PublicBox {
  static final Map<String, dynamic> _entities = {};

  static void set(String key, String value) => _entities[key] = value;

  static dynamic get(String key) => _entities[key];

  // Look up the value of [key], or put a new entry first if it isn't there, then return the value.
  static dynamic smartGet(String key, dynamic ifAbsent()) => _entities.putIfAbsent(key, ifAbsent);

  static void remove(String key) => _entities.remove(key);
}
