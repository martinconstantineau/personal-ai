/// dart:ffi bindings to the Rust core (`libpai_ffi`).
///
/// Convention: JSON in, JSON out; every returned pointer must be freed with
/// `pai_free_string`. `paiSend` blocks — always call it off the UI isolate
/// (see [PaiClient.send]).
library;

import 'dart:convert';
import 'dart:ffi';
import 'package:ffi/ffi.dart';

typedef _InitNative = Pointer<Void> Function(Pointer<Utf8>);
typedef _SendNative = Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>);
typedef _AuditNative = Pointer<Utf8> Function(Pointer<Void>, Uint32);
typedef _NoArgNative = Pointer<Utf8> Function(Pointer<Void>);
typedef _FreeStrNative = Void Function(Pointer<Utf8>);
typedef _FreeNative = Void Function(Pointer<Void>);

class PaiClient {
  PaiClient._(this._lib, this._handle);

  final DynamicLibrary _lib;
  final Pointer<Void> _handle;

  late final _init =
      _lib.lookupFunction<_InitNative, Pointer<Void> Function(Pointer<Utf8>)>(
          'pai_init');
  late final _send = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_send');
  late final _audit = _lib.lookupFunction<_AuditNative,
      Pointer<Utf8> Function(Pointer<Void>, int)>('pai_audit');
  late final _memories = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_memories');
  late final _freeString =
      _lib.lookupFunction<_FreeStrNative, void Function(Pointer<Utf8>)>(
          'pai_free_string');
  late final _free =
      _lib.lookupFunction<_FreeNative, void Function(Pointer<Void>)>(
          'pai_free');

  static DynamicLibrary _open() {
    // Bundled location (linux build) then dev checkout layout.
    for (final candidate in [
      'lib/libpai_ffi.so',
      'libpai_ffi.so',
      '../../target/debug/libpai_ffi.so',
      '../../target/release/libpai_ffi.so',
    ]) {
      try {
        return DynamicLibrary.open(candidate);
      } catch (_) {}
    }
    // Last resort: let the loader search standard paths.
    return DynamicLibrary.open('libpai_ffi.so');
  }

  /// Create a runtime. [config] is `{"data_dir": ..., "provider": ...}`.
  static PaiClient init(Map<String, dynamic> config) {
    final lib = _open();
    final client = PaiClient._(lib, Pointer<Void>.fromAddress(0));
    final c = jsonEncode(config).toNativeUtf8();
    final handle = client._init(c);
    calloc.free(c);
    if (handle.address == 0) {
      throw StateError('pai_init failed — check data_dir is writable');
    }
    return PaiClient._(lib, handle);
  }

  /// Blocking call — run inside a worker isolate, never on the UI thread.
  Map<String, dynamic> send(String text) {
    final m = text.toNativeUtf8();
    final out = _send(_handle, m);
    calloc.free(m);
    final result = jsonDecode(out.toDartString()) as Map<String, dynamic>;
    _freeString(out);
    return result;
  }

  List<dynamic> audit({int limit = 50}) {
    final out = _audit(_handle, limit);
    final result = jsonDecode(out.toDartString()) as List<dynamic>;
    _freeString(out);
    return result;
  }

  List<dynamic> memories() {
    final out = _memories(_handle);
    final result = jsonDecode(out.toDartString()) as List<dynamic>;
    _freeString(out);
    return result;
  }

  void dispose() => _free(_handle);
}
