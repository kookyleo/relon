import 'layer.dart';

class LayerStack {
  final List<Layer> _layers;

  LayerStack(this._layers);

  // Getters
  List<Layer> get layers => _layers;

  // Adds to the last
  void add(Layer layer) => _layers.add(layer);

  // Removes layer
  void delete(Layer layer) => _layers.remove(layer);

  // Move
  void move(int oldIndex, int newIndex) {
    if (oldIndex < 0 || oldIndex >= _layers.length) throw ArgumentError('oldIndex out of bounds');
    if (oldIndex == newIndex) return;
    if (oldIndex < newIndex) newIndex -= 1;
    final layer = _layers.removeAt(oldIndex);
    _layers.insert(newIndex, layer);
  }
}

class Frame {
  final String id;
  late final (int, int) _size;
  String? _currentLayerId;

  // direct access to inner LayerStack
  final LayerStack layers = LayerStack([]);

  Frame(this.id, this._size);

  // set size later
  Frame.deferredSize(this.id);

  set size((int, int) s) => _size = s;

  set currentLayerId(String layerId)=>_currentLayerId = layerId;

  (int, int) get size => _size;
  
  Layer? getCurrentLayer(){
    if(_currentLayerId==null) return null;
    for(var layer in layers.layers){
      if(layer.id == _currentLayerId){
        return layer;
      }
    }
  }

  // == equality
  @override
  bool operator ==(Object other) {
    if (identical(this, other)) return true;
    return other is Frame && other.id == id;
  }

  // hashCode
  @override
  int get hashCode => id.hashCode;

  // To Json
  Map<String, dynamic> toJson() {
    return {
      'id': id,
      'size': [size.$1, size.$2],
      'currentLayerId': _currentLayerId,
      'layers': layers.layers.map((layer) => layer.toJson()).toList(),
    };
  }

  // From Json
  factory Frame.fromJson(Map<String, dynamic> data) {
    final frame = Frame(data['id'], (data['size'][0], data['size'][1]));
    frame._currentLayerId = data['currentLayerId'];
    for (final layer in data['layers']) {
      frame.layers.add(Layer.fromJson(layer));
    }
    return frame;
  }
}
