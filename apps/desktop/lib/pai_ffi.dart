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
typedef _VoiceListenNative = Pointer<Utf8> Function(Pointer<Void>, Uint32);
typedef _NotifyListNative = Pointer<Utf8> Function(Pointer<Void>, Uint8);
typedef _ThreeStrNative = Pointer<Utf8> Function(
    Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>);
typedef _ShareGrantNative = Pointer<Utf8> Function(
    Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>, Int64, Pointer<Utf8>);
typedef _CancelNative = Void Function(Pointer<Void>);
typedef _DetectNative = Pointer<Utf8> Function();
typedef _StrIntNative = Pointer<Utf8> Function(
    Pointer<Void>, Pointer<Utf8>, Int32);

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
  late final _emailSend = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_email_send');
  late final _status = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_status');
  late final _modelsList = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_models_list');
  late final _modelsScan = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_models_scan');
  late final _modelsServe = _lib.lookupFunction<_StrIntNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, int)>(
      'pai_models_serve');
  late final _setProvider = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_set_provider');
  late final _voiceStatus = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_voice_status');
  late final _voiceListen = _lib.lookupFunction<_VoiceListenNative,
      Pointer<Utf8> Function(Pointer<Void>, int)>('pai_voice_listen');
  late final _voiceListenStream = _lib.lookupFunction<_VoiceListenNative,
      Pointer<Utf8> Function(Pointer<Void>, int)>('pai_voice_listen_stream');
  late final _voiceTranscribe = _lib.lookupFunction<_SendNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>(
      'pai_voice_transcribe');
  late final _notifyList = _lib.lookupFunction<_NotifyListNative,
      Pointer<Utf8> Function(Pointer<Void>, int)>('pai_notify_list');
  late final _notifyMarkRead = _lib.lookupFunction<_SendNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>(
      'pai_notify_mark_read');
  late final _appsList = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_apps_list');
  late final _appsRun = _lib.lookupFunction<_ThreeStrNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>)>(
      'pai_apps_run');
  late final _appsMigrate = _lib.lookupFunction<_ThreeStrNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>)>(
      'pai_apps_migrate');
  late final _peersList = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_peers_list');
  late final _shareGrant = _lib.lookupFunction<_ShareGrantNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>, int,
          Pointer<Utf8>)>('pai_share_grant');
  late final _shareDelegate = _lib.lookupFunction<_ShareGrantNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>, int,
          Pointer<Utf8>)>('pai_share_delegate');
  late final _shareList = _lib.lookupFunction<_NoArgNative,
      Pointer<Utf8> Function(Pointer<Void>)>('pai_share_list');
  late final _shareRevoke = _lib.lookupFunction<_SendNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>(
      'pai_share_revoke');
  late final _guestCall = _lib.lookupFunction<_SendNative,
          Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>(
      'pai_guest_call');
  late final _voiceSay = _lib.lookupFunction<_SendNative,
      Pointer<Utf8> Function(Pointer<Void>, Pointer<Utf8>)>('pai_voice_say');
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
      // Bundled with the packaged app (next to the exe / jniLibs).
      'pai_ffi.dll',
      'libpai_ffi.so',
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

  /// Send a draft immediately via SMTP: same JSON shape as [emailDraft].
  /// {sent: true} or {error: ...} when no smtp block is configured.
  Map<String, dynamic> emailSend(String draftJson) =>
      _call1(_emailSend, draftJson);

  /// Notification inbox: {notifications: [...], unread: n}. Rows sync
  /// across paired devices.
  Map<String, dynamic> notifyList({bool unreadOnly = false}) => _json(
      _notifyList(_handle, unreadOnly ? 1 : 0)) as Map<String, dynamic>;

  /// Mark a notification read — propagates to peers on sync.
  /// Returns {ok: bool}.
  Map<String, dynamic> notifyMarkRead(String id) =>
      _call1(_notifyMarkRead, id);

  /// Installed app packages: {apps: [{id, name, version, runtime}]}.
  /// Powers the Personal App Cloud dashboard.
  Map<String, dynamic> appsList() =>
      _json(_appsList(_handle)) as Map<String, dynamic>;

  /// Run an installed app in the wasmi sandbox. Blocking — worker
  /// isolate only. Returns {stdout, stderr, exit_code, fuel} or
  /// {error}.
  Map<String, dynamic> appsRun(String id, {List<String> args = const []}) =>
      _call2(_appsRun, id, jsonEncode(args));

  /// Migrate an app to a paired device (device-id prefix). Ships on
  /// the next sync push; the target restores on pull. {ok, pak} or
  /// {error}. Blocking — worker isolate only.
  Map<String, dynamic> appsMigrate(String id, String to) =>
      _call2(_appsMigrate, id, to);

  /// Paired peer devices: {peers: [{id, name, platform}]}.
  Map<String, dynamic> peersList() =>
      _json(_peersList(_handle)) as Map<String, dynamic>;

  /// Mint a capability token for [id] — the guest's credential.
  /// [actions] like `exec,read`; [forDevice] (a paired-peer prefix or
  /// empty) binds the grant to that peer's key.
  Map<String, dynamic> shareGrant(
      String id, String actions, int days, String forDevice) {
    final pa = id.toNativeUtf8();
    final pb = actions.toNativeUtf8();
    final pc = forDevice.toNativeUtf8();
    final out = _shareGrant(_handle, pa, pb, days, pc);
    calloc.free(pa);
    calloc.free(pb);
    calloc.free(pc);
    return _json(out) as Map<String, dynamic>;
  }

  /// Re-grant a narrower sub-token from a held parent token — the
  /// parent must carry `share` and be bound to this device's key.
  /// [forKey] is a paired-peer prefix or 64-hex pubkey ('' = bearer).
  Map<String, dynamic> shareDelegate(
      String parentJson, String actions, int days, String forKey) {
    final pa = parentJson.toNativeUtf8();
    final pb = actions.toNativeUtf8();
    final pc = forKey.toNativeUtf8();
    final out = _shareDelegate(_handle, pa, pb, days, pc);
    calloc.free(pa);
    calloc.free(pb);
    calloc.free(pc);
    return _json(out) as Map<String, dynamic>;
  }

  Map<String, dynamic> shareList() =>
      _json(_shareList(_handle)) as Map<String, dynamic>;

  Map<String, dynamic> shareRevoke(String tokenId) =>
      _call1(_shareRevoke, tokenId);

  /// Guest-side capability call — runs an op on the token's host.
  /// [request] fields: op, app_id, args, token (JSON), to, dir/relay.
  Map<String, dynamic> guestCall(String request) =>
      _call1(_guestCall, request);

  /// Resolved runtime status: {provider, model, device, data_dir}.
  Map<String, dynamic> status() =>
      _json(_status(_handle)) as Map<String, dynamic>;

  /// Re-point chat: {server_url?, model?} → {provider, model}.
  Map<String, dynamic> setProvider(String request) =>
      _call1(_setProvider, request);

  /// Known/installed models with pack status:
  /// [{slug, family, quant, size_mb, capabilities, installed, online, path, serving}].
  List<dynamic> modelsList() =>
      _json(_modelsList(_handle)) as List<dynamic>;

  /// Rescan pai-models/ pack roots (newly plugged drives count), then list.
  List<dynamic> modelsScan() =>
      _json(_modelsScan(_handle)) as List<dynamic>;

  /// Spawn llama-server for [slug] on [port] and point chat at it.
  Map<String, dynamic> modelsServe(String slug, int port) {
    final s = slug.toNativeUtf8();
    try {
      return _json(_modelsServe(_handle, s, port)) as Map<String, dynamic>;
    } finally {
      malloc.free(s);
    }
  }

  /// Voice capability probe: {stt, tts, mic, speaker, whisper_url}.
  Map<String, dynamic> voiceStatus() =>
      _json(_voiceStatus(_handle)) as Map<String, dynamic>;

  /// Capture + transcribe one utterance. Blocking (up to [maxSecs]) —
  /// worker isolate only. Returns {heard, text?, wav_b64?}.
  Map<String, dynamic> voiceListen({int maxSecs = 30}) =>
      _json(_voiceListen(_handle, maxSecs)) as Map<String, dynamic>;

  /// Streaming listen — per-segment partials collected into the result:
  /// {heard, text, partials[]}. Blocking — worker isolate only.
  Map<String, dynamic> voiceListenStream({int maxSecs = 30}) =>
      _json(_voiceListenStream(_handle, maxSecs)) as Map<String, dynamic>;

  /// Transcribe a WAV file. Blocking — worker isolate only.
  Map<String, dynamic> voiceTranscribe(String path) =>
      _call1(_voiceTranscribe, path);

  /// Speak text through the host speaker. Blocking — worker isolate only.
  /// {ok, played} or {ok, played:false, wav_b64} when playback failed.
  Map<String, dynamic> voiceSay(String text) => _call1(_voiceSay, text);

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
