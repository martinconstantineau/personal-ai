/// Isolate bridge: `pai_send` blocks on inference, so all FFI work happens
/// inside a dedicated worker isolate that owns the runtime handle. Live run
/// events (token deltas, approval requests, tool progress) stream back
/// through the reply port while a send is in flight.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:isolate';
import 'package:ffi/ffi.dart';
import 'pai_ffi.dart';

/// Handle for one in-flight `send`: live [events] then the final [result].
class SendHandle {
  SendHandle(this.events, this.result);
  final Stream<Map<String, dynamic>> events;
  final Future<Map<String, dynamic>> result;
}

class PaiBridge {
  PaiBridge._(this._requests);

  final SendPort _requests;
  int _nextId = 0;
  final _pending = <int, _Pending>{};

  /// Latest resolved provider/model status — refreshed by [status]
  /// and [setProvider]. [statusStream] pushes updates so the chat
  /// header can react live.
  Map<String, dynamic> lastStatus = const {};
  final _statusCtl = StreamController<Map<String, dynamic>>.broadcast();
  Stream<Map<String, dynamic>> get statusStream => _statusCtl.stream;

  /// Spawns the worker isolate and initializes the Rust runtime inside it.
  static Future<PaiBridge> start(Map<String, dynamic> config) async {
    final ready = ReceivePort();
    await Isolate.spawn(_workerMain, (ready.sendPort, config));
    final handshake = await ready.first;
    if (handshake is _InitError) {
      throw StateError(handshake.message);
    }
    final requests = handshake as SendPort;
    final bridge = PaiBridge._(requests);
    // Route replies back by id; `_event` payloads go to the event stream.
    final replies = ReceivePort();
    requests.send(_Subscribe(replies.sendPort));
    replies.listen((msg) {
      final (id, value) = msg as (int, dynamic);
      final p = bridge._pending[id];
      if (p == null) return;
      if (value is Map && value['_event'] != null) {
        final decoded = jsonDecode(value['_event'] as String);
        if (decoded is Map<String, dynamic>) p.events?.add(decoded);
      } else {
        bridge._pending.remove(id);
        p.completer.complete(value);
        p.events?.close();
      }
    });
    return bridge;
  }

  /// Send a message; live AgentEvents arrive on `events` while `result`
  /// completes with the final `{answer, events, run_id, ...}` payload.
  SendHandle sendStreaming(String text) {
    final events = StreamController<Map<String, dynamic>>();
    final result =
        _call(_Op.send, arg: text, events: events).then((v) => v as Map<String, dynamic>);
    return SendHandle(events.stream, result);
  }

  /// Back-compatible: wait for the run to finish, return the result map.
  Future<Map<String, dynamic>> send(String text) =>
      sendStreaming(text).result;

  Future<Map<String, dynamic>> resume(String runId) =>
      _call(_Op.resume, arg: runId).then((v) => v as Map<String, dynamic>);

  /// Resolve a pending approval (tool_call_id → allow/deny).
  Future<bool> approve(String callId, bool granted) async =>
      (await _call(_Op.approve,
          arg: jsonEncode({'id': callId, 'granted': granted}))) as bool;

  /// Cancel the in-flight run.
  Future<void> cancel() => _call(_Op.cancel).then((_) {});

  Future<List<dynamic>> memories() async =>
      (await _call(_Op.memories)) as List<dynamic>;
  Future<List<dynamic>> audit() async => (await _call(_Op.audit)) as List<dynamic>;
  Future<List<dynamic>> runs() async => (await _call(_Op.runs)) as List<dynamic>;
  Future<List<dynamic>> conversations() async =>
      (await _call(_Op.conversations)) as List<dynamic>;
  Future<List<dynamic>> history() async =>
      (await _call(_Op.history)) as List<dynamic>;
  Future<List<dynamic>> policies() async =>
      (await _call(_Op.policies)) as List<dynamic>;
  Future<Map<String, dynamic>> detect() async =>
      (await _call(_Op.detect)) as Map<String, dynamic>;

  /// Installed/pack models with online+serving status.
  Future<List<dynamic>> modelsList() async =>
      (await _call(_Op.modelsList)) as List<dynamic>;

