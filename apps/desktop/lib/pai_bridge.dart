/// Platform-neutral bridge over the runtime. Native (dart:ffi) runs ops
/// in a worker isolate; web (PWA) POSTs them to `/api/bridge` on the
/// `pai serve --bridge` gateway. Same ops, same result shapes — the
/// transport is conditionally imported.
library;

import 'dart:async';
import 'dart:convert';
import 'bridge_transport.dart';

/// Handle for one in-flight `send`: live [events] then the final [result].
class SendHandle {
  SendHandle(this.events, this.result);
  final Stream<Map<String, dynamic>> events;
  final Future<Map<String, dynamic>> result;
}

class PaiBridge {
  PaiBridge._(this._transport);

  final BridgeTransport _transport;

  /// Latest resolved provider/model status — refreshed by [status]
  /// and [setProvider]. [statusStream] pushes updates so the chat
  /// header can react live.
  Map<String, dynamic> lastStatus = const {};
  final _statusCtl = StreamController<Map<String, dynamic>>.broadcast();
  Stream<Map<String, dynamic>> get statusStream => _statusCtl.stream;

  /// Out-of-band UI events from the runtime (kind `ui:*`) — e.g. the
  /// drive watcher firing `ui:model_packs` when a pack root mounts.
  final _uiEvents = StreamController<Map<String, dynamic>>.broadcast();
  Stream<Map<String, dynamic>> get uiEvents => _uiEvents.stream;

