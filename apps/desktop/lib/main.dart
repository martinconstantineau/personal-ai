import 'dart:io';
import 'package:flutter/material.dart';
import 'pai_bridge.dart';

void main() => runApp(const PaiApp());

class PaiApp extends StatelessWidget {
  const PaiApp({super.key});

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Personal AI',
        debugShowCheckedModeBanner: false,
        theme: ThemeData(
          colorScheme: ColorScheme.fromSeed(
              seedColor: const Color(0xFF6C5CE7), brightness: Brightness.dark),
          useMaterial3: true,
        ),
        home: const ChatScreen(),
      );
}

/// One chat transcript row. `streaming` marks the in-flight assistant reply.
class _Entry {
  _Entry({required this.role, this.text = '', this.sub = '', this.streaming = false});
  final String role; // you | ai | system
  String text;
  String sub;
  bool streaming;
  bool isError = false;
}

class ChatScreen extends StatefulWidget {
  const ChatScreen({super.key});
  @override
  State<ChatScreen> createState() => _ChatScreenState();
}

class _ChatScreenState extends State<ChatScreen> {
  final _input = TextEditingController();
  final _scroll = ScrollController();
  PaiBridge? _pai;
  String? _error;
  bool _sending = false;
  final _entries = <_Entry>[];
  List<dynamic> _convs = const [];
  List<dynamic> _interrupted = const [];

  @override
  void initState() {
    super.initState();
    _init();
  }

  Future<void> _init() async {
    final dataDir = Platform.environment['PAI_DATA_DIR'] ??
        '${Directory.current.path}/.pai-data';
    // 'auto' probes llama-server / Ollama / LM Studio, falls back to echo.
    final provider = Platform.environment['PAI_PROVIDER'] ?? 'auto';
    try {
      final bridge = await PaiBridge.start(
          {'data_dir': dataDir, 'provider': provider});
      if (!mounted) return;
      setState(() => _pai = bridge);
      await _loadHistory();
      await _refreshConvs();
      final runs = await _pai!.runs();
      if (mounted) setState(() => _interrupted = runs);
    } catch (e) {
      setState(() => _error = 'Core init failed: $e\n'
          '(build the core: cargo build -p pai-ffi)');
    }
  }

  Future<void> _refreshConvs() async {
    if (_pai == null) return;
    final convs = await _pai!.conversations();
    if (mounted) setState(() => _convs = convs);
  }

  Future<void> _loadHistory() async {
    if (_pai == null) return;
    final msgs = await _pai!.history();
    if (!mounted) return;
    setState(() {
      _entries
        ..clear()
        ..addAll(msgs.map(_messageToEntry).whereType<_Entry>());
    });
  }

  _Entry? _messageToEntry(dynamic m) {
    if (m is! Map) return null;
    final role = m['role'];
    if (role == 'system' || role == 'tool') return null;
    final text = (m['content'] as List? ?? [])
        .where((c) => c is Map && c['type'] == 'text')
        .map((c) => c['text'] as String? ?? '')
        .join('');
    return _Entry(role: role == 'user' ? 'you' : 'ai', text: text);
  }

