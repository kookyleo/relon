import 'dart:ui';

class Environment {
  static final Environment _instance = Environment._();

  Environment._();

  factory Environment() {
    return _instance;
  }

  String? comfyuiService;

  String filePath = '';

  String projectName = '';

  String historyFilePath = '';

  Size currentFrameSize = const Size(0, 0);

  String currentPageStyle = 'CIS';
}
