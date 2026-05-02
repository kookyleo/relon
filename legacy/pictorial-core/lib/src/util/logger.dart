import 'package:logger/logger.dart';

// A global syntactic sugar wrapper class for logger

// alias of Logger.level
typedef LogLevel = Level;

var _logger = Logger(
  printer: PrettyPrinter(stackTraceBeginIndex: 1),
);

var _loggerWithPosition = Logger(
  printer: PrettyPrinter(methodCount: 2, stackTraceBeginIndex: 1),
);

var _loggerWithoutStack = Logger(
  printer: PrettyPrinter(methodCount: 0),
);

// trace(1000),
// debug(2000),
// info(3000),
// warning(4000),
// error(5000),
// fatal(6000),

/// trace is mainly used to record the detailed path of program execution.
trace(String message) {
  _logger.t(message);
}

/// debug is mainly used in the development and debugging phase to provide detailed debugging information.
debug(String message) {
  _loggerWithPosition.d(message);
}

/// info is used to record the general information in the running process of the program, which can help developers understand
/// the running status and execution of the program.
/// info messages are often used to provide useful context, such as the start and stop time of a program, the loading of
/// important configuration parameters, and so on.
info(String message) {
  _loggerWithoutStack.i(message);
}

/// warn level messages are typically used to alert developers to potential risks or actions that do not conform to best practices.
warn(String message) {
  _loggerWithoutStack.w(message);
}

/// it indicates that a serious problem occurs during the running of the program and that the program cannot continue to run.
error(String message) {
  _logger.e(message);
}

/// fatal errors are the most serious types of errors.
/// it indicates that the program has encountered a problem that cannot be recovered and must be terminated.
fetal(String message) {
  _logger.f(message);
}

/// Another form
class Log {
  static void trace(String message) => _logger.t(message);

  static void debug(String message) => _loggerWithPosition.d(message);

  static void info(String message) => _loggerWithoutStack.i(message);

  static void warn(String message) => _loggerWithoutStack.w(message);

  static void error(String message) => _logger.e(message);

  static void fatal(String message) => _logger.f(message);

  static Level get minimumOutputLevel => Logger.level;

  static set minimumOutputLevel(Level level) => Logger.level = level;
}