  void _scrollDown() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_scroll.hasClients) {
        _scroll.jumpTo(_scroll.position.maxScrollExtent);
      }
    });
  }

  Future<void> _send() async {
    final text = _input.text.trim();
    if (text.isEmpty || _pai == null || _sending) return;
    _input.clear();
    final user = _Entry(role: 'you', text: text);
    final ai = _Entry(role: 'ai', streaming: true);
    setState(() {
      _entries.add(user);
      _entries.add(ai);
      _sending = true;
    });
    _scrollDown();

    var streamed = false;
    final handle = _pai!.sendStreaming(text);
    // Drain the event stream to completion: pending `add`s are flushed
    // before the controller closes, which happens when `result` arrives.
    final eventsDone = () async {
      await for (final ev in handle.events) {
        _onEvent(ev, ai, () => streamed = true, () => streamed);
      }
    }();
    final result = await handle.result;
    await eventsDone;
    if (!mounted) return;
    setState(() {
      ai.streaming = false;
      if (!streamed && result['answer'] != null) {
        ai.text = result['answer'] as String;
      }
      if (result['error'] != null && ai.text.isEmpty) {
        ai
          ..isError = true
          ..text = 'error: ${result['error']}';
      }
      _sending = false;
    });
    _scrollDown();
  }

  void _onEvent(Map<String, dynamic> ev, _Entry ai, void Function() markStreamed,
      bool Function() isStreamed) {
    if (!mounted) return;
    switch (ev['type']) {
      case 'run_started':
        setState(() => ai.sub = 'run ${(ev['run'] as String? ?? '').substring(0, 8)}');
      case 'step':
        setState(() => ai.sub = 'step ${ev['index']}');
      case 'tool_call_requested':
        setState(() => _entries.add(_Entry(
            role: 'system', text: 'tool call: ${ev['tool']} (${ev['risk']})')));
      case 'approval_needed':
        _showApproval(ev['request'] as Map<String, dynamic>);
      case 'tool_executed':
        setState(() => _entries.add(_Entry(
            role: 'system', text: '${ev['tool']}: ${ev['summary']}')));
      case 'tool_denied':
        setState(() => _entries
            .add(_Entry(role: 'system', text: 'denied: ${ev['tool']}')));
      case 'text_delta':
        markStreamed();
        setState(() => ai.text += (ev['text'] as String? ?? ''));
        _scrollDown();
      case 'done':
        setState(() {
          ai.streaming = false;
          if (!isStreamed() && ev['answer'] != null) {
            ai.text = ev['answer'] as String;
          }
          ai.sub = 'state: ${ev['state']}';
        });
    }
  }

  Future<void> _showApproval(Map<String, dynamic> req) async {
    final granted = await showModalBottomSheet<bool>(
      context: context,
      isDismissible: false,
      enableDrag: false,
      builder: (ctx) => _ApprovalSheet(req: req),
    );
    await _pai!.approve(req['tool_call'] as String, granted ?? false);
  }

  Future<void> _selectConversation(String id) async {
    final res = await _pai!.conversationSelect(id);
    if (res['error'] == null) {
      final msgs = res['messages'] as List? ?? [];
      setState(() {
        _entries
          ..clear()
          ..addAll(msgs.map(_messageToEntry).whereType<_Entry>());
      });
      await _refreshConvs();
    }
    if (mounted) Navigator.of(context).maybePop();
  }

  Future<void> _newConversation({bool isolated = false}) async {
    await _pai!.conversationNew(isolated: isolated);
    setState(_entries.clear);
    await _refreshConvs();
    if (mounted) Navigator.of(context).maybePop();
  }

  Future<void> _resumeRun(String runId) async {
    if (_sending) return;
    final ai = _Entry(role: 'ai', streaming: true, sub: 'resuming…');
    setState(() {
      _sending = true;
      _entries.add(ai);
    });
    final res = await _pai!.resume(runId);
    if (!mounted) return;
    setState(() {
      ai.streaming = false;
      ai.sub = 'resumed run';
      ai.text = (res['answer'] as String?) ?? (res['error'] as String?) ?? '';
      _sending = false;
    });
    final runs = await _pai!.runs();
    if (mounted) setState(() => _interrupted = runs);
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(
        title: const Text('Personal AI'),
        actions: [
          if (_sending)
            IconButton(
              icon: const Icon(Icons.stop_circle_outlined),
              tooltip: 'Cancel run',
              onPressed: () => _pai?.cancel(),
            ),
          IconButton(
            icon: const Icon(Icons.psychology_outlined),
            tooltip: 'Memories',
            onPressed: _pai == null
                ? null
                : () => Navigator.of(context).push(MaterialPageRoute(
                    builder: (_) => MemoriesScreen(bridge: _pai!))),
          ),
          IconButton(
            icon: const Icon(Icons.description_outlined),
            tooltip: 'Documents',
            onPressed: _pai == null
                ? null
                : () => Navigator.of(context).push(MaterialPageRoute(
                    builder: (_) => DocumentsScreen(bridge: _pai!))),
          ),
          IconButton(
            icon: const Icon(Icons.mail_outline),
            tooltip: 'Email',
            onPressed: _pai == null
                ? null
                : () => Navigator.of(context).push(MaterialPageRoute(
                    builder: (_) => EmailScreen(bridge: _pai!))),
          ),
          IconButton(
            icon: const Icon(Icons.policy_outlined),
            tooltip: 'Permissions',
            onPressed: _pai == null
                ? null
                : () => Navigator.of(context).push(MaterialPageRoute(
                    builder: (_) => PoliciesScreen(bridge: _pai!))),
          ),
        ],
      ),
      drawer: _pai == null
          ? null
          : _ConvDrawer(
              convs: _convs,
              interrupted: _interrupted,
              onSelect: _selectConversation,
              onNew: _newConversation,
              onResume: _resumeRun,
              onRename: (id, title) async {
                await _pai!.conversationRename(id, title);
                await _refreshConvs();
              },
              onDelete: (id) async {
                await _pai!.conversationDelete(id);
                await _refreshConvs();
                await _loadHistory();
              },
              onScope: (id, mode) async {
                await _pai!.conversationSetMemory(id, mode);
                await _refreshConvs();
              },
            ),
      body: Column(children: [
        if (_error != null)
          MaterialBanner(
              content: Text(_error!),
              actions: const [SizedBox.shrink()],
              backgroundColor: cs.errorContainer),
        if (_interrupted.isNotEmpty)
          MaterialBanner(
            content: Text(
                '${_interrupted.length} interrupted run(s) — resume from the drawer'),
            actions: const [SizedBox.shrink()],
          ),
        Expanded(
          child: _entries.isEmpty
              ? Center(
                  child: Text(
                    _pai == null
                        ? (_error == null ? 'starting…' : '')
                        : 'local-first · private · auditable',
                    style: TextStyle(color: cs.onSurfaceVariant),
                  ))
              : ListView.builder(
                  controller: _scroll,
                  padding: const EdgeInsets.all(12),
                  itemCount: _entries.length,
                  itemBuilder: (_, i) => _Bubble(entry: _entries[i]),
                ),
        ),
        Padding(
          padding: const EdgeInsets.all(8),
          child: Row(children: [
            Expanded(
              child: TextField(
                controller: _input,
                onSubmitted: (_) => _send(),
                decoration: InputDecoration(
                  hintText: _pai == null
                      ? 'waiting for core…'
                      : 'message — try "remember that I like tea"',
                  border: const OutlineInputBorder(),
                  isDense: true,
                ),
              ),
            ),
            const SizedBox(width: 8),
            IconButton.filled(
                onPressed: _send, icon: const Icon(Icons.send)),
          ]),
        ),
      ]),
    );
  }
}

