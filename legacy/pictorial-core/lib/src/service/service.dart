library;

/// Totally, there are two types of services, Built-in services and Cdf services.
/// Built-in services are services that are provided by the Pictorial project itself, they are written in Dart.
/// Cdf means ComfyUi Defined Functions, and the Cdf service is a service that wraps a ComfyUi workflow carried by a bson file.

abstract class Service {
  static final Map<String, Service Function()> _services = {};

  static void registerService(String key, Service Function() service) {
    _services[key] = service;
  }

  factory Service(String key) {
    if (_services.containsKey(key)) {
      return _services[key]!();
    }
    throw Exception('Service not found');
  }

  // The main function of a service
  SvcOutput? act([SvcInput? input]);
}

abstract class SvcInput {}

abstract class SvcOutput {}
