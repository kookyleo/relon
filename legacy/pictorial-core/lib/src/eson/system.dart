import 'dart:math';

// seed 随机值
int randomSeed(int min, int max) {
  return Random().nextInt(max - min) + min;
}

// 采样步数
int getSteps() {
  return 4;
}

// 提示词引导系数
double getCfg() {
  return 1.5;
}

// 采样器
String getSamplerName() {
  return 'dpmpp_sde';
}


String getLoraName(String style) {
  final Map<String, String> loraNameMap = {
    "pencil sketch": "pencil_sketch.safetensors",
    "CIS": "CIS_V5.safetensors",
  };
  return loraNameMap[style] ?? '';
}