class _Bubble extends StatelessWidget {
  const _Bubble({required this.entry});
  final _Entry entry;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final e = entry;
    final isYou = e.role == 'you';
    final isSys = e.role == 'system';
    return Align(
      alignment: isYou ? Alignment.centerRight : Alignment.centerLeft,
      child: Container(
        margin: const EdgeInsets.symmetric(vertical: 4),
        padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
        constraints: const BoxConstraints(maxWidth: 560),
        decoration: BoxDecoration(
          color: isYou
              ? cs.primaryContainer
              : isSys
                  ? cs.surfaceContainerHighest.withValues(alpha: 0.5)
                  : cs.secondaryContainer,
          borderRadius: BorderRadius.circular(14),
        ),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Text(
            isYou ? 'you' : isSys ? 'event' : 'ai',
            style: TextStyle(
                fontSize: 11,
                fontWeight: FontWeight.w600,
                color: cs.onSecondaryContainer.withValues(alpha: 0.6)),
          ),
          if (e.text.isNotEmpty || !e.streaming) Text(e.text),
          if (e.streaming && e.text.isEmpty)
            const SizedBox(
                height: 16,
                width: 16,
                child: CircularProgressIndicator(strokeWidth: 2)),
          if (e.sub.isNotEmpty)
            Text(e.sub,
                style: TextStyle(
                    fontSize: 10,
                    color: cs.onSecondaryContainer.withValues(alpha: 0.5))),
        ]),
      ),
    );
  }
}

