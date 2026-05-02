import 'dart:io';

// 非空
bool validatorNullable(String input, bool value) {
  if (!value) {
    return input.isNotEmpty;
  }
  return true;
}

// 长度限制
bool validatorTextLength(String input, int min, int max) {
  return input.length >= min && input.length <= max;
}

// 文件格式
bool validatorFileFormat(File file, List<String> format) {
  String path = file.path;
  for (String item in format) {
    if (path.endsWith(item)) {
      return true;
    }
  }
  return false;
}

// 文件大小
Future<bool> validatorFileSize(File file, int min, int size) async {
  int length = await file.length();
  return length >= min && length <= size;
}

// 数字大小
bool validatorDenoise(double num, double min, double max) {
  return num >= min && num <= max;
}
