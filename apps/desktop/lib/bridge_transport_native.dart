/// Native transport: all FFI work happens inside a dedicated worker
/// isolate that owns the runtime handle. Live run events (token deltas,
/// approval requests, tool progress) stream back through the reply port
/// while a send is in flight.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:isolate';
import 'package:ffi/ffi.dart';
import 'bridge_transport.dart';
import 'pai_ffi.dart';

class _NativeTransport implements BridgeTransport {
  _NativeTransport(this._requests, this._replies);

  final SendPort _requests;
  final ReceivePort _replies;
  int _nextId = 0;
  final _pending = <int, _Pending>{};

  final _uiEvents = StreamController<Map<String, dynamic>>.broadcast();
  @override
  Stream<Map<String, dynamic>> get uiEvents => _uiEvents.stream;

  void _route() {
    _replies.listen((msg) {
      final (id, value) = msg as (int, dynamic);
      final p = _pending[id];
      if (value is Map && value['_event'] != null) {
        final decoded = jsonDecode(value['_event'] as String);
        if (decoded is Map<String, dynamic>) {
          // `ui:` kinds are runtime-originated UI events — broadcast to
          // screens rather than whichever call happens to be pending.
          if ('${decoded['kind']}'.startsWith('ui:')) {
            _uiEvents.add(decoded);
          } else {
            p?.events?.add(decoded);
          }
        }
      } else {
        if (p == null) return;
        _pending.remove(id);
        p.completer.complete(value);
        p.events?.close();
      }
    });
  }

  @override
  Future<dynamic> call(String op, String? arg,
      {StreamController<Map<String, dynamic>>? events}) {
    final id = _nextId++;
    final c = Completer<dynamic>();
    _pending[id] = _Pending(c, events);
    _requests.send(_Request(id, op, arg));
    return c.future;
  }

  @override
  void dispose() {
    _replies.close();
  }
}

Future<BridgeTransport> startBridgeTransport(Map<String, dynamic> config) async {
  final ready = ReceivePort();
  await Isolate.spawn(_workerMain, (ready.sendPort, config));
  final handshake = await ready.first;
  if (handshake is _InitError) {
    throw StateError(handshake.message);
  }
  final requests = handshake as SendPort;
  final replies = ReceivePort();
  requests.send(_Subscribe(replies.sendPort));
  final t = _NativeTransport(requests, replies).._route();
  return t;
}