/// Modal approval sheet — Approve/Deny resolve the pending tool call via
/// `pai_approve`; closing without a choice denies (fail closed).
class _ApprovalSheet extends StatelessWidget {
  const _ApprovalSheet({required this.req});
  final Map<String, dynamic> req;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final perms = (req['permissions'] as List? ?? []).join(', ');
    return Padding(
      padding: const EdgeInsets.all(24),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(children: [
            Icon(Icons.shield_outlined, color: cs.primary),
            const SizedBox(width: 8),
            Text('Approval needed',
                style: Theme.of(context).textTheme.titleLarge),
          ]),
          const SizedBox(height: 16),
          Text('${req['tool']}',
              style: const TextStyle(fontWeight: FontWeight.bold)),
          Text('${req['summary']}'),
          const SizedBox(height: 8),
          Wrap(spacing: 6, children: [
            Chip(
              label: Text('risk: ${req['risk']}',
                  style: const TextStyle(fontSize: 11)),
              visualDensity: VisualDensity.compact,
            ),
            Chip(
              label: Text(perms, style: const TextStyle(fontSize: 11)),
              visualDensity: VisualDensity.compact,
            ),
          ]),
          const SizedBox(height: 20),
          Row(mainAxisAlignment: MainAxisAlignment.end, children: [
            TextButton(
              onPressed: () => Navigator.of(context).pop(false),
              child: const Text('Deny'),
            ),
            const SizedBox(width: 8),
            FilledButton.icon(
              onPressed: () => Navigator.of(context).pop(true),
              icon: const Icon(Icons.check),
              label: const Text('Approve'),
            ),
          ]),
        ],
      ),
    );
  }
}

class _ConvDrawer extends StatelessWidget {
  const _ConvDrawer({
    required this.convs,
    required this.interrupted,
    required this.onSelect,
    required this.onNew,
    required this.onResume,
    required this.onRename,
    required this.onDelete,
    required this.onScope,
  });
  final List<dynamic> convs;
  final List<dynamic> interrupted;
  final void Function(String id) onSelect;
  final void Function({bool isolated}) onNew;
  final void Function(String runId) onResume;
  final void Function(String id, String title) onRename;
  final void Function(String id) onDelete;
  final void Function(String id, String mode) onScope;

  @override
  Widget build(BuildContext context) {
    return Drawer(
      child: SafeArea(
        child: ListView(children: [
          Padding(
            padding: const EdgeInsets.all(16),
            child: Row(children: [
              Text('Chats', style: Theme.of(context).textTheme.titleMedium),
              const Spacer(),
              IconButton(
                  tooltip: 'New chat',
                  icon: const Icon(Icons.add),
                  onPressed: () => onNew()),
              IconButton(
                  tooltip: 'New isolated chat (private memory)',
                  icon: const Icon(Icons.enhanced_encryption_outlined),
                  onPressed: () => onNew(isolated: true)),
            ]),
          ),
          for (final c in convs)
            ListTile(
              selected: c['active'] == true,
              leading: Icon(c['memory'] == 'isolated'
                  ? Icons.lock_outline
                  : Icons.chat_bubble_outline),
              title: Text('${c['title'] ?? 'Untitled'}',
                  maxLines: 1, overflow: TextOverflow.ellipsis),
              subtitle: Text('${c['memory']}',
                  style: const TextStyle(fontSize: 11)),
              onTap: () => onSelect(c['id'] as String),
              trailing: PopupMenuButton<String>(
                itemBuilder: (_) => [
                  const PopupMenuItem(value: 'rename', child: Text('Rename')),
                  PopupMenuItem(
                      value: 'scope',
                      child: Text(c['memory'] == 'isolated'
                          ? 'Share memory'
                          : 'Isolate memory')),
                  const PopupMenuItem(value: 'delete', child: Text('Delete')),
                ],
                onSelected: (v) => _menu(context, v, c),
              ),
            ),
          if (interrupted.isNotEmpty) ...[
            const Divider(),
            const Padding(
                padding: EdgeInsets.all(16),
                child: Text('Interrupted runs')),
            for (final r in interrupted)
              ListTile(
                leading: const Icon(Icons.replay),
                title: Text('${(r['id'] as String).substring(0, 8)} · ${r['state']}'),
                subtitle: Text('${r['started_at']}',
                    style: const TextStyle(fontSize: 11)),
                trailing: IconButton(
                    icon: const Icon(Icons.play_arrow),
                    onPressed: () => onResume(r['id'] as String)),
              ),
          ],
        ]),
      ),
    );
  }