  /// Rescan pai-models/ pack roots (freshly plugged drives), then list.
  Future<List<dynamic>> modelsScan() async =>
      (await _call(_Op.modelsScan)) as List<dynamic>;

  /// Serve a model via llama-server, then refresh shared status so the
  /// header + health dot follow.
  Future<Map<String, dynamic>> modelsServe(String slug,
      {int port = 0}) async {
    final r = (await _call(_Op.modelsServe,
            arg: jsonEncode({'slug': slug, 'port': port})))
        as Map<String, dynamic>;
    if (r['error'] == null) {
      try {
        await status();
      } catch (_) {}
    }
    return r;
  }

  /// The installable catalog (slug → manifest resolution happens
  /// core-side; hf:// refs work too).
  Future<List<dynamic>> modelsCatalog() async =>
      (await _call(_Op.modelsCatalog)) as List<dynamic>;

  /// Install a model — `dest` empty installs internally; a path like
  /// `D:\pai-models` writes a portable pack (copies if already on disk).
  /// Long-running — runs on the worker isolate.
  Future<Map<String, dynamic>> modelsInstall(String slug,
      {String dest = ''}) async =>
      (await _call(_Op.modelsInstall,
          arg: jsonEncode({'slug': slug, 'dest_dir': dest})))
          as Map<String, dynamic>;

  /// Media job log — newest first (local + broker-routed rows).
  Future<List<dynamic>> mediaList() async =>
      (await _call(_Op.mediaList)) as List<dynamic>;

  /// Generate audio locally. `json`: `{prompt, duration_seconds?}`.
  /// Long-running — runs on the worker isolate.
  Future<Map<String, dynamic>> mediaGen(String prompt,
      {int seconds = 10}) async =>
      (await _call(_Op.mediaGen,
          arg: jsonEncode(
              {'prompt': prompt, 'duration_seconds': seconds})))
          as Map<String, dynamic>;

  /// One-shot sync — `lan` discovers a paired mesh peer; `dir`/`relay`
  /// targets persist under `sync.*` meta so later calls need no args.
  /// `autoMinutes` (0 = off) schedules background syncs on the target.
  Future<Map<String, dynamic>> syncNow(
      {String mode = 'run',
      String? dir,
      String? relay,
      String? token,
      bool lan = false,
      int? autoMinutes}) async =>
      (await _call(_Op.syncNow,
          arg: jsonEncode({
            'mode': mode,
            'dir': ?dir,
            'relay': ?relay,
            'token': ?token,
            'lan': lan,
            'auto_minutes': ?autoMinutes,
          })))
          as Map<String, dynamic>;

  /// Persisted sync config + readiness — {lan, dir, relay, token_set,
  /// auto_minutes, last_auto, peers, has_vault}.
  Future<Map<String, dynamic>> syncStatus() async =>
      (await _call(_Op.syncStatus)) as Map<String, dynamic>;

  /// Pairing step 1 (this device offers): write offer.pai to [out].
  Future<Map<String, dynamic>> pairOffer(String out) async =>
      (await _call(_Op.pairOffer, arg: out)) as Map<String, dynamic>;

  /// Pairing step 2 (the other device accepts): offer file in, sealed
  /// accept file out.
  Future<Map<String, dynamic>> pairAccept(
          String offer, String out) async =>
      (await _call(_Op.pairAccept,
          arg: jsonEncode({'offer': offer, 'out': out})))
          as Map<String, dynamic>;

  /// Pairing step 3 (offerer completes): adopt the vault key.
  Future<Map<String, dynamic>> pairComplete(String accept) async =>
      (await _call(_Op.pairComplete, arg: accept)) as Map<String, dynamic>;

  /// Pairing exchange through the configured shared sync folder —
  /// publishes our offer, accepts pending offers, completes accepts.
  Future<Map<String, dynamic>> pairFolder() async =>
      (await _call(_Op.pairFolder)) as Map<String, dynamic>;

  /// Write a finished job's result blob to `dest`.
  Future<Map<String, dynamic>> mediaExport(String jobId, String dest) async =>
      (await _call(_Op.mediaExport,
          arg: jsonEncode({'id': jobId, 'dest': dest})))
          as Map<String, dynamic>;

