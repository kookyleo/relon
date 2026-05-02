import 'package:pictorial_core/pictorial_core.dart';

class FrameInstance {
  final Frame frame;
  final (int, int) position;
  final int zindex;

  FrameInstance(this.frame, this.position, this.zindex);

  // overlap check
  bool overlap(FrameInstance other) {
    final x1 = position.$1;
    final y1 = position.$2;
    final x2 = position.$1 + frame.size.$1;
    final y2 = position.$2 + frame.size.$2;
    final x3 = other.position.$1;
    final y3 = other.position.$2;
    final x4 = other.position.$1 + other.frame.size.$1;
    final y4 = other.position.$2 + other.frame.size.$2;
    return x1 < x4 && x2 > x3 && y1 < y4 && y2 > y3;
  }
}

class Page {
  late final int _id;
  late final (double, double) _size;

  late (double, double, double, double) _padding;

  // late Map<String, FrameInstance> _frames;
  late List<dynamic> _frames;

  Page(this._id, this._size, [this._padding = (0, 0, 0, 0)]) {
    _frames = [];
  }

  // String? _currentFrameId;
  // Getters
  int get id => _id;

  (double, double) get size => _size;

  (double, double, double, double) get padding => _padding;

  // Setters
  set id(int id) => _id = id;

  set size((double, double) size) => _size = size;

  set padding((double, double, double, double) padding) => _padding = padding;

  // set currentFrameId(String frameId) => _currentFrameId = frameId;

  List<dynamic> get frames =>_frames;

  // Frame
  // FrameInstance? getFrame(String id) => _frames[id];
  Map<String, dynamic> getFrame(int index) {
    if (index < 0 || index >= _frames.length) {
      return {};
    }
    return _frames[index];
  }

  void addFrame(dynamic frame, [int index = -1]) {
    if (index == -1) {
      _frames.add(frame);
      return;
    }
    _frames.insert(index, frame);
  }

  // void addFrame(Frame frame, (int, int) position, int zindex) {
  //   // if two frames have overlapping positions and the zindex is the same, throw an error.
  //   for (final frameInstance in _frames.values) {
  //     if (frameInstance.overlap(FrameInstance(frame, position, zindex))) {
  //       throw ArgumentError('Frame position overlaps with another frame');
  //     }
  //   }
  //   // if frame overflows page body size, just warn but still add it.
  //   if (position.$1 < _padding.$4 ||
  //       position.$2 < _padding.$1 ||
  //       (position.$1 + frame.size.$1) > (_size.$1 - _padding.$2) ||
  //       (position.$2 + frame.size.$2) > (_size.$2 - _padding.$3)) {
  //     warn('Warning: Frame position overflows page body size');
  //   }
  //   // size, position and zindex restrict check passed
  //   _frames.putIfAbsent(frame.id, () => FrameInstance(frame, position, zindex));
  // }
  //
  // FrameInstance? getCurrentFrame(){
  //   return _currentFrameId==null?null :getFrame(_currentFrameId!);
  // }

  void removeFrame(int index) {
    _frames.removeAt(index);
  }

  void clear() {
    _frames.clear();
  }

  // One frame only in one page
  // void singleFrame(Frame frame) {
  //   // assert frame size is not set
  //   assert(frame.size == (0, 0));
  //   frame.size = ((_size.$1 - _padding.$2 - _padding.$4), (_size.$2 - _padding.$1 - _padding.$3));
  //   addFrame(frame, (_padding.$4, _padding.$1), 0);
  // }

  // To Json
  Map<String, dynamic> toJson() {
    final Map<String, dynamic> data = <String, dynamic>{};
    data['id'] = _id;
    data['size'] = [_size.$1, _size.$2];
    data['padding'] = [_padding.$1, _padding.$2, _padding.$3, _padding.$4];
    data['frames'] = _frames;
    return data;
  }

  // From Json
  Page.fromJson(Map<String, dynamic> json) {
    _id = json['id'];
    _size = (json['size'][0], json['size'][1]);
    _padding = (json['padding'][0], json['padding'][1], json['padding'][2], json['padding'][3]);
    _frames = json['frames'];
  }
}