  void _menu(BuildContext context, String v, dynamic c) {
    switch (v) {
      case 'rename':
        final ctrl = TextEditingController(text: c['title'] as String? ?? '');
        showDialog(
            context: context,
            builder: (ctx) => AlertDialog(
                  title: const Text('Rename chat'),
                  content: TextField(controller: ctrl, autofocus: true),
                  actions: [
                    TextButton(
                        onPressed: () => Navigator.pop(ctx),
                        child: const Text('Cancel')),
                    FilledButton(
                        onPressed: () {
                          onRename(c['id'] as String, ctrl.text.trim());
                          Navigator.pop(ctx);
                        },
                        child: const Text('Save')),
                  ],
                ));
      case 'scope':
        onScope(c['id'] as String,
            c['memory'] == 'isolated' ? 'shared' : 'isolated');
      case 'delete':
        showDialog(
            context: context,
            builder: (ctx) => AlertDialog(
                  title: const Text('Delete chat?'),
                  content:
                      const Text('Messages are removed; memories are kept.'),
                  actions: [
                    TextButton(
                        onPressed: () => Navigator.pop(ctx),
                        child: const Text('Cancel')),
                    FilledButton(
                        style: FilledButton.styleFrom(
                            backgroundColor:
                                Theme.of(ctx).colorScheme.error),
                        onPressed: () {
                          onDelete(c['id'] as String);
                          Navigator.pop(ctx);
                        },
                        child: const Text('Delete')),
                  ],
                ));
    }
  }
}

/// Memory browser: every remembered item with scope/privacy provenance and
/// a forget action (soft-delete + audit record).
class MemoriesScreen extends StatefulWidget {
  const MemoriesScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<MemoriesScreen> createState() => _MemoriesScreenState();
}

class _MemoriesScreenState extends State<MemoriesScreen> {
  List<dynamic> _items = const [];
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    final items = await widget.bridge.memories();
    if (mounted) setState(() { _items = items; _loading = false; });
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(title: const Text('Memories')),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : _items.isEmpty
              ? const Center(child: Text('nothing remembered yet'))
              : ListView.builder(
                  itemCount: _items.length,
                  itemBuilder: (_, i) {
                    final m = _items[i];
                    final conv = m['conversation'];
                    return Card(
                      margin: const EdgeInsets.symmetric(
                          horizontal: 12, vertical: 4),
                      child: ListTile(
                        title: Text('${m['content']}'),
                        subtitle: Wrap(spacing: 6, children: [
                          Chip(
                              label: Text('${m['scope']}',
                                  style: const TextStyle(fontSize: 10)),
                              visualDensity: VisualDensity.compact),
                          Chip(
                              label: Text('${m['source']}',
                                  style: const TextStyle(fontSize: 10)),
                              visualDensity: VisualDensity.compact),
                          Chip(
                              label: Text(
                                  conv == null
                                      ? 'global'
                                      : 'chat ${(conv as String).substring(0, 8)}',
                                  style: const TextStyle(fontSize: 10)),
                              visualDensity: VisualDensity.compact),
                          Chip(
                              label: Text('${m['privacy']}',
                                  style: const TextStyle(fontSize: 10)),
                              visualDensity: VisualDensity.compact),
                        ]),
                        trailing: IconButton(
                          icon: Icon(Icons.delete_outline, color: cs.error),
                          tooltip: 'Forget',
                          onPressed: () => _forget(m),
                        ),
                      ),
                    );
                  },
                ),
    );
  }

  Future<void> _forget(dynamic m) async {
    final ok = await showDialog<bool>(
        context: context,
        builder: (ctx) => AlertDialog(
              title: const Text('Forget this?'),
              content: Text('"${m['content']}"'),
              actions: [
                TextButton(
                    onPressed: () => Navigator.pop(ctx, false),
                    child: const Text('Cancel')),
                FilledButton(
                    onPressed: () => Navigator.pop(ctx, true),
                    child: const Text('Forget')),
              ],
            ));
    if (ok == true) {
      await widget.bridge.forget(m['id'] as String);
      await _load();
    }
  }
}

