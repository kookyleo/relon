class Annotation {
  final String id;

  Annotation(this.id);

  @override
  bool operator ==(Object other) {
    if (identical(this, other)) return true;
    return other is Annotation && other.id == id;
  }

  @override
  int get hashCode => id.hashCode;

  // To Json
  Map<String, dynamic> toJson() {
    return {
      'id': id,
    };
  }

  // From Json
  factory Annotation.fromJson(Map<String, dynamic> data) {
    return Annotation(data['id']);
  }
}
