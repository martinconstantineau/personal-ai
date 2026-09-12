/// Isolate bridge: `pai_send` blocks on inference, so all FFI work happens
/// inside a dedicated worker isolate that owns the runtime handle.
library;

import 'dart:async';
import 'dart:isolate';
import 'pai_ffi.dart';

class PaiBridge {
  PaiBridge._(this._requests);

  final SendPort _requests;
  int _nextId = 0;
  final _pending = <int, Completer<dynamic>>{};

  /// Spawns the worker isolate and initializes the Rust runtime inside it.
  static Future<PaiBridge> start(Map<String, dynamic> config) async {
    final ready = ReceivePort();
    await Isolate.spawn(_workerMain, (ready.sendPort, config));
    final requests = await ready.first as SendPort;
    final bridge = PaiBridge._(requests);
    // Route replies back by id.
    final replies = ReceivePort();
    requests.send(_Subscribe(replies.sendPort));
    replies.listen((msg) {
      final (id, value) = msg as (int, dynamic);
      bridge._pending.remove(id)?.complete(value);
    });
    return bridge;
  }

  Future<Map<String, dynamic>> send(String text) async =>
      (await _call(_Op.send, text)) as Map<String, dynamic>;

  Future<List<dynamic>> memories() async =>
      (await _call(_Op.memories)) as List<dynamic>;

  Future<List<dynamic>> audit() async => (await _call(_Op.audit)) as List<dynamic>;

  Future<dynamic> _call(_Op op, [String? arg]) {
    final id = _nextId++;
    final c = Completer<dynamic>();
    _pending[id] = c;
    _requests.send(_Request(id, op, arg));
    return c.future;
  }

  static void _workerMain((SendPort, Map<String, dynamic>) args) {
    final (ready, config) = args;
    final inbox = ReceivePort();
    SendPort? replies;
    final client = PaiClient.init(config);
    ready.send(inbox.sendPort);
    inbox.listen((msg) {
      if (msg is _Subscribe) {
        replies = msg.port;
        return;
      }
      final req = msg as _Request;
      dynamic result;
      try {
        result = switch (req.op) {
          _Op.send => client.send(req.arg!),
          _Op.memories => client.memories(),
          _Op.audit => client.audit(),
        };
      } catch (e) {
        result = {'error': e.toString()};
      }
      replies?.send((req.id, result));
    });
  }
}

enum _Op { send, memories, audit }

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