  Future<Map<String, dynamic>> conversationNew({bool isolated = false}) async =>
      (await _call(_Op.convNew, arg: isolated ? 'isolated' : 'shared'))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> conversationSelect(String id) async =>
      (await _call(_Op.convSelect, arg: id)) as Map<String, dynamic>;
  Future<Map<String, dynamic>> conversationDelete(String id) async =>
      (await _call(_Op.convDelete, arg: id)) as Map<String, dynamic>;
  Future<Map<String, dynamic>> conversationRename(String id, String title) async =>
      (await _call(_Op.convRename,
          arg: jsonEncode({'id': id, 'title': title}))) as Map<String, dynamic>;
  Future<Map<String, dynamic>> conversationSetMemory(
          String id, String mode) async =>
      (await _call(_Op.convSetMemory, arg: jsonEncode({'id': id, 'mode': mode})))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> forget(String memoryId) async =>
      (await _call(_Op.forget, arg: memoryId)) as Map<String, dynamic>;
  Future<Map<String, dynamic>> setPolicy(String permission, String policy) async =>
      (await _call(_Op.setPolicy,
          arg: jsonEncode({'p': permission, 'x': policy}))) as Map<String, dynamic>;

  /// Ingested documents.
  Future<List<dynamic>> docs() async => (await _call(_Op.docs)) as List<dynamic>;
  Future<Map<String, dynamic>> docsIngest(String path) async =>
      (await _call(_Op.docsIngest, arg: path)) as Map<String, dynamic>;
  Future<List<dynamic>> docsSearch(String query) async =>
      (await _call(_Op.docsSearch, arg: query)) as List<dynamic>;
  Future<Map<String, dynamic>> docsDelete(String id) async =>
      (await _call(_Op.docsDelete, arg: id)) as Map<String, dynamic>;

  /// Email connector ops — `{"results": [...]}` / `{"message": {...}}` /
  /// `{"draft_id": ...}` or `{"error": ...}` when unconfigured.
  Future<Map<String, dynamic>> emailSearch(
          {String? query, String? from, String? label, bool unreadOnly = false, int limit = 20}) async =>
      (await _call(_Op.emailSearch,
          arg: jsonEncode({
            'query': ?query,
            'from': ?from,
            'label': ?label,
            'unread_only': unreadOnly,
            'limit': limit,
          }))) as Map<String, dynamic>;
  Future<Map<String, dynamic>> emailRead(String id) async =>
      (await _call(_Op.emailRead, arg: id)) as Map<String, dynamic>;
  Future<Map<String, dynamic>> emailDraft(
          {required List<String> to,
          List<String> cc = const [],
          required String subject,
          required String body,
          String? inReplyTo}) async =>
      (await _call(_Op.emailDraft,
          arg: jsonEncode({
            'to': to.map((a) => {'address': a}).toList(),
            'cc': cc.map((a) => {'address': a}).toList(),
            'subject': subject,
            'body': body,
            'in_reply_to': ?inReplyTo,
          }))) as Map<String, dynamic>;

  /// Send immediately via SMTP — same args as [emailDraft]. {sent:true}
  /// or {error} when no smtp block is configured.
  Future<Map<String, dynamic>> emailSend(
          {required List<String> to,
          List<String> cc = const [],
          required String subject,
          required String body,
          String? inReplyTo}) async =>
      (await _call(_Op.emailSend,
          arg: jsonEncode({
            'to': to.map((a) => {'address': a}).toList(),
            'cc': cc.map((a) => {'address': a}).toList(),
            'subject': subject,
            'body': body,
            'in_reply_to': ?inReplyTo,
          }))) as Map<String, dynamic>;

  /// Voice ops — `{stt, tts, mic, speaker}` probe; `voiceListen` blocks up
  /// to [maxSecs] in the worker isolate; `voiceSay` plays on the host
  /// speaker ({ok, played} or {ok, played:false, wav_b64}).
  /// Notification inbox — {notifications: [...], unread: n}. Poll from
  /// a timer for badge updates; rows roam via sync.
  Future<Map<String, dynamic>> notifyList({bool unreadOnly = false}) async =>
      (await _call(_Op.notifyList, arg: unreadOnly ? 'unread' : ''))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> notifyMarkRead(String id) async =>
      (await _call(_Op.notifyMarkRead, arg: id)) as Map<String, dynamic>;

