/// Web transport: POSTs ops to `/api/bridge` on the same origin — the
/// `pai serve --bridge` gateway hosting the runtime. Live events arrive
/// by draining the gateway's queue (`pollEvents`) on a short timer;
/// during a `send`/`resume` they route to that call's event sink, so
/// approvals and run progress behave like the native isolate stream.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:html' as html;
import 'bridge_transport.dart';

class _WebTransport implements BridgeTransport {
  _WebTransport._(this._endpoint, this._token) {
    _timer = Timer.periodic(const Duration(milliseconds: 350), (_) => _poll());
  }

  final Uri _endpoint;
  final String? _token;
  final _uiEvents = StreamController<Map<String, dynamic>>.broadcast();
  Timer? _timer;

  /// The in-flight send/resume's event sink — polled run events land here.
  StreamController<Map<String, dynamic>>? _active;

  @override
  Stream<Map<String, dynamic>> get uiEvents => _uiEvents.stream;

  Map<String, String> get _headers => {
        'Content-Type': 'application/json',
        'X-Pai-Bridge-Token': ?_token,
      };

  Future<dynamic> _post(String op, String? arg) async {
    final resp = await html.HttpRequest.request(
      _endpoint.toString(),
      method: 'POST',
      requestHeaders: _headers,
      sendData: jsonEncode({'op': op, 'arg': ?arg}),
    );
    // A 4xx still carries the runtime's {"error": …} JSON — return it
    // like the native transport does (PaiClient surfaces op errors as
    // maps, not exceptions).
    if (resp.status != 200) {
      final body = jsonDecode(resp.responseText ?? 'null');
      if (body is Map<String, dynamic> && body['error'] != null) {
        return body;
      }
      throw StateError('bridge $op → HTTP ${resp.status}');
    }
    return jsonDecode(resp.responseText ?? 'null');
  }

  Future<void> _poll() async {
    try {
      final r = await _post('pollEvents', null);
      final evs = (r is Map ? r['events'] : null) as List? ?? const [];
      for (final e in evs) {
        if (e is Map<String, dynamic>) {
          // `ui:` kinds broadcast to screens; the rest belong to the
          // run currently streaming.
          if ('${e['kind']}'.startsWith('ui:')) {
            _uiEvents.add(e);
          } else {
            _active?.add(e);
          }
        }
      }
    } catch (_) {
      // Gateway unreachable this tick — the next poll retries.
    }
  }

  @override
  Future<dynamic> call(String op, String? arg,
      {StreamController<Map<String, dynamic>>? events}) async {
    final isRun = op == 'send' || op == 'resume';
    if (isRun && events != null) _active = events;
    try {
      return await _post(op, arg);
    } finally {
      if (isRun && events != null) {
        // Final drain — catches events emitted between the last poll
        // tick and the run's completion.
        await _poll();
        _active = null;
      }
    }
  }

  @override
  void dispose() {
    _timer?.cancel();
    _uiEvents.close();
    _active?.close();
  }
}

Future<BridgeTransport> startBridgeTransport(Map<String, dynamic> config) async {
  // Token arrives once via `?token=` (non-loopback gateways), then lives
  // in localStorage so later launches — including the installed PWA —
  // need no URL parameter.
  final token = Uri.base.queryParameters['token'] ??
      html.window.localStorage['pai.bridge.token'];
  if (token != null) {
    html.window.localStorage['pai.bridge.token'] = token;
  }
  final t = _WebTransport._(
      Uri.parse('${Uri.base.origin}/api/bridge'), token);
  try {
    await t._post('status', null);
  } catch (e) {
    throw StateError('bridge unreachable: $e — start `pai serve --bridge`');
  }
  return t;
}