/// Documents — ingest files for RAG, browse, search, delete.
/// Everything stays local; sections get embeddings when an Ollama
/// embedding model is installed.
class DocumentsScreen extends StatefulWidget {
  const DocumentsScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<DocumentsScreen> createState() => _DocumentsScreenState();
}

class _DocumentsScreenState extends State<DocumentsScreen> {
  List<dynamic> _items = const [];
  List<dynamic> _hits = const [];
  bool _loading = true;
  final _pathCtrl = TextEditingController();
  final _searchCtrl = TextEditingController();

  @override
  void initState() {
    super.initState();
    _load();
  }

  @override
  void dispose() {
    _pathCtrl.dispose();
    _searchCtrl.dispose();
    super.dispose();
  }

  Future<void> _load() async {
    final items = await widget.bridge.docs();
    if (mounted) setState(() { _items = items; _loading = false; });
  }

  Future<void> _ingest() async {
    final path = _pathCtrl.text.trim();
    if (path.isEmpty) return;
    final r = await widget.bridge.docsIngest(path);
    _pathCtrl.clear();
    if (r['error'] != null && mounted) {
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text('${r['error']}')));
    }
    await _load();
  }

  Future<void> _search() async {
    final q = _searchCtrl.text.trim();
    if (q.isEmpty) {
      setState(() => _hits = const []);
      return;
    }
    final hits = await widget.bridge.docsSearch(q);
    if (mounted) setState(() => _hits = hits);
  }

  Future<void> _delete(dynamic d) async {
    final ok = await showDialog<bool>(
        context: context,
        builder: (ctx) => AlertDialog(
              title: const Text('Delete this document?'),
              content: Text('"${d['title'] ?? d['id']}"'),
              actions: [
                TextButton(
                    onPressed: () => Navigator.pop(ctx, false),
                    child: const Text('Cancel')),
                FilledButton(
                    onPressed: () => Navigator.pop(ctx, true),
                    child: const Text('Delete')),
              ],
            ));
    if (ok == true) {
      await widget.bridge.docsDelete(d['id'] as String);
      await _load();
    }
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(title: const Text('Documents')),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : Column(children: [
              Padding(
                padding: const EdgeInsets.all(12),
                child: Row(children: [
                  Expanded(
                    child: TextField(
                      controller: _pathCtrl,
                      decoration: const InputDecoration(
                          labelText: 'Ingest file path',
                          hintText: r'C:\path\to\notes.md'),
                      onSubmitted: (_) => _ingest(),
                    ),
                  ),
                  const SizedBox(width: 8),
                  FilledButton.icon(
                      onPressed: _ingest,
                      icon: const Icon(Icons.upload_file),
                      label: const Text('Ingest')),
                ]),
              ),
              Padding(
                padding: const EdgeInsets.symmetric(horizontal: 12),
                child: Row(children: [
                  Expanded(
                    child: TextField(
                      controller: _searchCtrl,
                      decoration: const InputDecoration(
                          labelText: 'Search sections',
                          hintText: 'keyword or phrase'),
                      onSubmitted: (_) => _search(),
                    ),
                  ),
                  const SizedBox(width: 8),
                  IconButton(
                      onPressed: _search, icon: const Icon(Icons.search)),
                ]),
              ),
              if (_hits.isNotEmpty)
                SizedBox(
                  height: 180,
                  child: ListView.builder(
                    itemCount: _hits.length,
                    itemBuilder: (_, i) {
                      final h = _hits[i];
                      return ListTile(
                        dense: true,
                        leading: Text('[D${i + 1}]',
                            style: TextStyle(color: cs.primary)),
                        title: Text('${h['title'] ?? "untitled"} §${h['section']}',
                            style: const TextStyle(fontSize: 12)),
                        subtitle: Text('${h['snippet']}',
                            maxLines: 2, overflow: TextOverflow.ellipsis),
                      );
                    },
                  ),
                ),
              const Divider(),
              Expanded(
                child: _items.isEmpty
                    ? const Center(
                        child: Text('no documents ingested yet'))
                    : ListView.builder(
                        itemCount: _items.length,
                        itemBuilder: (_, i) {
                          final d = _items[i];
                          return ListTile(
                            leading: const Icon(Icons.article_outlined),
                            title:
                                Text('${d['title'] ?? "untitled"}'),
                            subtitle: Text(
                                '${d['mime']}  •  ${d['sections']} sections'),
                            trailing: IconButton(
                              icon: Icon(Icons.delete_outline,
                                  color: cs.error),
                              tooltip: 'Delete',
                              onPressed: () => _delete(d),
                            ),
                          );
                        },
                      ),
              ),
            ]),
    );
  }
}

