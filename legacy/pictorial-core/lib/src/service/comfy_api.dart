import 'dart:async';
import 'dart:collection';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:pictorial_core/src/util/public_box.dart';
import 'package:web_socket_channel/io.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import '../../pictorial_core.dart';

typedef Json = Map<String, dynamic>;

class ViewRequest {
  final String filename;
  final String type;
  final String subfolder;
  final String t;

  ViewRequest(this.filename, this.type, this.subfolder, this.t);

  ViewRequest.fromJson(Json data)
      : filename = data['filename'],
        type = data['type'],
        subfolder = data['subfolder'],
        t = DateTime.now().millisecondsSinceEpoch.toString();

  String path() {
    return '/view?filename=$filename&type=$type&subfolder=$subfolder&t=$t';
  }
}

class ComfyApi {
  late String sevScheme;
  late String sevHost;
  late int sevPort;
  late String clientId;

  ComfyApi(String serverUrl) {
    Uri uri = Uri.parse(serverUrl);
    sevScheme = uri.scheme;
    if (sevScheme.startsWith('ws')) {
      sevScheme = uri.scheme == 'wss' ? 'https' : 'http';
    } else if (!sevScheme.startsWith('http')) {
      throw Exception('Unsupported schema: $sevScheme');
    }
    sevHost = uri.host;
    sevPort = uri.port;
  }

  String get serverUrl => '$sevScheme://$sevHost:$sevPort';

  // GET /api/system_stats HTTP/1.1
  Future<HttpClientResponse> getSystemStats() async {
    return await get('/api/system_stats');
  }

  // GET /view?filename=... HTTP/1.1
  Future<Uint8List?> getView(ViewRequest vr) async {
    HttpClientResponse response = await get(
        '/view?filename=${vr.filename}&type=${vr.type}&subfolder=${vr.subfolder}&t=${vr.t}');
    if (response.statusCode == 200) {
      return await consolidateHttpClientResponseBytes(response);
    } else {
      return null;
    }
  }

  // POST /api/prompt HTTP/1.1
  Future<Json> postPrompt(String data) async {
    clientId = await sessionId();

    HashMap<String, dynamic> cmd = HashMap();
    cmd['prompt'] = jsonDecode(data);
    cmd['client_id'] = clientId;
    HttpClientResponse resp = await post('/api/prompt', jsonEncode(cmd));
    String r = await resp.transform(utf8.decoder).join();
    return jsonDecode(r);
  }

  /// TODO 
  /// 1. header-token 动态传递
  /// 2. 此为获取 comfy_api 服务的地址，暂时先放这里
  Future<Json> getService() async {
    HttpClientResponse resp =
        await get('/api/v1/services', headers: {'token': '123456'});
    String r = await resp.transform(utf8.decoder).join();
    return jsonDecode(r);
  }

  Future<HttpClientResponse> get(String path,
      {Map<String, String>? headers}) async {
    final HttpClientRequest request =
        await HttpClient().getUrl(Uri.parse('$serverUrl$path'));
    if (headers != null) {
      headers.forEach((key, value) {
        request.headers.set(key, value);
      });
    }
    return request.close();
  }

  Future<HttpClientResponse> post(String path, String data,
      {Map<String, String>? headers}) async {
    final HttpClientRequest request =
        await HttpClient().postUrl(Uri.parse('$serverUrl$path'));
    if (headers != null) {
      headers.forEach((key, value) {
        request.headers.set(key, value);
      });
    }

    request.add(utf8.encode(data));
    return request.close();
  }
}

enum WebSocketChannelState { connected, notConnected }

class WebSocketProvider extends ChangeNotifier {
  WebSocketChannel? _channel;
  WebSocketChannelState _state = WebSocketChannelState.notConnected;
  Map<String, dynamic> _message = {};

  WebSocketChannelState get state => _state;

  Map<String, dynamic> get message => _message;

  void connect(String channelAddress) async {
    // server url
    PublicBox.set('servUrl', channelAddress);

    // session id
    var clientId = await sessionId();

    Uri uri = Uri.parse(channelAddress);
    String schema = uri.scheme;
    if (schema.startsWith('http')) {
      schema = uri.scheme == 'https' ? 'wss' : 'ws';
    } else if (schema.startsWith('ws')) {
      schema = uri.scheme;
    } else {
      throw Exception('Unsupported schema: $schema');
    }
    channelAddress = '$schema://${uri.host}:${uri.port}/ws?clientId=$clientId';

    try {
      _channel = IOWebSocketChannel.connect(channelAddress);
      _state = WebSocketChannelState.connected;
      notifyListeners();

      _channel?.stream.listen((message) {
        _message = json.decode(message);
        notifyListeners();
      }, onError: (error) {
        _state = WebSocketChannelState.notConnected;
        notifyListeners();
      }, onDone: () {
        _state = WebSocketChannelState.notConnected;
        notifyListeners();
      });

      // keep alive
      Timer.periodic(const Duration(minutes: 1), (timer) {
        if (_channel?.sink != null) {
          _channel?.sink.add(jsonEncode({'type': 'ping'}));
        } else {
          timer.cancel();
        }
      });
    } catch (e) {
      _state = WebSocketChannelState.notConnected;
      notifyListeners();
    }
  }

  @override
  void dispose() {
    _channel?.sink.close();
    super.dispose();
  }
}

Future<String> sessionId() async {
  return await PublicBox.smartGet('sessionId', () => uuid(false));
}
