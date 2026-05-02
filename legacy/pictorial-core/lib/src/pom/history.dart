import 'dart:collection';

import 'package:flutter/foundation.dart';
import 'package:pictorial_core/src/pom/state.dart';

/// An Exception type for the history extension.
extension type HistoryExtension._(Exception _) implements Exception {
  HistoryExtension(String message) : _ = Exception(message);
}

/// ** StatCache ** is a simple cache system for the state object.
class StateCache {
  Queue<PlumpState> _cache = Queue();

  /// The size of the cache
  int? cacheSize;

  StateCache({this.cacheSize = 8});

  /// Push a new state into the cache
  void push(PlumpState stat) {
    // The easiest way to implement a cache system with swap in/swap out functionality, hah..
    if (_cache.length >= cacheSize!) {
      _cache.removeFirst();
    }
    _cache.add(stat);
  }

  /// Get a state by index, index is negative number,
  /// eg. -1, -2 .. -n which means the last state, the second last state, .. the nth last state.
  PlumpState get(int index) {
    if (index >= 0 || index.abs() > _cache.length) {
      throw HistoryExtension("Index out of range");
    }
    return _cache.elementAt(_cache.length + index);
  }

  /// Get the last state in the cache
  PlumpState get last => _cache.last;

  /// Find a state by stateId
  PlumpState? find(int stateId) {
    try {
      return _cache.firstWhere((s) => s.id == stateId);
    } catch (e) {
      return null;
    }
  }
}

/// ** History ** is a sets of states, which can be navigated back and forth.
class History {
  List<PictState> _states = [];
  late StateCache _fullStateCache;
  int? historyWindowSize;
  int? fsCacheSize;

  /// inner current state index
  int _currentIdx = 0;

  int get length => _states.length;

  History({this.historyWindowSize = 64, this.fsCacheSize = 8}) {
    _fullStateCache = StateCache(cacheSize: fsCacheSize);
  }

  /// Get the last state in the history
  PlumpState get state => _fullStateCache.last;

  /// Find a state by stateId, return the state and its index in the states list
  (PictState?, int?) find(int stateId) {
    // try from the cache first
    PictState? cached = _fullStateCache.find(stateId);
    if (cached != null) return (cached, -1);
    // then reverse search from the states[0, -fsCacheSize]
    for (int i = (_states.length - fsCacheSize!) - 1; i >= 0; i--) {
      if (_states[i].id == stateId) return (_states[i], i);
    }
    // miss
    return (null, null);
  }

  /// Merge a set of states into a PlumpState from a specific idx of the states list.
  /// idx is the index of the current states list
  PlumpState plumpUp(int idx) {
    if (idx < 0 || idx >= _states.length) throw HistoryExtension("Index out of range");
    if (idx >= _states.length - fsCacheSize!) {
      return _fullStateCache.get(idx - _states.length);
    } else {
      // Merge all the way back to the first state
      // This is a little bit expensive operation, but it's necessary, and fortunately not very frequent because of the presence of the cache.
      // Another attention point is that this method is only correct when the history is linear, no branching, no forking..
      // So in that case, snapshot the state before branching, and restore it when needed. see more in the `pushState` method.
      PictState cur = _states[idx];
      int? previousId = cur.previousId;
      while (previousId != null) {
        cur = cur.merge(_states[previousId]);
        previousId = cur.previousId;
      }
      // cur.previousId == null, means cur a is PlumpState
      return cur as PlumpState;
    }
  }

  /// Push a new state into the history
  void pushState(PictState state) {
    // if the history is full, swap out the oldest state
    if (_states.length >= historyWindowSize!) {
      _states[1].merge(_states[0]);
      _states.removeAt(0);
    }
    // when the new state.previousId is null, it means the state is the first state in the history.
    if (state.previousId == null) {
      _states.add(state);
      _fullStateCache.push(state as PlumpState);
    }
    // else when the new state.previousId is not equal to the last state id, it means a branching or forking happened.
    // In this case, we need to snapshot the current state and restore it (as a PlumpState).
    else if (state.previousId != _states.last.id) {
      (PictState?, int?) previous = find(state.previousId!);
      if (previous.$1 == null) {
        throw HistoryExtension("Previous state not found");
      }
      _states.add(state.merge((previous.$2 == -1) ? previous.$1! : plumpUp(previous.$2!)));
      _fullStateCache.push(state as PlumpState);
    }
    // else the new state is a linear continuation of the last state.
    else {
      _states.add(state);
      _fullStateCache.push(state.merge(_fullStateCache.last) as PlumpState);
    }
    // finally, update the current index
    _currentIdx = _states.length - 1;
    return;
  }

  /// Navigate back to the previous state
  PictState back() => go(-1);

  /// Navigate forward to the next state
  PictState forward() => go(1);

  /// Navigate to a specific state by a delta number
  PlumpState go(int delta) {
    int sum = _currentIdx + delta;
    if (sum < 0 || sum >= _states.length) throw HistoryExtension("Index out of range");
    return plumpUp(_currentIdx = sum);
  }

  List<PictState> curStates() => _states;

  StateCache curCache() => _fullStateCache;

  @visibleForTesting
  List<PictState> debugCurStates() => _states;

  @visibleForTesting
  StateCache debugCurCache() => _fullStateCache;
}