  /// Personal App Cloud — installed packages and on-device sandboxed
  /// runs. `appsList` → `{apps: [...]}`; `appsRun` →
  /// `{stdout, stderr, exit_code, fuel}` or `{error}`.
  Future<Map<String, dynamic>> appsList() async =>
      (await _call(_Op.appsList)) as Map<String, dynamic>;
  Future<Map<String, dynamic>> appsRun(String id,
          {List<String> args = const []}) async =>
      (await _call(_Op.appsRun, arg: jsonEncode({'id': id, 'args': args})))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> appsMigrate(String id, String to) async =>
      (await _call(_Op.appsMigrate,
          arg: jsonEncode({'id': id, 'to': to}))) as Map<String, dynamic>;
  Future<Map<String, dynamic>> peersList() async =>
      (await _call(_Op.peersList)) as Map<String, dynamic>;

  /// Mint a capability token for [id]: [actions] like `exec,read`,
  /// [forDevice] a paired-peer prefix ('' = bearer).
  Future<Map<String, dynamic>> appsShareGrant(
          String id, String actions, int days, String forDevice) async =>
      (await _call(_Op.appsShareGrant,
          arg: jsonEncode({
            'id': id,
            'actions': actions,
            'days': days,
            'for': forDevice,
          }))) as Map<String, dynamic>;

  /// Re-grant a narrower sub-token from a held parent token JSON —
  /// the parent must carry `share` and be bound to this device.
  /// [forKey] is a paired-peer prefix or 64-hex pubkey ('' = bearer).
  Future<Map<String, dynamic>> shareDelegate(
          String parentJson, String actions, int days, String forKey) async =>
      (await _call(_Op.shareDelegate,
          arg: jsonEncode({
            'parent': parentJson,
            'actions': actions,
            'days': days,
            'for': forKey,
          }))) as Map<String, dynamic>;

  /// Guest-side capability call — [request] is a JSON object:
  /// {op, app_id, args, token, to, dir | relay, relay_token}.
  Future<Map<String, dynamic>> guestCall(Map<String, dynamic> request) async =>
      (await _call(_Op.guestCall, arg: jsonEncode(request)))
          as Map<String, dynamic>;

  Future<Map<String, dynamic>> shareList() async =>
      (await _call(_Op.shareList)) as Map<String, dynamic>;

  Future<Map<String, dynamic>> shareRevoke(String tokenId) async =>
      (await _call(_Op.shareRevoke, arg: tokenId)) as Map<String, dynamic>;

  /// Resolved provider/model + device — the chat header line.
  Future<Map<String, dynamic>> status() async {
    final r = (await _call(_Op.status)) as Map<String, dynamic>;
    if (!r.containsKey('error')) {
      lastStatus = r;
      _statusCtl.add(r);
    }
    return r;
  }

  /// Switch the serving endpoint/model: {server_url?, model?}. The
  /// returned status is also pushed into [statusNotifier] so headers
  /// refresh live.
  Future<Map<String, dynamic>> setProvider(
      {String? serverUrl, String? model}) async {
    final r = (await _call(_Op.setProvider,
        arg: jsonEncode(
            {'server_url': ?serverUrl, 'model': ?model}))) as Map<String, dynamic>;
    if (!r.containsKey('error')) {
      await status();
    }
    return r;
  }

  Future<Map<String, dynamic>> voiceStatus() async =>
      (await _call(_Op.voiceStatus)) as Map<String, dynamic>;
  Future<Map<String, dynamic>> voiceListen({int maxSecs = 30}) async =>
      (await _call(_Op.voiceListen, arg: '$maxSecs'))
          as Map<String, dynamic>;

