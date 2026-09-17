/// Transport for bridge ops. Native drives `dart:ffi` inside a worker
/// isolate; web POSTs to `/api/bridge` on the `pai serve --bridge`
/// gateway that hosts the same runtime.
library;

import 'dart:async';

export 'bridge_transport_web.dart' if (dart.library.io) 'bridge_transport_native.dart';

abstract class BridgeTransport {
  /// Runtime-originated UI events (`kind` `ui:*`) — not tied to a call.
  Stream<Map<String, dynamic>> get uiEvents;

  /// One op. [events], when passed, receives the call's live AgentEvents
  /// (token deltas, approval requests, tool progress).
  Future<dynamic> call(String op, String? arg,
      {StreamController<Map<String, dynamic>>? events});

  void dispose();
}
