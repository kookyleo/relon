/// 提示词工厂
String promptFactory(
    {required String prompt,
    required String style,
    required String taskType,
    String? rolePrompt}) {
  final dynamic promptDict = {
    "pencil sketch": {
      "name": "极简线条",
      "embededPrefix":
          "(Exaggerated and abstract expression),realstic,pencil sketch of",
      "embededFunc1": "full body,Standing posture",
      "embededFunc2": "(((solid background)))",
      "userInput1": r"$$",
      "userInput2": r"((($$:1.3)))",
      "negative": "nsfw,photograph"
    },
    "CIS": {
      "name": "儿童插画风",
      "embededPrefix": "CIS, masterpiece, best quality.",
      "embededFunc1": "full body,Standing posture",
      "embededFunc2": "(((solid background)))",
      "userInput1": r"$$",
      "userInput2": r"((($$:1.3)))",
      "negative": "photograph, realistic"
    },
  };

  final dict = promptDict[style];
  switch (taskType) {
    case 'normal':
      return [
        dict['embededPrefix'],
        dict['userInput1'].replaceAll(r"$$", prompt),
      ].join(',');
    case 'role':
      return [
        dict['embededPrefix'],
        dict['embededFunc1'],
        dict['userInput1'].replaceAll(r"$$", prompt),
        dict['embededFunc2']
      ].join(',');
    case 'roleAction':
      if (rolePrompt == null) return '';
      return [
        dict['embededPrefix'],
        dict['userInput2'].replaceAll(r"$$", prompt),
        dict['userInput1'].replaceAll(r"$$", rolePrompt),
        dict['embededFunc2']
      ].join(',');
    default:
      return '';
  }
}

/// 反向提示词工厂
Future<String> negativePromptFactory(String style) async {
  final dynamic promptDict = {
    "pencil sketch": {
      "name": "极简线条",
      "negative": "nsfw,photograph",
    },
    "CIS": {
      "name": "儿童插画风",
      "negative": "photograph, realistic",
    },
  };
  return promptDict[style]['negative'];
}

/// 补充 prompt: 判断输入文本中是否包含人
String textHumanExists(String input) {
  const String prefix = "判断我下面说的话有没有包含人，只回答是和否：";
  return "$prefix$input";
}

String splitTextToJson(String input, double width, double height) {
  return '''提示词模板：
## 任务
用户用自然语言告诉你要生成的图片的主题及画布尺寸、位置，你需要基于这个描述创造性地使用矩形块来构建一个基础的布局，用户可能只提供了场景描述而没有具体的实体布局位置和尺寸信息，每个实体的大小和位置都是你基于场景描述的直观理解来设定。

##要求
- 需要体现自然语言中提及的所有实体以及没有提及但应该会有的实体
- 每个实体都要输出1对1的矩形块，每个矩形块的内容为对应的实体名称
- 明确给出每个实体在画布尺寸内的 left，top，width，height 属性，无需container
- 每个实体的边界不能超出画布，即实体的 0﹤left+width的值﹤画布宽度；0﹤top+height的值﹤画布高度 
- 每个矩形的颜色不能重复
- 尺寸越大的矩形z-index值越小
- 输出的结果只能是 json 文本中的内容，无需出现任何其他文字，比如```json 等注释信息
- json 整体为一个数组，每一项为每个实体的信息，"name"对应实体名称（要有中英文两版），"left"、"top"对应位置信息，"width":、"height"对应尺寸信息，"color"对应颜色信息；

以下是用户提供的场景“${input}”，画布尺寸为${width}x${height}，现在开始构建的json：''';
}