  /// Streaming listen: {heard, text, partials[]} — partials are the
  /// ordered per-segment transcript (UI can render the segmentation).
  Future<Map<String, dynamic>> voiceListenStream({int maxSecs = 30}) async =>
      (await _call(_Op.voiceListenStream, arg: '$maxSecs'))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> voiceTranscribe(String path) async =>
      (await _call(_Op.voiceTranscribe, arg: path)) as Map<String, dynamic>;
  Future<Map<String, dynamic>> voiceSay(String text) async =>
      (await _call(_Op.voiceSay, arg: text)) as Map<String, dynamic>;

  Future<dynamic> _call(_Op op,
      {String? arg, StreamController<Map<String, dynamic>>? events}) {
    final id = _nextId++;
    final c = Completer<dynamic>();
    _pending[id] = _Pending(c, events);
    _requests.send(_Request(id, op, arg));
    return c.future;
  }

  static void _workerMain((SendPort, Map<String, dynamic>) args) {
    final (ready, config) = args;
    final inbox = ReceivePort();
    SendPort? replies;
    // The event callback is invoked synchronously on this isolate's thread
    // during client.send — `isolateLocal` is exactly that contract.
    int activeSend = 0;
    _eventSink = NativeCallable<NativeEventCallback>.isolateLocal(
        (Pointer<Utf8> evt, Pointer<Void> _) {
          replies?.send((activeSend, {'_event': evt.toDartString()}));
        });
    final PaiClient client;
    try {
      client = PaiClient.init(config);
      client.setEventCallback(_eventSink!.nativeFunction);
    } catch (e) {
      ready.send(_InitError(e.toString()));
      return;
    }
    ready.send(inbox.sendPort);
    inbox.listen((msg) {
      if (msg is _Subscribe) {
        replies = msg.port;
        return;
      }
      final req = msg as _Request;
      dynamic result;
      try {
        switch (req.op) {
          case _Op.send:
            activeSend = req.id;
            result = client.send(req.arg!);
            activeSend = 0;
          case _Op.resume:
            activeSend = req.id;
            result = client.resume(req.arg!);
            activeSend = 0;
          case _Op.approve:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.approve(a['id'] as String, a['granted'] as bool);
          case _Op.cancel:
            client.cancel();
            result = {'ok': true};
          case _Op.memories:
            result = client.memories();
          case _Op.modelsList:
            result = client.modelsList();
          case _Op.modelsScan:
            result = client.modelsScan();
          case _Op.modelsServe:
            {
              final a = jsonDecode(req.arg!) as Map<String, dynamic>;
              result = client.modelsServe(
                  a['slug'] as String, (a['port'] as num? ?? 0).toInt());
            }
          case _Op.modelsCatalog:
            result = client.modelsCatalog();
          case _Op.modelsInstall:
            result = client.modelsInstall(req.arg!);
          case _Op.audit:
            result = client.audit();
          case _Op.runs:
            result = client.runs();
          case _Op.conversations:
            result = client.conversations();
          case _Op.history:
            result = client.history();
          case _Op.policies:
            result = client.policies();
          case _Op.detect:
            result = client.detect();
          case _Op.convNew:
            result = client.conversationNew(isolated: req.arg == 'isolated');
          case _Op.convSelect:
            result = client.conversationSelect(req.arg!);
          case _Op.convDelete:
            result = client.conversationDelete(req.arg!);
          case _Op.convRename:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.conversationRename(
                a['id'] as String, a['title'] as String);
          case _Op.convSetMemory:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.conversationSetMemory(
                a['id'] as String, a['mode'] as String);
          case _Op.forget:
            result = client.forget(req.arg!);
          case _Op.setPolicy:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result =
                client.setPolicy(a['p'] as String, a['x'] as String);
          case _Op.docs:
            result = client.docs();
          case _Op.docsIngest:
            result = client.docsIngest(req.arg!);
          case _Op.docsSearch:
            result = client.docsSearch(req.arg!);
          case _Op.docsDelete:
            result = client.docsDelete(req.arg!);
          case _Op.emailSearch:
            result = client.emailSearch(req.arg);
          case _Op.emailRead:
            result = client.emailRead(req.arg!);
          case _Op.emailDraft:
            result = client.emailDraft(req.arg!);
          case _Op.emailSend:
            result = client.emailSend(req.arg!);
          case _Op.notifyList:
            result = client.notifyList(unreadOnly: req.arg == 'unread');
          case _Op.notifyMarkRead:
            result = client.notifyMarkRead(req.arg!);
          case _Op.status:
            result = client.status();
          case _Op.setProvider:
            result = client.setProvider(req.arg!);
          case _Op.voiceStatus:
            result = client.voiceStatus();
          case _Op.voiceListen:
            result =
                client.voiceListen(maxSecs: int.tryParse(req.arg ?? '') ?? 30);
          case _Op.voiceListenStream:
            result = client.voiceListenStream(
                maxSecs: int.tryParse(req.arg ?? '') ?? 30);
          case _Op.voiceTranscribe:
            result = client.voiceTranscribe(req.arg!);
          case _Op.voiceSay:
            result = client.voiceSay(req.arg!);
          case _Op.appsList:
            result = client.appsList();
          case _Op.appsRun:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.appsRun(a['id'] as String,
                args: (a['args'] as List).cast<String>());
          case _Op.appsMigrate:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result =
                client.appsMigrate(a['id'] as String, a['to'] as String);
          case _Op.peersList:
            result = client.peersList();
          case _Op.appsShareGrant:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.shareGrant(
                a['id'] as String,
                a['actions'] as String,
                a['days'] as int,
                a['for'] as String);
          case _Op.shareDelegate:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.shareDelegate(
                a['parent'] as String,
                a['actions'] as String,
                a['days'] as int,
                a['for'] as String);
          case _Op.guestCall:
            result = client.guestCall(req.arg!);
          case _Op.shareList:
            result = client.shareList();
          case _Op.shareRevoke:
            result = client.shareRevoke(req.arg!);
          case _Op.mediaList:
            result = client.mediaList();
          case _Op.mediaGen:
            result = client.mediaGen(req.arg!);
          case _Op.mediaExport:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.mediaExport(
                a['id'] as String, a['dest'] as String);
          case _Op.syncNow:
            result = client.syncNow(req.arg ?? '{}');
          case _Op.pairOffer:
            result = client.pairOffer(req.arg!);
          case _Op.pairAccept:
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.pairAccept(
                a['offer'] as String, a['out'] as String);
          case _Op.pairComplete:
            result = client.pairComplete(req.arg!);
          case _Op.syncStatus:
            result = client.syncStatus();
          case _Op.pairFolder:
            result = client.pairFolder();
        }
      } catch (e) {
        result = {'error': e.toString()};
      }
      replies?.send((req.id, result));
    });
  }
}