void _workerMain((SendPort, Map<String, dynamic>) args) {
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
        case 'send':
          activeSend = req.id;
          result = client.send(req.arg!);
          activeSend = 0;
        case 'resume':
          activeSend = req.id;
          result = client.resume(req.arg!);
          activeSend = 0;
        case 'approve':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result = client.approve(a['id'] as String, a['granted'] as bool);
        case 'cancel':
          client.cancel();
          result = {'ok': true};
        case 'memories':
          result = client.memories();
        case 'modelsList':
          result = client.modelsList();
        case 'modelsScan':
          result = client.modelsScan();
        case 'modelsServe':
          {
            final a = jsonDecode(req.arg!) as Map<String, dynamic>;
            result = client.modelsServe(
                a['slug'] as String, (a['port'] as num? ?? 0).toInt());
          }
        case 'modelsCatalog':
          result = client.modelsCatalog();
        case 'modelsInstall':
          result = client.modelsInstall(req.arg!);
        case 'audit':
          result = client.audit();
        case 'runs':
          result = client.runs();
        case 'conversations':
          result = client.conversations();
        case 'history':
          result = client.history();
        case 'policies':
          result = client.policies();
        case 'detect':
          result = client.detect();
        case 'convNew':
          result = client.conversationNew(isolated: req.arg == 'isolated');
        case 'convSelect':
          result = client.conversationSelect(req.arg!);
        case 'convDelete':
          result = client.conversationDelete(req.arg!);
        case 'convRename':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result = client.conversationRename(
              a['id'] as String, a['title'] as String);
        case 'convSetMemory':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result = client.conversationSetMemory(
              a['id'] as String, a['mode'] as String);
        case 'forget':
          result = client.forget(req.arg!);
        case 'setPolicy':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result =
              client.setPolicy(a['p'] as String, a['x'] as String);
        case 'docs':
          result = client.docs();
        case 'docsIngest':
          result = client.docsIngest(req.arg!);
        case 'docsSearch':
          result = client.docsSearch(req.arg!);
        case 'docsDelete':
          result = client.docsDelete(req.arg!);
        case 'emailSearch':
          result = client.emailSearch(req.arg);
        case 'emailRead':
          result = client.emailRead(req.arg!);
        case 'emailDraft':
          result = client.emailDraft(req.arg!);
        case 'emailSend':
          result = client.emailSend(req.arg!);
        case 'emailConfigure':
          result = client.emailConfigure(req.arg!);
        case 'gitlabStatus':
          result = client.gitlabStatus();
        case 'gitlabProjects':
          result = client.gitlabProjects(req.arg);
        case 'gitlabIssues':
          result = client.gitlabIssues(req.arg);
        case 'gitlabIssue':
          result = client.gitlabIssue(req.arg!);
        case 'gitlabIssueCreate':
          result = client.gitlabIssueCreate(req.arg!);
        case 'gitlabComment':
          result = client.gitlabComment(req.arg!);
        case 'gitlabMrs':
          result = client.gitlabMrs(req.arg);
        case 'gitlabMr':
          result = client.gitlabMr(req.arg!);
        case 'gitlabMrCreate':
          result = client.gitlabMrCreate(req.arg!);
        case 'gitlabMrMerge':
          result = client.gitlabMrMerge(req.arg!);
        case 'gitlabPipelines':
          result = client.gitlabPipelines(req.arg);
        case 'gitlabFile':
          result = client.gitlabFile(req.arg!);
        case 'gitlabConfigure':
          result = client.gitlabConfigure(req.arg!);
        case 'notifyList':
          result = client.notifyList(unreadOnly: req.arg == 'unread');
        case 'notifyMarkRead':
          result = client.notifyMarkRead(req.arg!);
        case 'status':
          result = client.status();
        case 'setProvider':
          result = client.setProvider(req.arg!);
        case 'voiceStatus':
          result = client.voiceStatus();
        case 'voiceListen':
          result =
              client.voiceListen(maxSecs: int.tryParse(req.arg ?? '') ?? 30);
        case 'voiceListenStream':
          result = client.voiceListenStream(
              maxSecs: int.tryParse(req.arg ?? '') ?? 30);
        case 'voiceTranscribe':
          result = client.voiceTranscribe(req.arg!);
        case 'voiceSay':
          result = client.voiceSay(req.arg!);
        case 'appsList':
          result = client.appsList();
        case 'appsRun':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result = client.appsRun(a['id'] as String,
              args: (a['args'] as List).cast<String>());
        case 'appsMigrate':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result =
              client.appsMigrate(a['id'] as String, a['to'] as String);
        case 'peersList':
          result = client.peersList();
        case 'devicesPlacement':
          result = client.devicesPlacement();
        case 'appsShareGrant':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result = client.shareGrant(
              a['id'] as String,
              a['actions'] as String,
              a['days'] as int,
              a['for'] as String);
        case 'shareDelegate':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result = client.shareDelegate(
              a['parent'] as String,
              a['actions'] as String,
              a['days'] as int,
              a['for'] as String);
        case 'guestCall':
          result = client.guestCall(req.arg!);
        case 'shareList':
          result = client.shareList();
        case 'shareRevoke':
          result = client.shareRevoke(req.arg!);
        case 'mediaList':
          result = client.mediaList();
        case 'mediaGen':
          result = client.mediaGen(req.arg!);
        case 'mediaExport':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result = client.mediaExport(
              a['id'] as String, a['dest'] as String);
        case 'syncNow':
          result = client.syncNow(req.arg ?? '{}');
        case 'pairOffer':
          result = client.pairOffer(req.arg!);
        case 'pairAccept':
          final a = jsonDecode(req.arg!) as Map<String, dynamic>;
          result = client.pairAccept(
              a['offer'] as String, a['out'] as String);
        case 'pairComplete':
          result = client.pairComplete(req.arg!);
        case 'syncStatus':
          result = client.syncStatus();
        case 'pairFolder':
          result = client.pairFolder();
        case 'pairQr':
          result = client.pairQr(req.arg!);
        default:
          result = {'error': 'unknown op ${req.op}'};
      }
    } catch (e) {
      result = {'error': e.toString()};
    }
    replies?.send((req.id, result));
  });
}

/// Retained for the process lifetime: the Rust side may invoke this
/// callback at any time during a `pai_send` call.
NativeCallable<NativeEventCallback>? _eventSink;

class _Pending {
  _Pending(this.completer, this.events);
  final Completer<dynamic> completer;
  final StreamController<Map<String, dynamic>>? events;
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
  final String op;
  final String? arg;
}