/// Mailbox view — search + read via the email connector. Send/delete stay
/// tool-side and approval-gated; drafts can be composed here.
class EmailScreen extends StatefulWidget {
  const EmailScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<EmailScreen> createState() => _EmailScreenState();
}

class _EmailScreenState extends State<EmailScreen> {
  List<dynamic> _hits = const [];
  bool _loading = true;
  String? _error;
  final _searchCtrl = TextEditingController();

  @override
  void initState() {
    super.initState();
    _search();
  }

  @override
  void dispose() {
    _searchCtrl.dispose();
    super.dispose();
  }

  Future<void> _search() async {
    setState(() => _loading = true);
    final r = await widget.bridge.emailSearch(
        query: _searchCtrl.text.trim().isEmpty ? null : _searchCtrl.text.trim());
    if (!mounted) return;
    if (r['error'] != null) {
      setState(() { _error = '${r['error']}'; _hits = const []; _loading = false; });
    } else {
      setState(() { _error = null; _hits = r['results'] as List<dynamic>? ?? const []; _loading = false; });
    }
  }

  Future<void> _open(dynamic m) async {
    final r = await widget.bridge.emailRead('${m['id']}');
    if (!mounted) return;
    if (r['error'] != null) {
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text('${r['error']}')));
      return;
    }
    final msg = r['message'] as Map<String, dynamic>;
    showDialog<void>(
        context: context,
        builder: (ctx) => AlertDialog(
              title: Text(msg['subject'] ?? '(no subject)'),
              content: SingleChildScrollView(
                  child: Text(msg['body_text'] ?? '(empty)')),
              actions: [
                TextButton(
                    onPressed: () {
                      Navigator.of(ctx).pop();
                      _compose(
                          to: (msg['from'] as Map?)?['address'] as String? ?? '',
                          subject: 'Re: ${msg['subject'] ?? ''}',
                          inReplyTo: '${msg['id']}');
                    },
                    child: const Text('Draft reply')),
                TextButton(
                    onPressed: () => Navigator.of(ctx).pop(),
                    child: const Text('Close')),
              ],
            ));
  }

  Future<void> _compose(
      {String to = '', String subject = '', String? inReplyTo}) async {
    final toCtrl = TextEditingController(text: to);
    final subjCtrl = TextEditingController(text: subject);
    final bodyCtrl = TextEditingController();
    final ok = await showDialog<bool>(
        context: context,
        builder: (ctx) => AlertDialog(
              title: const Text('New draft'),
              content: SizedBox(
                  width: 480,
                  child: Column(mainAxisSize: MainAxisSize.min, children: [
                    TextField(
                        controller: toCtrl,
                        decoration: const InputDecoration(labelText: 'To')),
                    TextField(
                        controller: subjCtrl,
                        decoration: const InputDecoration(labelText: 'Subject')),
                    TextField(
                        controller: bodyCtrl,
                        maxLines: 8,
                        decoration: const InputDecoration(labelText: 'Body')),
                  ])),
              actions: [
                TextButton(
                    onPressed: () => Navigator.of(ctx).pop(false),
                    child: const Text('Cancel')),
                FilledButton(
                    onPressed: () => Navigator.of(ctx).pop(true),
                    child: const Text('Save draft')),
              ],
            ));
    if (ok != true) return;
    final r = await widget.bridge.emailDraft(
        to: [toCtrl.text.trim()],
        subject: subjCtrl.text.trim(),
        body: bodyCtrl.text,
        inReplyTo: inReplyTo);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? '${r['error']}'
            : 'Draft saved (${r['draft_id']})')));
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
        appBar: AppBar(
            title: const Text('Email'),
            actions: [
              IconButton(
                  icon: const Icon(Icons.edit_outlined),
                  tooltip: 'New draft',
                  onPressed: () => _compose()),
              IconButton(
                  icon: const Icon(Icons.refresh),
                  onPressed: _search),
            ]),
        body: Column(children: [
          Padding(
              padding: const EdgeInsets.all(8),
              child: TextField(
                  controller: _searchCtrl,
                  decoration: const InputDecoration(
                      hintText: 'Search mail',
                      prefixIcon: Icon(Icons.search)),
                  onSubmitted: (_) => _search())),
          if (_error != null)
            Padding(
                padding: const EdgeInsets.all(12),
                child: Text(_error!,
                    style: TextStyle(
                        color: Theme.of(context).colorScheme.error))),
          Expanded(
              child: _loading
                  ? const Center(child: CircularProgressIndicator())
                  : ListView.builder(
                      itemCount: _hits.length,
                      itemBuilder: (ctx, i) {
                        final m = _hits[i] as Map<String, dynamic>;
                        return ListTile(
                            leading: Icon((m['flags'] as List?)
                                        ?.contains('\\Seen') ==
                                    true
                                ? Icons.mark_email_read_outlined
                                : Icons.mark_email_unread_outlined),
                            title: Text(m['subject'] ?? '(no subject)',
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis),
                            subtitle: Text(
                                '${m['from'] ?? ''} — ${m['snippet'] ?? ''}',
                                maxLines: 2,
                                overflow: TextOverflow.ellipsis),
                            onTap: () => _open(m));
                      })),
        ]));
  }
}