/// Retained for the process lifetime: the Rust side may invoke this
/// callback at any time during a `pai_send` call.
NativeCallable<NativeEventCallback>? _eventSink;

class _Pending {
  _Pending(this.completer, this.events);
  final Completer<dynamic> completer;
  final StreamController<Map<String, dynamic>>? events;
}

enum _Op {
  send,
  resume,
  approve,
  cancel,
  memories,
  modelsList,
  modelsScan,
  modelsServe,
  modelsCatalog,
  modelsInstall,
  audit,
  runs,
  conversations,
  history,
  policies,
  detect,
  convNew,
  convSelect,
  convDelete,
  convRename,
  convSetMemory,
  forget,
  setPolicy,
  docs,
  docsIngest,
  docsSearch,
  docsDelete,
  emailSearch,
  emailRead,
  emailDraft,
  emailSend,
  notifyList,
  notifyMarkRead,
  status,
  setProvider,
  voiceStatus,
  voiceListen,
  voiceListenStream,
  voiceTranscribe,
  voiceSay,
  appsList,
  appsRun,
  appsMigrate,
  peersList,
  appsShareGrant,
  shareDelegate,
  guestCall,
  shareList,
  shareRevoke,
  mediaList,
  mediaGen,
  mediaExport,
  syncNow,
  syncStatus,
  pairOffer,
  pairAccept,
  pairComplete,
  pairFolder,
}

class _InitError {
  _InitError(this.message);
  final String message;
}

class _Subscribe {
  _Subscribe(this.port);
  final SendPort port;
}

class _Request {
  _Request(this.id, this.op, this.arg);
  final int id;
  final _Op op;
  final String? arg;
}
