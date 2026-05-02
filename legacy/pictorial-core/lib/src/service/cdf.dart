import 'dart:typed_data';

import 'package:pictorial_core/pictorial_core.dart';
import 'package:pictorial_core/src/service/service.dart';

import '../form/field.dart';

/// CDF: ComfyUi Defined Functions

// `__SPEC_VERSION` is required for all Cdf versions
Cdf _specificCdf(Bson bson) {
  if (!bson.containsKey('__SPEC_VERSION')) {
    throw CdfException('Missing required key: `__SPEC_VERSION`, which is required for all Cdf versions');
  }
  switch (bson['__SPEC_VERSION']) {
    /// make sure to add a new case for each new version like this:
    case 1:
      return Cdf1(bson);
    default:
      throw CdfException('Invalid Cdf version: ${bson['__SPEC_VERSION']}');
  }
}

/// Cdf exception
extension type CdfException._(Exception _) implements Exception {
  CdfException(String message) : _ = Exception(message);
}

abstract class Cdf implements Service {
  // Returns key constraint information for a specific version,
  // the value is a list of tuples such as: List<String key, bool required, String? defaultKey>
  List<(String, bool, String?)> keySpecification();

  // Read and write key-value pairs in Cdf data
  dynamic value(String key);

  pub(String key, dynamic value);

  // Returns a Cdf instance from a Uint8List
  factory Cdf.fromUint8List(Uint8List data) {
    Bson bson = data.deserializeAsBson();
    Cdf cdf = _specificCdf(bson);
    for (var k in cdf.keySpecification()) {
      if (!bson.containsKey(k.$1)) {
        if (k.$2) {
          throw CdfException('Missing required key: ${k.$1}');
        } else {
          cdf.pub(k.$1, bson[k.$3]);
        }
      }
    }
    return cdf;
  }

  // Returns a Cdf instance from an asset path
  static Future<Cdf> fromAsset(String path) async {
    return Cdf.fromUint8List(await loadAssetAsUint8List(path));
  }

  // For Cdf, an additional method is required to get the information so Cdf can generate the user form form.
  //                    App                  Service
  // acquireInputProto() ├─────────────────────►│
  //                     │◄─────────────────────┤
  //           act(form) ├─────────────────────►│
  //                     │◄─────────────────────┤ Service Response
  List<UsrInputProto> acquireInputProto();
}

// Cdf${N}, where N is the number of version of Cdf specification,
// All backward compatible versions that need to be retained !
class Cdf1 implements Cdf {
  late Map<String, dynamic> _data;

  Cdf1(this._data);

  @override
  List<(String, bool, String?)> keySpecification() {
    return [
      ('brief', true, null),
      ('brief_zh', false, 'brief'),
      ('brief_ja', false, 'brief'),
      ('author', true, null),
      ('ver', true, null),
      ('icon', true, null),
      ('workflow_tpl', true, null),
    ];
  }

  @override
  pub(String key, value) {
    this._data[key] = value;
  }

  @override
  value(String key) {
    return this._data[key];
  }

  @override
  SvcOutput? act([SvcInput? input]) {
    // TODO: implement act
    throw UnimplementedError();
  }

  @override
  List<UsrInputProto> acquireInputProto() {
    // TODO: implement acquireInputProto
    throw UnimplementedError();
  }
}