/// Policy editor — every permission, its current policy, and a selector.
/// Changes apply immediately and persist in the local DB.
class PoliciesScreen extends StatefulWidget {
  const PoliciesScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<PoliciesScreen> createState() => _PoliciesScreenState();
}

class _PoliciesScreenState extends State<PoliciesScreen> {
  List<dynamic> _rows = const [];
  bool _loading = true;

  static const _policies = [
    'ALWAYS_ALLOW',
    'ASK_USER',
    'ALLOW_WITH_RULE',
    'NEVER_ALLOW',
  ];
  static const _labels = {
    'ALWAYS_ALLOW': 'Always allow',
    'ASK_USER': 'Ask me first',
    'ALLOW_WITH_RULE': 'Allow by rule',
    'NEVER_ALLOW': 'Never allow',
  };

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    final rows = await widget.bridge.policies();
    if (mounted) setState(() { _rows = rows; _loading = false; });
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Permissions')),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : ListView.builder(
              itemCount: _rows.length,
              itemBuilder: (_, i) {
                final r = _rows[i];
                return ListTile(
                  title: Text('${r['permission']}',
                      style: const TextStyle(
                          fontFamily: 'monospace', fontSize: 13)),
                  trailing: DropdownButton<String>(
                    value: '${r['policy']}',
                    underline: const SizedBox.shrink(),
                    items: _policies
                        .map((p) => DropdownMenuItem(
                            value: p, child: Text(_labels[p] ?? p)))
                        .toList(),
                    onChanged: (v) async {
                      if (v == null) return;
                      await widget.bridge
                          .setPolicy('${r['permission']}', v);
                      await _load();
                    },
                  ),
                );
              },
            ),
    );
  }
}
