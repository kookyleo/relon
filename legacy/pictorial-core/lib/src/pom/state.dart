/// ** State ** is a class that holds the state data, state id and previous state id.
/// state id is used to identify the state, the same id means the same state.
/// previous state id is used to navigate back to the previous state.
/// the state data is not limited to a single level Map, it can be a nested Map, the key could be a path with dot separator,
/// and the value could be any Object. usually, this state data is incremental rather than a complete one.
class PictState {
  late int id;
  int? _previousId;
  Map<String, dynamic> data = Map();

  PictState({required this.id, PictState? previous}) : _previousId = previous?.id;

  PictState.fromJson(Map<String, dynamic> json) {
    id = json["id"] as int;
    _previousId = json["previous"] ?? null;
    data = Map.from(json["data"] as Map<String, dynamic>);
  }

  int? get previousId => _previousId;

  void setPreviousId(int? previousId) {
    _previousId = previousId;
  }

  Map<String, dynamic> toJson() {
    return {
      'id': id,
      'previous': _previousId,
      'data': data,
    };
  }

  void clear() => data.clear();

  void operator []=(String key, Object value) => set(key, value);

  void set(String key, Object value) {
    (Map<String, dynamic>, String) map = _navigateToLastMap(key, autoCreate: true);
    map.$1[map.$2] = value;
  }

  Object? operator [](String key) => get(key);

  Object? get(String key) {
    (Map<String, dynamic>, String) map = _navigateToLastMap(key);
    return map.$1[map.$2];
  }

  Object? remove(String key) {
    (Map<String, dynamic>, String) map = _navigateToLastMap(key);
    return map.$1.remove(map.$2);
  }

  bool containsKey(String key) {
    try {
      (Map<String, dynamic>, String) map = _navigateToLastMap(key);
      return map.$1.containsKey(map.$2);
    } catch (e) {
      return false;
    }
  }

  // Same idx means the same state
  @override
  bool operator ==(Object other) {
    return other is PictState && other.id == id;
  }

  @override
  int get hashCode => id;

  // Recursively merges another state into this state
  // if the key already exists, keep the original value
  PictState merge(PictState? other) {
    if (other == null) return this;
    data = data.merge(other.data);
    setPreviousId(other.id);
    return this;
  }

  // A Private helper function,
  // used to navigate to the second-to-last level of the Map, and return the last level Map and the last key
  (Map<String, dynamic>, String) _navigateToLastMap(String key, {bool autoCreate = false}) {
    var keys = key.split('.');
    var currentMap = data;
    for (var i = 0; i < keys.length - 1; i++) {
      if (currentMap.containsKey(keys[i])) {
        currentMap = currentMap[keys[i]] as Map<String, dynamic>;
      } else if (autoCreate) {
        var newMap = Map<String, dynamic>();
        currentMap[keys[i]] = newMap;
        currentMap = newMap;
      } else {
        throw Exception("Key not found: ${keys.sublist(0, i + 1).join('.')}");
      }
    }
    return (currentMap, keys.last);
  }
}

/// ** PlumpState ** is a State with complete state data, not incremental.
/// it just differs from State by name only, and used as a separate type to enforce type safety.
extension type PlumpState._(PictState _) implements PictState {
  PlumpState({required int id, PictState? previous}) : _ = PictState(id: id, previous: previous);

  PlumpState.fromJson(Map<String, Object> json) : _ = PictState.fromJson(json);
}

/// ** Extension on Map<String, dynamic> **
extension JsonMapExtensions on Map<String, dynamic> {
  /// Recursively merges another map into this map
  Map<String, dynamic> merge(Map<String, dynamic> other) {
    Map<String, dynamic> merged = {};
    // Merge the keys and values of the current map first
    forEach((key, value) {
      if (other.containsKey(key) && value is Map<String, dynamic> && other[key] is Map<String, dynamic>) {
        // Recursively merge the values of the current map and the other map
        merged[key] = value.merge(other[key]);
      } else {
        // Or keep the value of the current map
        merged[key] = value;
      }
    });
    // Add the keys and values in other but not in the current map
    other.forEach((key, value) {
      if (!containsKey(key)) {
        merged[key] = value;
      }
    });
    return merged;
  }
}