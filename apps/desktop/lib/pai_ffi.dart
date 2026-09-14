/// dart:ffi bindings to the Rust core (`libpai_ffi`).
///
/// Convention: JSON in, JSON out; every returned pointer must be freed with
/// `pai_free_string`. `paiSend`/`paiResume` block — always call them off the
/// UI isolate (see [PaiClient.send]). Live run events arrive through the
/// `NativeCallable` registered via `pai_set_event_callback`.
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

/// `void cb(const char* json_event, void* user_data)`
typedef NativeEventCallback = Void Function(Pointer<Utf8>, Pointer<Void>);
typedef _SetEventCbNative = Void Function(
    Pointer<Void>, Pointer<NativeFunction<NativeEventCallback>>, Pointer<Void>);
typedef _ApproveNative = Int32 Function(Pointer<Void>, Pointer<Utf8>, Int32);
typedef _ThreeStrNative = Pointer<Utf8> Function(
    Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>);
typedef _CancelNative = Void Function(Pointer<Void>);
typedef _DetectNative = Pointer<Utf8> Function();

class PaiClient {
  PaiClient._(this._lib, this._handle);

  final DynamicLibrary _lib;
  final Pointer<Void> _handle;

  late final _init =
      _lib.lookupFunction<_InitNative, Pointer<Void> Function(Pointer<Utf8>)>(
          'pai_init');
  late final _send = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_send');
  late final _resume = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_resume');
  late final _audit = _lib.lookupFunction<_AuditNative,
      Pointer<Utf8> Function(Pointer<Void>, int)>('pai_audit');
  late final _memories = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_memories');
  late final _runs = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_runs');
  late final _conversations = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_conversations');
  late final _history = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_history');
  late final _policies = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_policies');
  late final _convNew = _lib.lookupFunction<_SendNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>(
      'pai_conversation_new');
  late final _convSelect = _lib.lookupFunction<_SendNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>(
      'pai_conversation_select');
  late final _convDelete = _lib.lookupFunction<_SendNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>(
      'pai_conversation_delete');
  late final _forget = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_forget');
  late final _convRename = _lib.lookupFunction<_ThreeStrNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>)>(
      'pai_conversation_rename');
  late final _convSetMemory = _lib.lookupFunction<_ThreeStrNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>)>(
      'pai_conversation_set_memory');
  late final _docs = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_docs');
  late final _docsIngest = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_docs_ingest');
  late final _docsSearch = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_docs_search');
  late final _docsDelete = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_docs_delete');
  late final _emailSearch = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_email_search');
  late final _emailRead = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_email_read');
  late final _emailDraft = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_email_draft');
  late final _setPolicy = _lib.lookupFunction<_ThreeStrNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>)>(
      'pai_set_policy');
  late final _approve = _lib.lookupFunction<_ApproveNative,
      int Function(Pointer<Void>, Pointer<Utf8>, int)>('pai_approve');
  late final _cancel =
      _lib.lookupFunction<_CancelNative, void Function(Pointer<Void>)>(
          'pai_cancel');
  late final _setEventCb = _lib.lookupFunction<
      _SetEventCbNative,
      void Function(
          Pointer<Void>,
          Pointer<NativeFunction<NativeEventCallback>>,
          Pointer<Void>)>('pai_set_event_callback');
  late final _detect =
      _lib.lookupFunction<_DetectNative, Pointer<Utf8> Function()>(
          'pai_detect');
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
      // Windows / macOS dev layouts.
      '../../target/debug/pai_ffi.dll',
      '../../target/release/pai_ffi.dll',
      '../../target/debug/libpai_ffi.dylib',
      '../../target/release/libpai_ffi.dylib',
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

  dynamic _json(Pointer<Utf8> out) {
    final result = jsonDecode(out.toDartString());
    _freeString(out);
    return result;
  }

  /// Blocking call — run inside a worker isolate, never on the UI thread.
  Map<String, dynamic> send(String text) {
    final m = text.toNativeUtf8();
    final out = _send(_handle, m);
    calloc.free(m);
    return _json(out) as Map<String, dynamic>;
  }

  /// Resume an interrupted run. Blocking — worker isolate only.
  Map<String, dynamic> resume(String runId) {
    final m = runId.toNativeUtf8();
    final out = _resume(_handle, m);
    calloc.free(m);
    return _json(out) as Map<String, dynamic>;
  }

  /// Resolve a pending approval. Non-blocking; safe from any thread.
  bool approve(String callId, bool granted) {
    final m = callId.toNativeUtf8();
    final r = _approve(_handle, m, granted ? 1 : 0);
    calloc.free(m);
    return r != 0;
  }

  /// Cancel the in-flight run.
  void cancel() => _cancel(_handle);

  /// Register the live-event sink. [callback] must be a
  /// `NativeCallable.isolateLocal` — it is invoked synchronously on the
  /// thread running [send].
  void setEventCallback(
      Pointer<NativeFunction<NativeEventCallback>> callback) {
    _setEventCb(_handle, callback, nullptr);
  }

  /// Live endpoints + provider binaries found on this machine.
  Map<String, dynamic> detect() =>
      _json(_detect()) as Map<String, dynamic>;

  List<dynamic> audit({int limit = 50}) => _json(_audit(_handle, limit)) as List<dynamic>;

  List<dynamic> memories() => _json(_memories(_handle)) as List<dynamic>;

  List<dynamic> runs() => _json(_runs(_handle)) as List<dynamic>;

  List<dynamic> conversations() => _json(_conversations(_handle)) as List<dynamic>;

  List<dynamic> history() => _json(_history(_handle)) as List<dynamic>;

  List<dynamic> policies() => _json(_policies(_handle)) as List<dynamic>;

  Map<String, dynamic> conversationNew({bool isolated = false}) {
    final c = jsonEncode({'memory': isolated ? 'isolated' : 'shared'})
        .toNativeUtf8();
    final out = _convNew(_handle, c);
    calloc.free(c);
    return _json(out) as Map<String, dynamic>;
  }

  Map<String, dynamic> conversationSelect(String id) => _call1(_convSelect, id);
  Map<String, dynamic> conversationDelete(String id) => _call1(_convDelete, id);
  Map<String, dynamic> forget(String memoryId) => _call1(_forget, memoryId);

  Map<String, dynamic> conversationRename(String id, String title) =>
      _call2(_convRename, id, title);
  Map<String, dynamic> conversationSetMemory(String id, String mode) =>
      _call2(_convSetMemory, id, mode);
  Map<String, dynamic> setPolicy(String permission, String policy) =>
      _call2(_setPolicy, permission, policy);

  /// Ingested documents: [{id, title, mime, created_at, sections}].
  List<dynamic> docs() => _json(_docs(_handle)) as List<dynamic>;

  /// Ingest a file by absolute path. Blocking — worker isolate only.
  Map<String, dynamic> docsIngest(String path) => _call1(_docsIngest, path);

  /// Hybrid search over document sections: [{document, title, section,
  /// snippet, score}].
  List<dynamic> docsSearch(String query) {
    final q = query.toNativeUtf8();
    final out = _docsSearch(_handle, q);
    calloc.free(q);
    return _json(out) as List<dynamic>;
  }

  Map<String, dynamic> docsDelete(String id) => _call1(_docsDelete, id);

  /// Search the mailbox: {query, from, label, unread_only, limit}.
  Map<String, dynamic> emailSearch(String? queryJson) =>
      _callOpt(_emailSearch, queryJson);

  /// Read one message by id.
  Map<String, dynamic> emailRead(String id) => _call1(_emailRead, id);

  /// Create a draft: {to:[{address}], cc:[], subject, body, in_reply_to?}.
  Map<String, dynamic> emailDraft(String draftJson) =>
      _call1(_emailDraft, draftJson);

  Map<String, dynamic> _callOpt(
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>) f, String? a) {
    if (a == null) {
      final out = f(_handle, nullptr);
      return _json(out) as Map<String, dynamic>;
    }
    return _call1(f, a);
  }

  Map<String, dynamic> _call1(
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>) f, String a) {
    final pa = a.toNativeUtf8();
    final out = f(_handle, pa);
    calloc.free(pa);
    return _json(out) as Map<String, dynamic>;
  }

  Map<String, dynamic> _call2(
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>) f,
      String a,
      String b) {
    final pa = a.toNativeUtf8();
    final pb = b.toNativeUtf8();
    final out = f(_handle, pa, pb);
    calloc.free(pa);
    calloc.free(pb);
    return _json(out) as Map<String, dynamic>;
  }

  void dispose() => _free(_handle);
}