  /// Connects the platform transport (worker isolate on native, HTTP on
  /// web) and initializes the Rust runtime behind it.
  static Future<PaiBridge> start(Map<String, dynamic> config) async {
    final t = await startBridgeTransport(config);
    final bridge = PaiBridge._(t);
    t.uiEvents.listen(bridge._uiEvents.add);
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

  /// Generate media. `json`: `{prompt, kind?, duration_seconds?,
  /// width?, height?}` — kind is `audio` (default) / `image` /
  /// `image_edit` / `upscale` / `video`. Long-running — runs on the
  /// worker isolate.
  Future<Map<String, dynamic>> mediaGen(String prompt,
      {String kind = 'audio',
      int seconds = 10,
      int? width,
      int? height}) async =>
      (await _call(_Op.mediaGen,
          arg: jsonEncode({
            'prompt': prompt,
            'kind': kind,
            'duration_seconds': seconds,
            'width': ?width,
            'height': ?height,
          })))
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

  /// Pairing via QR — `{"mode":"offer"}` mints this device's offer
  /// payload+matrix; `{"mode":"accept","offer":"<json>"}` consumes
  /// a scanned offer payload and returns the accept QR to show back.
  Future<Map<String, dynamic>> pairQr(String mode, {String? offer}) async =>
      (await _call(_Op.pairQr,
          arg: jsonEncode({'mode': mode, 'offer': ?offer})))
          as Map<String, dynamic>;

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

  /// Configure the mail account in-app: writes email.json and stores the
  /// password in the OS keystore (never in the file). Pass an empty
  /// [smtpHost] for drafts-only mode.
  Future<Map<String, dynamic>> emailConfigure(
          {required String host,
          int port = 993,
          required String user,
          String? password,
          String? smtpHost,
          int smtpPort = 465,
          String smtpTls = 'tls'}) async =>
      (await _call(_Op.emailConfigure,
          arg: jsonEncode({
            'host': host,
            'port': port,
            'user': user,
            'password': ?password,
            'smtp': (smtpHost == null || smtpHost.isEmpty)
                ? null
                : {'host': smtpHost, 'port': smtpPort, 'tls': smtpTls},
          }))) as Map<String, dynamic>;

  /// GitLab connector — binding state `{configured, host?, project?,
  /// auth?}`; list/read ops return `{issues|merge_requests|pipelines|
  /// projects: [...]}` or `{error}` when unconfigured.
  Future<Map<String, dynamic>> gitlabStatus() async =>
      (await _call(_Op.gitlabStatus)) as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabProjects(
          {String? search, int limit = 20}) async =>
      (await _call(_Op.gitlabProjects,
          arg: jsonEncode({'search': ?search, 'limit': limit})))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabIssues(
          {String? project,
          String? state,
          String? search,
          List<String> labels = const [],
          int limit = 20}) async =>
      (await _call(_Op.gitlabIssues,
          arg: jsonEncode({
            'project': ?project,
            'state': ?state,
            'search': ?search,
            'labels': labels,
            'limit': limit,
          }))) as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabIssue(int iid,
          {String? project}) async =>
      (await _call(_Op.gitlabIssue,
          arg: jsonEncode({'iid': iid, 'project': ?project})))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabIssueCreate(
          {required String title,
          String? description,
          List<String> labels = const [],
          String? project}) async =>
      (await _call(_Op.gitlabIssueCreate,
          arg: jsonEncode({
            'title': title,
            'description': ?description,
            'labels': labels,
            'project': ?project,
          }))) as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabComment(
          {required String kind,
          required int iid,
          required String body,
          String? project}) async =>
      (await _call(_Op.gitlabComment,
          arg: jsonEncode({
            'kind': kind,
            'iid': iid,
            'body': body,
            'project': ?project,
          }))) as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabMrs(
          {String? project,
          String? state,
          String? search,
          int limit = 20}) async =>
      (await _call(_Op.gitlabMrs,
          arg: jsonEncode({
            'project': ?project,
            'state': ?state,
            'search': ?search,
            'limit': limit,
          }))) as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabMr(int iid, {String? project}) async =>
      (await _call(_Op.gitlabMr,
          arg: jsonEncode({'iid': iid, 'project': ?project})))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabMrCreate(
          {required String sourceBranch,
          String? targetBranch,
          required String title,
          String? description,
          String? project}) async =>
      (await _call(_Op.gitlabMrCreate,
          arg: jsonEncode({
            'source_branch': sourceBranch,
            'target_branch': ?targetBranch,
            'title': title,
            'description': ?description,
            'project': ?project,
          }))) as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabMrMerge(int iid,
          {String? project}) async =>
      (await _call(_Op.gitlabMrMerge,
          arg: jsonEncode({'iid': iid, 'project': ?project})))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabPipelines(
          {String? project, int limit = 20}) async =>
      (await _call(_Op.gitlabPipelines,
          arg: jsonEncode({'project': ?project, 'limit': limit})))
          as Map<String, dynamic>;
  Future<Map<String, dynamic>> gitlabFile(String path,
          {String? ref, String? project}) async =>
      (await _call(_Op.gitlabFile,
          arg: jsonEncode(
              {'path': path, 'ref': ?ref, 'project': ?project})))
          as Map<String, dynamic>;

  /// Configure the binding in-app: writes gitlab.json and stores the
  /// personal access token in the OS keystore (never in the file).
  Future<Map<String, dynamic>> gitlabConfigure(
          {required String host, String? token, String? project}) async =>
      (await _call(_Op.gitlabConfigure,
          arg: jsonEncode(
              {'host': host, 'token': ?token, 'project': ?project})))
          as Map<String, dynamic>;

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

  /// Placement view — every device with its latest capability
  /// announcement (ops, load, freshness, score) + local weight.
  Future<Map<String, dynamic>> devicesPlacement() async =>
      (await _call(_Op.devicesPlacement)) as Map<String, dynamic>;

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
      {String? arg, StreamController<Map<String, dynamic>>? events}) =>
      _transport.call(op.name, arg, events: events);
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
  emailConfigure,
  gitlabStatus,
  gitlabProjects,
  gitlabIssues,
  gitlabIssue,
  gitlabIssueCreate,
  gitlabComment,
  gitlabMrs,
  gitlabMr,
  gitlabMrCreate,
  gitlabMrMerge,
  gitlabPipelines,
  gitlabFile,
  gitlabConfigure,
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
  devicesPlacement,
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
  pairQr,
}
