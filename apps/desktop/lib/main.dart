import 'dart:async';
import 'dart:math' as math;
import 'dart:convert';
import 'dart:io';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
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
        home: const HomeShell(),
      );
}

/// App shell — one shared `PaiBridge`, a labeled rail for every surface,
/// and lazy tab construction (each screen builds on first visit, keeps
/// state after).
class HomeShell extends StatefulWidget {
  const HomeShell({super.key});
  @override
  State<HomeShell> createState() => _HomeShellState();
}

/// The ten surfaces, in rail order — shared by the wide rail, the
/// compact icon-rail, and the narrow bottom bar.
const _dests = [
  (Icons.chat_bubble_outline, 'Chat'),
  (Icons.apps_outlined, 'Apps'),
  (Icons.devices_outlined, 'Devices'),
  (Icons.notifications_outlined, 'Alerts'),
  (Icons.psychology_outlined, 'Memories'),
  (Icons.description_outlined, 'Documents'),
  (Icons.mail_outline, 'Email'),
  (Icons.receipt_long_outlined, 'Activity'),
  (Icons.policy_outlined, 'Permissions'),
  (Icons.music_note_outlined, 'Media'),
];

/// Ctrl+1..9,0 jump straight to a rail destination.
const _railKeys = [
  LogicalKeyboardKey.digit1,
  LogicalKeyboardKey.digit2,
  LogicalKeyboardKey.digit3,
  LogicalKeyboardKey.digit4,
  LogicalKeyboardKey.digit5,
  LogicalKeyboardKey.digit6,
  LogicalKeyboardKey.digit7,
  LogicalKeyboardKey.digit8,
  LogicalKeyboardKey.digit9,
  LogicalKeyboardKey.digit0,
];

class _HomeShellState extends State<HomeShell> {
  PaiBridge? _pai;
  String? _error;
  int _index = 0;
  int _unread = 0;
  Map<String, dynamic> _status = const {};
  StreamSubscription? _statusSub;
  final _visited = <int>{0};
  final _chatKey = GlobalKey<_ChatScreenState>();

  @override
  void initState() {
    super.initState();
    _init();
  }

  Future<void> _init() async {
    final dataDir = Platform.environment['PAI_DATA_DIR'] ??
        (Platform.isAndroid
            // The app's private files dir — always writable, no plugin
            // needed (path_provider is absent: build host lacks symlink
            // privilege).
            ? '/data/data/com.example.pai_app/files'
            : '${Directory.current.path}/.pai-data');
    // 'auto' probes llama-server / Ollama / LM Studio, falls back to echo.
    final provider = Platform.environment['PAI_PROVIDER'] ?? 'auto';
    try {
      final bridge = await PaiBridge.start(
          {'data_dir': dataDir, 'provider': provider});
      if (!mounted) return;
      setState(() => _pai = bridge);
      _statusSub = bridge.statusStream.listen((st) {
        if (mounted) setState(() => _status = st);
      });
      try {
        await bridge.status();
      } catch (_) {}
      _refreshUnread();
    } catch (e) {
      setState(() => _error = 'Core init failed: $e\n'
          '(build the core: cargo build -p pai-ffi)');
    }
  }

  Future<void> _refreshUnread() async {
    if (_pai == null) return;
    try {
      final r = await _pai!.notifyList(unreadOnly: true);
      if (mounted) {
        setState(() => _unread = (r['unread'] as num? ?? 0).toInt());
      }
    } catch (_) {}
  }

  void _select(int i) {
    setState(() {
      _index = i;
      _visited.add(i);
    });
    if (i == 3 || _unread > 0) _refreshUnread();
  }

  @override
  void dispose() {
    _statusSub?.cancel();
    super.dispose();
  }

  /// Rail-bottom provider health: green when a real model serves chat,
  /// amber on the echo fallback, grey until status lands.
  Color _healthColor(ColorScheme cs) {
    if (_status.isEmpty) return cs.onSurfaceVariant.withValues(alpha: 0.4);
    return _status['provider'] == 'echo' ? Colors.amber : Colors.greenAccent;
  }

  /// Destination icon — Alerts carries the unread-count badge.
  Widget _navIcon(int i) {
    final icon = Icon(_dests[i].$1);
    if (i != 3) return icon;
    return Badge.count(
        count: _unread, isLabelVisible: _unread > 0, child: icon);
  }

  String _healthMsg() {
    if (_status.isEmpty) return 'Provider status unknown';
    return _status['provider'] == 'echo'
        ? 'No local model found — using echo fallback. '
            'The Devices tab shows what is live.'
        : 'Serving: ${_status['provider']} · ${_status['model']}';
  }

  Widget _tab(int i) {
    if (!_visited.contains(i)) return const SizedBox.shrink();
    final pai = _pai!;
    switch (i) {
      case 0:
        return ChatScreen(key: _chatKey, bridge: pai);
      case 1:
        return AppsScreen(bridge: pai);
      case 2:
        return DevicesScreen(bridge: pai);
      case 3:
        return NotificationsScreen(bridge: pai, onChanged: _refreshUnread);
      case 4:
        return MemoriesScreen(bridge: pai);
      case 5:
        return DocumentsScreen(bridge: pai);
      case 6:
        return EmailScreen(bridge: pai);
      case 7:
        return ActivityScreen(bridge: pai);
      case 8:
        return PoliciesScreen(bridge: pai);
      case 9:
        return MediaScreen(bridge: pai);
      default:
        return const SizedBox.shrink();
    }
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    if (_pai == null) {
      return Scaffold(
          body: Center(
              child: _error != null
                  ? Padding(
                      padding: const EdgeInsets.all(24),
                      child: Text(_error!,
                          textAlign: TextAlign.center,
                          style: TextStyle(color: cs.error)))
                  : const Column(mainAxisSize: MainAxisSize.min, children: [
                      CircularProgressIndicator(),
                      SizedBox(height: 16),
                      Text('Starting core…'),
                    ])));
    }
    return CallbackShortcuts(
      bindings: {
        for (var i = 0; i < _railKeys.length; i++)
          SingleActivator(_railKeys[i], control: true): () => _select(i),
        const SingleActivator(LogicalKeyboardKey.keyK, control: true):
            () {
          _select(0);
          _chatKey.currentState?.focusInput();
        },
        const SingleActivator(LogicalKeyboardKey.keyN, control: true):
            () {
          _select(0);
          _chatKey.currentState?.newConversation();
        },
      },
      child: LayoutBuilder(
        builder: (_, c) {
          final content = IndexedStack(
              index: _index, children: List.generate(10, _tab));
          // Narrow windows (and the Android build): bottom bar with
          // labels on the selected destination only — all ten fit.
          if (c.maxWidth < 640) {
            return Scaffold(
              body: content,
              bottomNavigationBar: NavigationBar(
                selectedIndex: _index,
                onDestinationSelected: _select,
                labelBehavior:
                    NavigationDestinationLabelBehavior.onlyShowSelected,
                destinations: [
                  for (var i = 0; i < _dests.length; i++)
                    NavigationDestination(
                        icon: _navIcon(i), label: _dests[i].$2),
                ],
              ),
            );
          }
          final wide = c.maxWidth > 1120;
          // Ten destinations outgrow short windows — let the rail scroll.
          // SizedBox keeps height bounded so the trailing health dot can
          // still dock at the bottom on tall windows.
          final rail = LayoutBuilder(
            builder: (_, rc) {
              final overflow = rc.maxHeight < 10 * 72 + 96;
              final scrollable = SingleChildScrollView(
                child: SizedBox(
                  height: math.max(rc.maxHeight, 10 * 72 + 96),
                  child: NavigationRail(
                    selectedIndex: _index,
                    onDestinationSelected: _select,
                    labelType: wide
                        ? NavigationRailLabelType.all
                        : NavigationRailLabelType.none,
                    trailing: Expanded(
                      child: Align(
                        alignment: Alignment.bottomCenter,
                        child: Padding(
                          padding: const EdgeInsets.only(bottom: 14),
                          child: Tooltip(
                            message: _healthMsg(),
                            child: Icon(Icons.circle,
                                size: 10,
                                color: _healthColor(
                                    Theme.of(context).colorScheme)),
                          ),
                        ),
                      ),
                    ),
                    destinations: [
                      for (var i = 0; i < _dests.length; i++)
                        NavigationRailDestination(
                            icon: wide
                                ? _navIcon(i)
                                : Tooltip(
                                    message: _dests[i].$2,
                                    child: _navIcon(i)),
                            label: Text(_dests[i].$2)),
                    ],
                  ),
                ),
              );
              return overflow
                  ? Scrollbar(
                      thumbVisibility: true, thickness: 4, child: scrollable)
                  : scrollable;
            },
          );
          return Scaffold(
            body: Row(children: [
              rail,
              const VerticalDivider(width: 1),
              Expanded(child: content),
            ]),
          );
        },
      ),
    );
  }
}

/// One chat transcript row. `streaming` marks the in-flight assistant reply.
class _Entry {
  _Entry({required this.role, this.text = '', this.sub = '', this.streaming = false, DateTime? at})
      : at = at ?? DateTime.now();
  final String role; // you | ai | system
  final DateTime at;
  String text;
  String sub;
  bool streaming;
  bool isError = false;
}

class ChatScreen extends StatefulWidget {
  const ChatScreen({super.key, required this.bridge});
  final PaiBridge? bridge;
  @override
  State<ChatScreen> createState() => _ChatScreenState();
}

class _ChatScreenState extends State<ChatScreen> {
  final _input = TextEditingController();
  final _inputFocus = FocusNode();

  /// Ctrl+K lands here — jump focus to the message field.
  void focusInput() => _inputFocus.requestFocus();

  /// Ctrl+N lands here — same as the drawer's New chat button.
  void newConversation() => _newConversation();
  final _scroll = ScrollController();
  PaiBridge? get _pai => widget.bridge;
  bool _ready = false;
  String? _error;
  bool _sending = false;
  String _lastUserText = '';
  bool _listening = false;
  bool _speakReplies = false;
  Map<String, dynamic> _voice = const {};
  Map<String, dynamic> _status = const {};
  StreamSubscription<Map<String, dynamic>>? _statusSub;
  final _entries = <_Entry>[];
  List<dynamic> _convs = const [];
  List<dynamic> _interrupted = const [];

  @override
  void initState() {
    super.initState();
    if (_pai != null) _onReady();
  }

  @override
  void dispose() {
    _statusSub?.cancel();
    _inputFocus.dispose();
    _input.dispose();
    _scroll.dispose();
    super.dispose();
  }

  @override
  void didUpdateWidget(ChatScreen old) {
    super.didUpdateWidget(old);
    if (old.bridge == null && _pai != null) _onReady();
  }

  Future<void> _onReady() async {
    if (_ready) return;
    _ready = true;
    try {
      await _loadHistory();
      await _refreshConvs();
      final runs = await _pai!.runs();
      if (mounted) setState(() => _interrupted = runs);
      final voice = await _pai!.voiceStatus();
      if (mounted) setState(() => _voice = voice);
      _statusSub ??= _pai!.statusStream.listen((st) {
        if (mounted) setState(() => _status = st);
      });
      await _pai!.status();
    } catch (e) {
      if (mounted) setState(() => _error = 'Load failed: $e');
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
    DateTime? at;
    if (m['created_at'] is String) {
      at = DateTime.tryParse(m['created_at'] as String)?.toLocal();
    }
    return _Entry(role: role == 'user' ? 'you' : 'ai', text: text, at: at);
  }

  /// Plugin-free export: the visible transcript goes to the clipboard
  /// as "You (HH:MM): ..." lines.
  void _copyTranscript() {
    final buf = StringBuffer();
    for (final e in _entries) {
      if (e.streaming || e.text.isEmpty) continue;
      final who =
          e.role == 'you' ? 'You' : e.role == 'system' ? 'Event' : 'Assistant';
      buf.writeln('$who (${_fmtHm(e.at)}): ${e.text}\n');
    }
    Clipboard.setData(ClipboardData(text: buf.toString()));
    ScaffoldMessenger.of(context).showSnackBar(const SnackBar(
        content: Text('Transcript copied to clipboard'),
        duration: Duration(seconds: 2)));
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
    _lastUserText = text;
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
    // A fresh conversation may have been created lazily by the send —
    // keep the drawer listing honest.
    _refreshConvs();
    if (_speakReplies && ai.text.isNotEmpty && !ai.isError) {
      // Fire-and-forget: playback happens on the host speaker inside the
      // worker isolate; failures surface as a banner, not a crash.
      _pai!.voiceSay(ai.text).then((res) {
        if (res['error'] != null && mounted) {
          setState(() => _error = 'voice: ${res['error']}');
        }
      });
    }
  }

  /// Attach → transcribe an audio file via whisper-server; the text
  /// lands in the input for review.
  Future<void> _attachTranscribe() async {
    if (_pai == null) return;
    final ctrl = TextEditingController();
    final path = await showDialog<String>(
        context: context,
        builder: (ctx) => AlertDialog(
              title: const Text('Transcribe audio file'),
              content: TextField(
                  controller: ctrl,
                  autofocus: true,
                  decoration: const InputDecoration(
                      hintText: r'C:\path\to\audio.wav'),
                  onSubmitted: (v) => Navigator.pop(ctx, v)),
              actions: [
                TextButton(
                    onPressed: () => Navigator.pop(ctx),
                    child: const Text('Cancel')),
                FilledButton(
                    onPressed: () => Navigator.pop(ctx, ctrl.text),
                    child: const Text('Transcribe')),
              ],
            ));
    final p = path?.trim() ?? '';
    if (p.isEmpty || !mounted) return;
    final r = await _pai!.voiceTranscribe(p);
    if (!mounted) return;
    if (r['error'] != null) {
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text('${r['error']}')));
      return;
    }
    final text = (r['text'] as String? ?? '').trim();
    if (text.isNotEmpty) {
      setState(() {
        _input.text =
            _input.text.isEmpty ? text : '${_input.text} $text';
        _input.selection =
            TextSelection.collapsed(offset: _input.text.length);
      });
    }
  }

  /// Push-to-talk: capture one utterance, drop the transcript into the
  /// input field for review (the user still presses send).
  Future<void> _listen() async {
    if (_pai == null || _listening) return;
    setState(() => _listening = true);
    try {
      final res = await _pai!.voiceListen();
      if (!mounted) return;
      if (res['error'] != null) {
        setState(() => _error = 'voice: ${res['error']}');
      } else if (res['heard'] == true) {
        final text = (res['text'] as String?)?.trim() ?? '';
        setState(() {
          if (text.isNotEmpty) {
            _input.text =
                _input.text.isEmpty ? text : '${_input.text} $text';
            _input.selection = TextSelection.collapsed(
                offset: _input.text.length);
          } else {
            _error =
                'Heard you, but whisper-server is not configured — see `pai voice status`';
          }
        });
      }
    } finally {
      if (mounted) setState(() => _listening = false);
    }
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
        title: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              const Text('Personal AI'),
              if (_status.isNotEmpty)
                Text(
                  '${_status['provider']}'
                  '${_status['model'] != null ? ' · ${_status['model']}' : ''}',
                  style: TextStyle(fontSize: 12, color: cs.onSurfaceVariant),
                ),
            ]),
        actions: [
          if (_entries.any((e) => !e.streaming && e.text.isNotEmpty))
            IconButton(
              icon: const Icon(Icons.copy_all_outlined),
              tooltip: 'Copy transcript',
              onPressed: _copyTranscript,
            ),
          if (_sending)
            IconButton(
              icon: const Icon(Icons.stop_circle_outlined),
              tooltip: 'Cancel run',
              onPressed: () => _pai?.cancel(),
            ),
          if (_voice['tts'] == true && _voice['speaker'] == true)
            IconButton(
              icon: Icon(_speakReplies
                  ? Icons.volume_up
                  : Icons.volume_off_outlined),
              tooltip: _speakReplies
                  ? 'Speaking replies — tap to mute'
                  : 'Speak replies aloud (piper)',
              onPressed: () =>
                  setState(() => _speakReplies = !_speakReplies),
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
                final messenger = ScaffoldMessenger.of(context);
                final navigator = Navigator.of(context);
                final r = await _pai!.conversationDelete(id);
                if (!mounted) return;
                final err = r['error'] as String?;
                if (err != null) {
                  setState(() => _error = 'Delete failed: $err');
                  return;
                }
                if (r['was_active'] == true) setState(_entries.clear);
                await _refreshConvs();
                await _loadHistory();
                if (!mounted) return;
                navigator.maybePop();
                messenger.showSnackBar(
                    const SnackBar(content: Text('Chat deleted')));
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
                  child: Column(mainAxisSize: MainAxisSize.min, children: [
                    Icon(Icons.forum_outlined,
                        size: 40, color: cs.onSurfaceVariant),
                    const SizedBox(height: 12),
                    Text(
                      _pai == null
                          ? (_error == null ? 'Starting the core…' : '')
                          : 'No messages yet — ask anything.',
                      style: TextStyle(color: cs.onSurfaceVariant),
                    ),
                    if (_pai != null) ...[
                      Padding(
                        padding: const EdgeInsets.only(top: 4),
                        child: Text('local-first · private · auditable',
                            style: TextStyle(
                                fontSize: 11,
                                color: cs.onSurfaceVariant
                                    .withValues(alpha: 0.7))),
                      ),
                      const SizedBox(height: 16),
                      Wrap(
                          spacing: 8,
                          runSpacing: 8,
                          alignment: WrapAlignment.center,
                          children: [
                            for (final s in const [
                              'Remember that I like tea',
                              'What can you do?',
                              'Summarize my recent activity',
                            ])
                              ActionChip(
                                  label: Text(s),
                                  onPressed: () {
                                    _input.text = s;
                                    _send();
                                  }),
                          ]),
                    ],
                  ]))
              : ListView.builder(
                  controller: _scroll,
                  padding: const EdgeInsets.all(12),
                  itemCount: _entries.length,
                  itemBuilder: (_, i) {
                    final e = _entries[i];
                    return _Bubble(
                      entry: e,
                      onRetry: e.isError && _lastUserText.isNotEmpty
                          ? () {
                              _input.text = _lastUserText;
                              _send();
                            }
                          : null,
                    );
                  },
                ),
        ),
        Padding(
          padding: const EdgeInsets.all(8),
          child: Row(children: [
            Expanded(
              child: TextField(
                controller: _input,
                focusNode: _inputFocus,
                maxLines: null,
                textInputAction: TextInputAction.send,
                onChanged: (_) => setState(() {}),
                onSubmitted: (_) => _send(),
                decoration: InputDecoration(
                  hintText: _pai == null
                      ? 'Waiting for core…'
                      : 'Message — try "remember that I like tea"',
                  border: const OutlineInputBorder(),
                  isDense: true,
                ),
              ),
            ),
            const SizedBox(width: 8),
            IconButton(
              icon: const Icon(Icons.attach_file),
              tooltip: _voice['stt'] == true
                  ? 'Transcribe an audio file'
                  : 'Transcribe an audio file (needs whisper-server)',
              onPressed:
                  (_pai == null || _sending) ? null : _attachTranscribe,
            ),
            IconButton(
              icon: _listening
                  ? const SizedBox(
                      width: 18,
                      height: 18,
                      child: CircularProgressIndicator(strokeWidth: 2))
                  : Icon(Icons.mic,
                      color: _voice['mic'] == true
                          ? cs.primary
                          : cs.onSurfaceVariant),
              tooltip: _voice['mic'] == true
                  ? (_voice['stt'] == true
                      ? 'Dictate (mic → whisper)'
                      : 'Dictate (mic only — no whisper-server)')
                  : 'No microphone detected',
              onPressed: (_pai == null ||
                      _voice['mic'] != true ||
                      _listening ||
                      _sending)
                  ? null
                  : _listen,
            ),
            IconButton.filled(
                tooltip: 'Send',
                onPressed:
                    (_pai == null || _sending || _input.text.trim().isEmpty)
                        ? null
                        : _send,
                icon: const Icon(Icons.send)),
          ]),
        ),
      ]),
    );
  }
}

/// Shared skeleton rows shown while a list screen loads.
Widget _listSkeleton(BuildContext context) {
  final c = Theme.of(context)
      .colorScheme
      .surfaceContainerHighest
      .withValues(alpha: 0.45);
  return ListView(padding: const EdgeInsets.all(12), children: [
    for (var i = 0; i < 5; i++)
      Container(
          margin: const EdgeInsets.only(bottom: 10),
          height: 56,
          decoration: BoxDecoration(
              color: c, borderRadius: BorderRadius.circular(10))),
  ]);
}

/// Shared error block with a Retry action.
Widget _errorView(BuildContext context, String err, VoidCallback onRetry) {
  final cs = Theme.of(context).colorScheme;
  return Center(
      child: Column(mainAxisSize: MainAxisSize.min, children: [
    Icon(Icons.error_outline, color: cs.error, size: 32),
    const SizedBox(height: 8),
    Text(err, style: TextStyle(color: cs.error)),
    const SizedBox(height: 12),
    TextButton.icon(
        onPressed: onRetry,
        icon: const Icon(Icons.refresh, size: 16),
        label: const Text('Retry')),
  ]));
}

/// Approval-sheet risk styling.
Color _riskColor(ColorScheme cs, String risk) => switch (risk) {
      'high' || 'critical' => cs.error,
      'medium' || 'moderate' => Colors.amber,
      _ => Colors.greenAccent,
    };

IconData _riskIcon(String risk) => switch (risk) {
      'high' || 'critical' => Icons.warning_amber_outlined,
      'medium' || 'moderate' => Icons.error_outline,
      _ => Icons.verified_outlined,
    };

/// "HH:MM" for bubble labels.
String _fmtHm(DateTime t) =>
    '${t.hour.toString().padLeft(2, '0')}:${t.minute.toString().padLeft(2, '0')}';

class _Bubble extends StatefulWidget {
  const _Bubble({required this.entry, this.onRetry});
  final _Entry entry;
  final VoidCallback? onRetry;

  @override
  State<_Bubble> createState() => _BubbleState();
}

class _BubbleState extends State<_Bubble> {
  bool _hov = false;

  void _copy() {
    Clipboard.setData(ClipboardData(text: widget.entry.text));
    ScaffoldMessenger.of(context).showSnackBar(const SnackBar(
        content: Text('Copied'), duration: Duration(seconds: 1)));
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final e = widget.entry;
    final isYou = e.role == 'you';
    final isSys = e.role == 'system';
    return MouseRegion(
      onEnter: (_) => setState(() => _hov = true),
      onExit: (_) => setState(() => _hov = false),
      child: Align(
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
          Row(mainAxisSize: MainAxisSize.min, children: [
            Text(
              '${isYou ? 'You' : isSys ? 'Event' : 'Assistant'} · ${_fmtHm(e.at)}',
              style: TextStyle(
                  fontSize: 11,
                  fontWeight: FontWeight.w600,
                  color: cs.onSecondaryContainer.withValues(alpha: 0.6)),
            ),
            if (_hov && !e.streaming && e.text.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(left: 8),
                child: InkWell(
                  borderRadius: BorderRadius.circular(4),
                  onTap: _copy,
                  child: Icon(Icons.copy_outlined,
                      size: 12,
                      color:
                          cs.onSecondaryContainer.withValues(alpha: 0.6)),
                ),
              ),
          ]),
          if (e.text.isNotEmpty || !e.streaming) SelectableText(e.text),
          if (e.isError && e.text.contains('provider'))
            Text(
                'Check the provider endpoint — the Devices tab shows what\'s live',
                style: TextStyle(
                    fontSize: 10,
                    color: cs.onSecondaryContainer.withValues(alpha: 0.6))),
          if (e.isError && widget.onRetry != null)
            TextButton.icon(
                style: TextButton.styleFrom(
                    visualDensity: VisualDensity.compact,
                    padding: EdgeInsets.zero,
                    minimumSize: const Size(0, 28)),
                onPressed: widget.onRetry,
                icon: const Icon(Icons.replay, size: 14),
                label: const Text('Retry',
                    style: TextStyle(fontSize: 12))),
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
              style: const TextStyle(
                  fontWeight: FontWeight.bold)),
          Text('${req['summary']}'),
          const SizedBox(height: 8),
          Wrap(spacing: 6, children: [
            Chip(
              avatar: Icon(_riskIcon('${req['risk']}'),
                  size: 14,
                  color: _riskColor(cs, '${req['risk']}')),
              label: Text('Risk: ${req['risk']}',
                  style: TextStyle(
                      fontSize: 11,
                      color: _riskColor(cs, '${req['risk']}'))),
              visualDensity: VisualDensity.compact,
            ),
            if (perms.isNotEmpty)
              Chip(
                avatar: const Icon(Icons.key_outlined, size: 14),
                label:
                    Text(perms, style: const TextStyle(fontSize: 11)),
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

class _ConvDrawer extends StatefulWidget {
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
  State<_ConvDrawer> createState() => _ConvDrawerState();
}

class _ConvDrawerState extends State<_ConvDrawer> {
  String _q = '';

  @override
  Widget build(BuildContext context) {
    final convs = _q.isEmpty
        ? widget.convs
        : widget.convs
            .where((c) =>
                '${c['title'] ?? ''}'.toLowerCase().contains(_q))
            .toList();
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
                  onPressed: () => widget.onNew()),
              IconButton(
                  tooltip: 'New isolated chat (private memory)',
                  icon: const Icon(Icons.enhanced_encryption_outlined),
                  onPressed: () => widget.onNew(isolated: true)),
            ]),
          ),
          Padding(
            padding: const EdgeInsets.symmetric(horizontal: 16),
            child: TextField(
              decoration: const InputDecoration(
                hintText: 'Filter chats',
                prefixIcon: Icon(Icons.search, size: 18),
                isDense: true,
                border: OutlineInputBorder(),
              ),
              onChanged: (v) =>
                  setState(() => _q = v.trim().toLowerCase()),
            ),
          ),
          const SizedBox(height: 8),
          if (convs.isEmpty && widget.convs.isNotEmpty)
            Padding(
              padding: const EdgeInsets.all(16),
              child: Text('No chats match',
                  style: TextStyle(
                      color:
                          Theme.of(context).colorScheme.onSurfaceVariant)),
            ),
          for (final c in convs)
            ListTile(
              selected: c['active'] == true,
              leading: Icon(c['memory'] == 'isolated'
                  ? Icons.lock_outline
                  : Icons.chat_bubble_outline),
              title: Text('${c['title'] ?? 'Untitled'}',
                  maxLines: 1, overflow: TextOverflow.ellipsis),
              subtitle: Text(
                  '${c['memory']} · ${_fmtTs(c['created_at'])}',
                  style: const TextStyle(fontSize: 11)),
              onTap: () => widget.onSelect(c['id'] as String),
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
          if (widget.interrupted.isNotEmpty) ...[
            const Divider(),
            const Padding(
                padding: EdgeInsets.all(16),
                child: Text('Interrupted runs')),
            for (final r in widget.interrupted)
              ListTile(
                leading: const Icon(Icons.replay),
                title: Text('${(r['id'] as String).substring(0, 8)} · ${r['state']}'),
                subtitle: Text('${r['started_at']}',
                    style: const TextStyle(fontSize: 11)),
                trailing: IconButton(
                    icon: const Icon(Icons.play_arrow),
                    onPressed: () => widget.onResume(r['id'] as String)),
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
                  content: TextField(
                      controller: ctrl,
                      autofocus: true,
                      onSubmitted: (_) {
                        widget.onRename(
                            c['id'] as String, ctrl.text.trim());
                        Navigator.pop(ctx);
                      }),
                  actions: [
                    TextButton(
                        onPressed: () => Navigator.pop(ctx),
                        child: const Text('Cancel')),
                    FilledButton(
                        onPressed: () {
                          widget.onRename(c['id'] as String, ctrl.text.trim());
                          Navigator.pop(ctx);
                        },
                        child: const Text('Save')),
                  ],
                ));
      case 'scope':
        widget.onScope(c['id'] as String,
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
                          widget.onDelete(c['id'] as String);
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

  String? _error;

  Future<void> _load() async {
    try {
      final items = await widget.bridge.memories();
      if (mounted) {
        setState(() { _items = items; _loading = false; _error = null; });
      }
    } catch (e) {
      if (mounted) {
        setState(() { _loading = false; _error = '$e'; });
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(title: const Text('Memories'), actions: [
        IconButton(icon: const Icon(Icons.refresh), onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _items.isEmpty
                  ? Center(
                      child: Text(
                          'Nothing remembered yet — tell the agent '
                          'something worth keeping.',
                          textAlign: TextAlign.center,
                          style: TextStyle(color: cs.onSurfaceVariant)))
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
                          if (m['created_at'] != null)
                            Chip(
                                label: Text(_fmtTs(m['created_at']),
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
  bool _ingesting = false;
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

  String? _error;

  Future<void> _load() async {
    try {
      final items = await widget.bridge.docs();
      if (mounted) {
        setState(() { _items = items; _loading = false; _error = null; });
      }
    } catch (e) {
      if (mounted) {
        setState(() { _loading = false; _error = '$e'; });
      }
    }
  }

  Future<void> _ingest() async {
    final path = _pathCtrl.text.trim();
    if (path.isEmpty || _ingesting) return;
    setState(() => _ingesting = true);
    final r = await widget.bridge.docsIngest(path);
    if (!mounted) return;
    setState(() => _ingesting = false);
    _pathCtrl.clear();
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? '${r['error']}'
            : 'Ingested — sections are searchable now')));
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
      appBar: AppBar(title: const Text('Documents'), actions: [
        IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : Column(children: [
              if (_error != null)
                Padding(
                    padding: const EdgeInsets.symmetric(
                        horizontal: 12, vertical: 4),
                    child: Row(children: [
                      Icon(Icons.error_outline, size: 16, color: cs.error),
                      const SizedBox(width: 6),
                      Expanded(
                          child: Text(_error!,
                              style: TextStyle(color: cs.error))),
                      TextButton(
                          onPressed: _load, child: const Text('Retry')),
                    ])),
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
                      onPressed: _ingesting ? null : _ingest,
                      icon: _ingesting
                          ? const SizedBox(
                              width: 14,
                              height: 14,
                              child: CircularProgressIndicator(
                                  strokeWidth: 2))
                          : const Icon(Icons.upload_file),
                      label:
                          Text(_ingesting ? 'Ingesting…' : 'Ingest')),
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
                          hintText: 'Keyword or phrase'),
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
                        title: Text('${h['title'] ?? 'Untitled'} §${h['section']}',
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
                        child: Text('No documents yet — ingest a file to search it.'))
                    : ListView.builder(
                        itemCount: _items.length,
                        itemBuilder: (_, i) {
                          final d = _items[i];
                          return ListTile(
                            leading: const Icon(Icons.article_outlined),
                            title:
                                Text('${d['title'] ?? 'Untitled'}'),
                            subtitle: Text(
                                '${d['mime']}  •  ${d['sections']} sections'
                                '${d['created_at'] != null ? '  •  ${_fmtTs(d['created_at'])}' : ''}'),
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
              title: Text(msg['subject'] ?? '(No subject)'),
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
    final ok = await showDialog<String>(
        context: context,
        builder: (ctx) => AlertDialog(
              title: const Text('New message'),
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
                    onPressed: () => Navigator.of(ctx).pop(),
                    child: const Text('Cancel')),
                TextButton(
                    onPressed: () => Navigator.of(ctx).pop('draft'),
                    child: const Text('Save draft')),
                FilledButton.icon(
                    onPressed: () => Navigator.of(ctx).pop('send'),
                    icon: const Icon(Icons.send, size: 16),
                    label: const Text('Send')),
              ],
            ));
    if (ok == null) return;
    final recipients = toCtrl.text
        .split(',')
        .map((a) => a.trim())
        .where((a) => a.isNotEmpty)
        .toList();
    final r = ok == 'send'
        ? await widget.bridge.emailSend(
            to: recipients,
            subject: subjCtrl.text.trim(),
            body: bodyCtrl.text,
            inReplyTo: inReplyTo)
        : await widget.bridge.emailDraft(
            to: recipients,
            subject: subjCtrl.text.trim(),
            body: bodyCtrl.text,
            inReplyTo: inReplyTo);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? '${r['error']}'
            : ok == 'send'
                ? 'Sent'
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
                padding:
                    const EdgeInsets.symmetric(horizontal: 12, vertical: 4),
                child: Row(children: [
                  Icon(Icons.error_outline,
                      size: 16,
                      color: Theme.of(context).colorScheme.error),
                  const SizedBox(width: 6),
                  Expanded(
                      child: Text(_error!,
                          style: TextStyle(
                              color:
                                  Theme.of(context).colorScheme.error))),
                  TextButton(
                      onPressed: _search, child: const Text('Retry')),
                ])),
          Expanded(
              child: _loading
                  ? _listSkeleton(context)
                  : _hits.isEmpty && _error == null
                      ? Center(
                          child: Text(
                              _searchCtrl.text.trim().isEmpty
                                  ? 'Inbox is empty'
                                  : 'No mail matches that search',
                              style: TextStyle(
                                  color: Theme.of(context)
                                      .colorScheme
                                      .onSurfaceVariant)))
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
                            title: Text(m['subject'] ?? '(No subject)',
                                maxLines: 1,
                                overflow: TextOverflow.ellipsis,
                                style: TextStyle(
                                    fontWeight:
                                        (m['flags'] as List?)?.contains(
                                                    '\\Seen') ==
                                                true
                                            ? FontWeight.normal
                                            : FontWeight.w600)),
                            subtitle: Text(
                                '${m['from'] ?? ''}'
                                '${m['date'] != null ? ' · ${_fmtTs(m['date'])}' : ''}'
                                ' — ${m['snippet'] ?? ''}',
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
          ? _listSkeleton(context)
          : ListView(children: [
              Padding(
                padding: const EdgeInsets.fromLTRB(16, 12, 16, 4),
                child: Text(
                    'What the agent may do without asking — '
                    'changes apply immediately.',
                    style: TextStyle(
                        fontSize: 12,
                        color: Theme.of(context)
                            .colorScheme
                            .onSurfaceVariant)),
              ),
              for (final r in _rows) _policyRow(r),
            ]),
    );
  }

  Widget _policyRow(dynamic r) {
    return ListTile(
      title: Text('${r['permission']}',
          style: const TextStyle(fontFamily: 'monospace', fontSize: 13)),
      trailing: DropdownButton<String>(
        value: '${r['policy']}',
        underline: const SizedBox.shrink(),
        items: _policies
            .map((p) =>
                DropdownMenuItem(value: p, child: Text(_labels[p] ?? p)))
            .toList(),
        onChanged: (v) async {
          if (v == null) return;
          await widget.bridge.setPolicy('${r['permission']}', v);
          await _load();
        },
      ),
    );
  }
}

class AppsScreen extends StatefulWidget {
  const AppsScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<AppsScreen> createState() => _AppsScreenState();
}

class _AppsScreenState extends State<AppsScreen> {
  List<dynamic> _apps = const [];
  String _device = '';
  bool _loading = true;
  String? _running;
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    try {
      final r = await widget.bridge.appsList();
      if (mounted) {
        setState(() {
          _apps = (r['apps'] as List?) ?? const [];
          _device = '${r['device'] ?? ''}';
          _loading = false;
          _error = null;
        });
      }
    } catch (e) {
      if (mounted) setState(() { _loading = false; _error = '$e'; });
    }
  }

  /// Where this app's live data runs: null → everywhere (legacy),
  /// this device → here, otherwise inactive-here.
  String _placement(Map<String, dynamic> app) {
    final active = app['active_device'];
    if (active == null) return 'everywhere';
    if (active == _device) return 'here';
    return 'on ${'$active'.substring(0, 8)}…';
  }

  bool _runnableHere(Map<String, dynamic> app) {
    final active = app['active_device'];
    return active == null || active == _device;
  }

  Future<void> _run(Map<String, dynamic> app,
      {List<String> args = const []}) async {
    final id = '${app['id']}';
    setState(() => _running = id);
    final r = await widget.bridge.appsRun(id, args: args);
    if (!mounted) return;
    setState(() => _running = null);
    final error = r['error'];
    await showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text('${app['name']}'),
        content: SingleChildScrollView(
          child: SelectableText(error != null
              ? 'error: $error'
              : [
                  if ('${r['stdout']}'.isNotEmpty) '${r['stdout']}',
                  if ('${r['stderr']}'.isNotEmpty)
                    'stderr:\n${r['stderr']}',
                  'exit: ${r['exit_code'] ?? 'clean'}'
                      ' · fuel: ${r['fuel']}',
                ].join('\n\n')),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx),
              child: const Text('Close')),
        ],
      ),
    );
  }

  Future<void> _runWithArgs(Map<String, dynamic> app) async {
    final ctrl = TextEditingController();
    final args = await showDialog<List<String>>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text('Run ${app['name']}'),
        content: TextField(
          controller: ctrl,
          autofocus: true,
          decoration: const InputDecoration(
              labelText: 'Arguments', hintText: 'Space-separated'),
          onSubmitted: (_) =>
              Navigator.pop(ctx, ctrl.text.trim().split(RegExp(r'\s+'))
                  .where((s) => s.isNotEmpty).toList()),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx),
              child: const Text('Cancel')),
          FilledButton(
              onPressed: () => Navigator.pop(
                  ctx,
                  ctrl.text.trim().split(RegExp(r'\s+'))
                      .where((s) => s.isNotEmpty).toList()),
              child: const Text('Run')),
        ],
      ),
    );
    if (args != null) _run(app, args: args);
  }

  Future<void> _migrate(Map<String, dynamic> app) async {
    final id = '${app['id']}';
    final peers = await widget.bridge.peersList();
    if (!mounted) return;
    final list = (peers['peers'] as List?) ?? const [];
    if (list.isEmpty) {
      ScaffoldMessenger.of(context).showSnackBar(const SnackBar(
          content: Text('No paired devices — run pai pair offer on one and pai pair accept on the other')));
      return;
    }
    final to = await showModalBottomSheet<String>(
      context: context,
      builder: (ctx) => SafeArea(
        child: Column(
          mainAxisSize: MainAxisSize.min,
          children: [
            const ListTile(
                title: Text('Migrate to…'),
                subtitle: Text('Moves the app and its live data')),
            for (final p in list)
              ListTile(
                leading: const Icon(Icons.devices),
                title: Text('${p['name']}'),
                subtitle: Text(
                    "${p['platform']} · ${(p['id'] as String).substring(0, 12)}…"),
                onTap: () => Navigator.pop(ctx, '${p['id']}'),
              ),
          ],
        ),
      ),
    );
    if (to == null) return;
    final r = await widget.bridge.appsMigrate(id, to);
    if (!mounted) return;
    final err = r['error'];
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(err != null
            ? 'Migration failed: $err'
            : 'Migration queued — the app moves on the next sync')));
    _load();
  }

  /// Mint a capability token for this app — the file the guest holds.
  Future<void> _share(Map<String, dynamic> app) async {
    final id = '${app['id']}';
    final peers = await widget.bridge.peersList();
    if (!mounted) return;
    final peerList = (peers['peers'] as List?) ?? const [];
    final actions = <String>{'exec'};
    var days = 30;
    String forDevice = '';
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, setD) => AlertDialog(
          title: Text('Share ${app['name']}'),
          content: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              for (final a in const ['exec', 'read', 'write'])
                CheckboxListTile(
                  dense: true,
                  title: Text(a),
                  value: actions.contains(a),
                  onChanged: (v) => setD(() =>
                      v == true ? actions.add(a) : actions.remove(a)),
                ),
              TextFormField(
                initialValue: '$days',
                decoration: const InputDecoration(labelText: 'Days valid'),
                keyboardType: TextInputType.number,
                onChanged: (v) => days = int.tryParse(v) ?? days,
              ),
              DropdownButtonFormField<String>(
                initialValue: forDevice,
                decoration: const InputDecoration(
                    labelText: 'Bind to device (optional)'),
                items: [
                  const DropdownMenuItem(
                      value: '', child: Text('Bearer token')),
                  for (final p in peerList)
                    DropdownMenuItem(
                        value: '${p['id']}',
                        child: Text('${p['name']}')),
                ],
                onChanged: (v) => forDevice = v ?? '',
              ),
            ],
          ),
          actions: [
            TextButton(
                onPressed: () => Navigator.pop(ctx, false),
                child: const Text('Cancel')),
            FilledButton(
                onPressed: () => Navigator.pop(ctx, true),
                child: const Text('Mint token')),
          ],
        ),
      ),
    );
    if (ok != true || actions.isEmpty) return;
    final r = await widget.bridge
        .appsShareGrant(id, actions.join(','), days, forDevice);
    if (!mounted) return;
    final err = r['error'];
    if (err != null) {
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text('Share failed: $err')));
      return;
    }
    await showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Capability token'),
        content: SingleChildScrollView(
          child: SelectableText(
              "${r['token_json']}\n\nsaved to ${r['path']} — copy this "
              'JSON to the guest device; it is the whole credential.'),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx),
              child: const Text('Done')),
        ],
      ),
    );
  }

  /// Re-grant a narrower sub-token from a parent token this device
  /// holds — the parent must carry `share` and be bound to us.
  Future<void> _delegate() async {
    var parentJson = '';
    Map<String, dynamic>? parent;
    final chosen = <String>{};
    var days = 0;
    var forKey = '';
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, setD) {
          final parentActions =
              (parent?['actions'] as List?)?.cast<String>() ?? const [];
          return AlertDialog(
            title: const Text('Re-grant a token'),
            content: SingleChildScrollView(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  TextField(
                    maxLines: 4,
                    decoration: const InputDecoration(
                        labelText: 'Parent token JSON',
                        hintText: 'Paste the token file contents'),
                    onChanged: (v) => setD(() {
                      parentJson = v;
                      try {
                        parent =
                            jsonDecode(v) as Map<String, dynamic>;
                        chosen.removeWhere(
                            (a) => !parentActions.contains(a));
                      } catch (_) {
                        parent = null;
                      }
                    }),
                  ),
                  if (parentJson.isNotEmpty && parent == null)
                    const Text('Not a token JSON',
                        style: TextStyle(color: Colors.redAccent)),
                  if (parent != null)
                    Text('app ${parent!['app_id']} — may delegate '
                        '${parentActions.join(",")}'),
                  for (final a in parentActions)
                    CheckboxListTile(
                      dense: true,
                      title: Text(a),
                      value: chosen.contains(a),
                      onChanged: (v) => setD(() =>
                          v == true ? chosen.add(a) : chosen.remove(a)),
                    ),
                  TextFormField(
                    decoration: const InputDecoration(
                        labelText: 'Days valid (0 = parent expiry)'),
                    keyboardType: TextInputType.number,
                    onChanged: (v) => days = int.tryParse(v) ?? 0,
                  ),
                  TextFormField(
                    decoration: const InputDecoration(
                        labelText: 'Bind to device (prefix or hex key, '
                            'blank = bearer)'),
                    onChanged: (v) => forKey = v.trim(),
                  ),
                ],
              ),
            ),
            actions: [
              TextButton(
                  onPressed: () => Navigator.pop(ctx, false),
                  child: const Text('Cancel')),
              FilledButton(
                  onPressed: parent != null && chosen.isNotEmpty
                      ? () => Navigator.pop(ctx, true)
                      : null,
                  child: const Text('Mint sub-token')),
            ],
          );
        },
      ),
    );
    if (ok != true || parent == null || chosen.isEmpty) return;
    final r = await widget.bridge
        .shareDelegate(parentJson, chosen.join(','), days, forKey);
    if (!mounted) return;
    final err = r['error'];
    await showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(err != null ? 'Delegate failed' : 'Sub-token'),
        content: SingleChildScrollView(
          child: SelectableText(err != null
              ? '$err'
              : "${r['token_json']}\n\nsaved to ${r['path']} — copy "
                  'this JSON to the next device; it embeds the chain.'),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx),
              child: const Text('Done')),
        ],
      ),
    );
  }

  /// Act as a guest: present a capability token to run/read/write an
  /// app on its host over a shared folder (or relay).
  Future<void> _useToken() async {
    var tokenJson = '';
    Map<String, dynamic>? token;
    var dir = '';
    var relay = '';
    var path = 'data/note.txt';
    var text = '';
    String op = 'app-run';
    const opAction = {'app-run': 'exec', 'app-read': 'read', 'app-write': 'write'};
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, setD) {
          final acts =
              (token?['actions'] as List?)?.cast<String>() ?? const [];
          final ops = opAction.entries
              .where((e) => acts.contains(e.value))
              .map((e) => e.key)
              .toList();
          return AlertDialog(
            title: const Text('Use a shared token'),
            content: SingleChildScrollView(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  TextField(
                    maxLines: 4,
                    decoration: const InputDecoration(
                        labelText: 'Capability token JSON'),
                    onChanged: (v) => setD(() {
                      tokenJson = v;
                      try {
                        token = jsonDecode(v) as Map<String, dynamic>;
                        if (!ops.contains(op) && ops.isNotEmpty) {
                          op = ops.first;
                        }
                      } catch (_) {
                        token = null;
                      }
                    }),
                  ),
                  if (tokenJson.isNotEmpty && token == null)
                    const Text('Not a token JSON',
                        style: TextStyle(color: Colors.redAccent)),
                  if (token != null)
                    Text('app ${token!['app_id']} — '
                        '${acts.join(",")}'
                        '${token!['bound'] == true || token!['grantee_key'] != null ? " (bound)" : ""}'),
                  TextFormField(
                    decoration: const InputDecoration(
                        labelText: 'Shared folder (transport dir)'),
                    onChanged: (v) => setD(() => dir = v.trim()),
                  ),
                  TextFormField(
                    decoration: const InputDecoration(
                        labelText: 'or relay URL'),
                    onChanged: (v) => setD(() => relay = v.trim()),
                  ),
                  if (token != null)
                    DropdownButtonFormField<String>(
                      initialValue: ops.contains(op) ? op : null,
                      decoration:
                          const InputDecoration(labelText: 'Operation'),
                      items: [
                        for (final o in ops)
                          DropdownMenuItem(value: o, child: Text(o)),
                      ],
                      onChanged: (v) => op = v ?? op,
                    ),
                  if (op != 'app-run')
                    TextFormField(
                      initialValue: path,
                      decoration: const InputDecoration(
                          labelText: 'App path (data/… or files/…)'),
                      onChanged: (v) => path = v.trim(),
                    ),
                  if (op == 'app-write')
                    TextFormField(
                      decoration:
                          const InputDecoration(labelText: 'Text to write'),
                      onChanged: (v) => text = v,
                    ),
                ],
              ),
            ),
            actions: [
              TextButton(
                  onPressed: () => Navigator.pop(ctx, false),
                  child: const Text('Cancel')),
              FilledButton(
                  onPressed: token != null && (dir.isNotEmpty || relay.isNotEmpty)
                      ? () => Navigator.pop(ctx, true)
                      : null,
                  child: const Text('Call')),
            ],
          );
        },
      ),
    );
    if (ok != true || token == null) return;
    final args = op == 'app-write'
        ? [path, base64Encode(utf8.encode(text))]
        : op == 'app-read'
            ? [path]
            : <String>[];
    final r = await widget.bridge.guestCall({
      'op': op,
      'app_id': '${token!['app_id']}',
      'args': args,
      'token': tokenJson,
      'dir': dir,
      'relay': relay,
      'timeout_secs': 60,
    });
    if (!mounted) return;
    String shown;
    final err = r['error'];
    if (err != null) {
      shown = '$err';
    } else {
      final payload =
          base64Decode('${r['payload_b64']}');
      final v = jsonDecode(utf8.decode(payload));
      if (op == 'app-run') {
        final out = utf8.decode(base64Decode('${v['stdout_b64']}'));
        final stderr = '${v['stderr_b64']}';
        shown = '$out'
            '${stderr.isNotEmpty ? "\nstderr: ${utf8.decode(base64Decode(stderr))}" : ""}'
            '\n(exit ${v['exit_code'] ?? 0})';
      } else if (op == 'app-read') {
        shown = utf8.decode(base64Decode('${v['data_b64']}'));
      } else {
        shown = '$v';
      }
    }
    await showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(err != null ? 'Guest call failed' : 'Result'),
        content: SingleChildScrollView(child: SelectableText(shown)),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx),
              child: const Text('Done')),
        ],
      ),
    );
  }

  /// Issued grants with revoke — the `pai apps grants` surface.
  Future<void> _grants() async {
    final r = await widget.bridge.shareList();
    if (!mounted) return;
    final list = (r['grants'] as List?) ?? const [];
    await showModalBottomSheet<void>(
      context: context,
      builder: (ctx) => SafeArea(
        child: ListView(
          shrinkWrap: true,
          children: [
            Padding(
              padding: const EdgeInsets.all(8),
              child: Row(children: [
                TextButton.icon(
                    icon: const Icon(Icons.account_tree_outlined),
                    label: const Text('Re-grant token…'),
                    onPressed: () {
                      Navigator.pop(ctx);
                      _delegate();
                    }),
              ]),
            ),
            if (list.isEmpty)
              const Padding(
                  padding: EdgeInsets.all(24),
                  child: Text('No grants issued — share an app first.')),
            for (final g in list)
              ListTile(
                dense: true,
                title: Text('${g['app_id']} — '
                    '${(g['actions'] as List).join(",")}'),
                subtitle: Text(
                    '${'${g['token_id']}'.substring(0, 8)}… · '
                    '${g['status']} · '
                    '${g['bound'] == true ? "bound" : "bearer"}'
                    '${g['parent'] != null ? " · ↳ ${'${g['parent']}'.substring(0, 8)}…" : ""}',
                    style: const TextStyle(
                        fontFamily: 'monospace', fontSize: 12)),
                      trailing: g['status'] == 'active'
                          ? IconButton(
                              icon: const Icon(Icons.block),
                              tooltip: 'Revoke',
                              onPressed: () async {
                                final rr = await widget.bridge
                                    .shareRevoke('${g['token_id']}');
                                if (ctx.mounted) {
                                  if (rr['error'] != null) {
                                    ScaffoldMessenger.of(ctx).showSnackBar(
                                        SnackBar(
                                            content: Text(
                                                'revoke failed: ${rr['error']}')));
                                  } else {
                                    Navigator.pop(ctx);
                                    _grants();
                                  }
                                }
                              },
                            )
                          : null,
                    ),
                ],
              ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Apps'), actions: [
        IconButton(
            icon: const Icon(Icons.login),
            tooltip: 'Use a shared token (guest)',
            onPressed: _useToken),
        IconButton(
            icon: const Icon(Icons.key_outlined),
            tooltip: 'Issued grants',
            onPressed: _grants),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _apps.isEmpty
                  ? const Center(
                      child: Text(
                          'No apps installed — `pai deploy <dir>` on any\n'
                          'paired device syncs them here.',
                          textAlign: TextAlign.center))
              : ListView.builder(
                  itemCount: _apps.length,
                  itemBuilder: (_, i) {
                    final a = _apps[i] as Map<String, dynamic>;
                    final id = '${a['id']}';
                    final runnable = _runnableHere(a);
                    return ListTile(
                      leading: const Icon(Icons.widgets_outlined),
                      title: Text('${a['name']}'),
                      subtitle: Text(
                          '${id.length > 8 ? id.substring(0, 8) : id} · v${a['version']} · ${a['runtime']} · '
                          '${_placement(a)}',
                          style: const TextStyle(
                              fontFamily: 'monospace', fontSize: 12)),
                      trailing: _running == id
                          ? const SizedBox(
                              width: 20,
                              height: 20,
                              child: CircularProgressIndicator(
                                  strokeWidth: 2))
                          : Row(mainAxisSize: MainAxisSize.min, children: [
                              IconButton(
                                  icon: const Icon(Icons.play_arrow),
                                  tooltip: runnable
                                      ? 'Run on this device'
                                      : 'Active on another device',
                                  onPressed:
                                      runnable ? () => _run(a) : null),
                              PopupMenuButton<String>(
                                onSelected: (v) => switch (v) {
                                  'args' => _runWithArgs(a),
                                  'migrate' => _migrate(a),
                                  'share' => _share(a),
                                  _ => null,
                                },
                                itemBuilder: (_) => [
                                  if (runnable)
                                    const PopupMenuItem(
                                        value: 'args',
                                        child: Text('Run with args…')),
                                  const PopupMenuItem(
                                      value: 'migrate',
                                      child: Text('Migrate to…')),
                                  const PopupMenuItem(
                                      value: 'share',
                                      child: Text('Share…')),
                                ],
                              ),
                            ]),
                    );
                  },
                ),
    );
  }
}

/// '2026-09-15T20:11:03.123Z' → '2026-09-15 20:11' for list subtitles.
String _fmtTs(dynamic ts) {
  if (ts == null) return '';
  final t = '$ts';
  return t.length > 16 ? t.substring(0, 16).replaceFirst('T', ' ') : t;
}


/// Media — the media_jobs log (local + broker-routed generation work)
/// plus a local audio-generation action. Remote jobs submitted through
/// the CLI appear here too; they share the same store.
class MediaScreen extends StatefulWidget {
  const MediaScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<MediaScreen> createState() => _MediaScreenState();
}

class _MediaScreenState extends State<MediaScreen> {
  List<dynamic> _jobs = const [];
  bool _loading = true;
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    setState(() {
      _loading = true;
      _error = null;
    });
    try {
      final jobs = await widget.bridge.mediaList();
      if (mounted) setState(() { _jobs = jobs; _loading = false; });
    } catch (e) {
      if (mounted) setState(() { _error = '$e'; _loading = false; });
    }
  }

  Future<void> _generate() async {
    final promptCtl = TextEditingController();
    final secsCtl = TextEditingController(text: '10');
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Generate audio'),
        content: Column(mainAxisSize: MainAxisSize.min, children: [
          TextField(
            controller: promptCtl,
            autofocus: true,
            maxLines: 3,
            minLines: 1,
            decoration: const InputDecoration(
                labelText: 'Prompt',
                hintText: 'e.g. calm lo-fi rain ambience',
                border: OutlineInputBorder()),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: secsCtl,
            keyboardType: TextInputType.number,
            decoration: const InputDecoration(
                labelText: 'Duration (seconds, 1-300)',
                border: OutlineInputBorder()),
          ),
        ]),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx, false),
              child: const Text('Cancel')),
          FilledButton(
              onPressed: () => Navigator.pop(ctx, true),
              child: const Text('Generate')),
        ],
      ),
    );
    final prompt = promptCtl.text.trim();
    final secs = int.tryParse(secsCtl.text.trim()) ?? 10;
    promptCtl.dispose();
    secsCtl.dispose();
    if (ok != true || prompt.isEmpty || !mounted) return;
    try {
      final r = await widget.bridge.mediaGen(prompt, seconds: secs);
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(SnackBar(
          content: Text(r['error'] != null
              ? 'Generation failed: ${r['error']}'
              : 'Done - ${r['bytes']} bytes (job ${('${r['job_id']}').substring(0, 8)})')));
      _load();
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: Text('Generation failed: $e')));
      }
    }
  }

  Future<void> _export(Map job) async {
    final destCtl = TextEditingController(
        text: 'media-${(job['id'] as String? ?? 'job').substring(0, 8)}.wav');
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Export result'),
        content: TextField(
          controller: destCtl,
          autofocus: true,
          decoration: const InputDecoration(
              labelText: 'Destination path',
              hintText: 'e.g. clip.wav',
              border: OutlineInputBorder()),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx, false),
              child: const Text('Cancel')),
          FilledButton(
              onPressed: () => Navigator.pop(ctx, true),
              child: const Text('Save')),
        ],
      ),
    );
    final dest = destCtl.text.trim();
    destCtl.dispose();
    if (ok != true || dest.isEmpty || !mounted) return;
    final r = await widget.bridge.mediaExport('${job['id']}', dest);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? 'Export failed: ${r['error']}'
            : 'Saved ${r['bytes']} bytes to ${r['path']}')));
  }

  Color _stateColor(ColorScheme cs, String state) => switch (state) {
        'done' => Colors.greenAccent,
        'running' => cs.primary,
        'failed' => cs.error,
        _ => cs.onSurfaceVariant,
      };

  IconData _kindIcon(String kind) => switch (kind) {
        'text_to_audio' => Icons.music_note_outlined,
        'text_to_image' => Icons.image_outlined,
        'text_to_video' => Icons.videocam_outlined,
        'image_edit' => Icons.edit_outlined,
        _ => Icons.auto_awesome_outlined,
      };

  String _kindLabel(String kind) => kind.replaceAll('_', ' ');

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(title: const Text('Media'), actions: [
        IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _load),
        Padding(
          padding: const EdgeInsets.only(right: 12),
          child: FilledButton.icon(
              icon: const Icon(Icons.add, size: 18),
              label: const Text('New audio'),
              onPressed: _generate),
        ),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _jobs.isEmpty
                  ? Center(
                      child: Column(mainAxisSize: MainAxisSize.min, children: [
                        Icon(Icons.auto_awesome_outlined,
                            size: 40, color: cs.onSurfaceVariant),
                        const SizedBox(height: 12),
                        const Text('No media jobs yet'),
                        const SizedBox(height: 4),
                        Text('Generate audio from a prompt - it lands here.',
                            style: TextStyle(
                                color: cs.onSurfaceVariant, fontSize: 13)),
                      ]),
                    )
                  : ListView.builder(
                      itemCount: _jobs.length,
                      itemBuilder: (_, i) {
                        final j = _jobs[i];
                        final state = '${j['state']}';
                        final kind = '${j['kind']}';
                        final err = j['error'] as String?;
                        final done =
                            state == 'done' && j['result_blob'] != null;
                        return Card(
                          margin: const EdgeInsets.symmetric(
                              horizontal: 12, vertical: 4),
                          child: ListTile(
                            leading: Icon(_kindIcon(kind),
                                color: _stateColor(cs, state)),
                            title: Text('${j['prompt']}',
                                maxLines: 1, overflow: TextOverflow.ellipsis),
                            subtitle: Column(
                              crossAxisAlignment: CrossAxisAlignment.start,
                              children: [
                                Wrap(spacing: 6, runSpacing: 4, children: [
                                  Chip(
                                      label: Text(state,
                                          style: TextStyle(
                                              fontSize: 10,
                                              color:
                                                  _stateColor(cs, state))),
                                      visualDensity:
                                          VisualDensity.compact),
                                  Chip(
                                      label: Text(_kindLabel(kind),
                                          style:
                                              const TextStyle(fontSize: 10)),
                                      visualDensity:
                                          VisualDensity.compact),
                                  if (j['worker'] != null)
                                    Chip(
                                        label: Text(
                                            'worker ${(j['worker'] as String).substring(0, 8)}',
                                            style: const TextStyle(
                                                fontSize: 10)),
                                        visualDensity:
                                            VisualDensity.compact),
                                  Chip(
                                      label: Text(_fmtTs(j['created_at']),
                                          style:
                                              const TextStyle(fontSize: 10)),
                                      visualDensity:
                                          VisualDensity.compact),
                                ]),
                                if (err != null && err.isNotEmpty)
                                  Padding(
                                    padding: const EdgeInsets.only(top: 4),
                                    child: Text(err,
                                        style: TextStyle(
                                            color: cs.error, fontSize: 12)),
                                  ),
                              ],
                            ),
                            isThreeLine: err != null && err.isNotEmpty,
                            trailing: done
                                ? IconButton(
                                    icon:
                                        const Icon(Icons.save_alt, size: 20),
                                    tooltip: 'Export result (WAV)',
                                    onPressed: () => _export(j))
                                : null,
                          ),
                        );
                      },
                    ),
    );
  }
}

/// Devices — this machine's detected inference endpoints + binaries, and
/// the paired peer devices workloads can be routed to.
class DevicesScreen extends StatefulWidget {
  const DevicesScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<DevicesScreen> createState() => _DevicesScreenState();
}

class _DevicesScreenState extends State<DevicesScreen> {
  Map<String, dynamic>? _detect;
  List<dynamic> _peers = const [];
  List<dynamic> _models = const [];
  Map<String, dynamic> _status = const {};
  bool _loading = true;
  bool _scanning = false;
  String? _serving;
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _scan() async {
    if (_scanning) return;
    setState(() => _scanning = true);
    try {
      final models = await widget.bridge.modelsScan();
      if (mounted) setState(() => _models = models);
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: Text('Scan failed: $e')));
      }
    } finally {
      if (mounted) setState(() => _scanning = false);
    }
  }

  Future<void> _serve(String slug) async {
    if (_serving != null) return;
    setState(() => _serving = slug);
    try {
      final r = await widget.bridge.modelsServe(slug);
      if (!mounted) return;
      ScaffoldMessenger.of(context).showSnackBar(SnackBar(
          content: Text(r['error'] != null
              ? '${r['error']}'
              : 'Serving $slug — chat switched to it')));
      await _load();
    } finally {
      if (mounted) setState(() => _serving = null);
    }
  }

  Future<void> _load() async {
    setState(() => _loading = true);
    try {
      final d = await widget.bridge.detect();
      final p = await widget.bridge.peersList();
      Map<String, dynamic> st = const {};
      List<dynamic> models = const [];
      try {
        st = await widget.bridge.status();
      } catch (_) {}
      try {
        models = await widget.bridge.modelsList();
      } catch (_) {}
      if (!mounted) return;
      setState(() {
        _detect = d;
        _peers = (p['peers'] as List? ?? const []);
        _models = models;
        _status = st;
        _loading = false;
        _error = d['error'] as String? ?? p['error'] as String?;
      });
    } catch (e) {
      if (mounted) {
        setState(() {
          _loading = false;
          _error = '$e';
        });
      }
    }
  }

  Future<void> _switchTo(Map<dynamic, dynamic> e, String model) async {
    final r = await widget.bridge
        .setProvider(serverUrl: e['base_url'] as String?, model: model);
    if (!mounted) return;
    final err = r['error'] as String?;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(err ?? 'Chat now served by $model')));
    await _load();
  }

  /// Catalog picker + destination — `dest` empty installs internally,
  /// a path like `D:\pai-models` writes a portable pack onto that
  /// drive (the file is copied, not re-downloaded, when the model is
  /// already on disk somewhere reachable).
  Future<void> _installDialog() async {
    List<dynamic> catalog;
    try {
      catalog = await widget.bridge.modelsCatalog();
    } catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context)
            .showSnackBar(SnackBar(content: Text('Catalog failed: $e')));
      }
      return;
    }
    if (!mounted) return;
    String? chosen = catalog.isNotEmpty ? '${catalog.first['slug']}' : null;
    final destCtl = TextEditingController();
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, setD) => AlertDialog(
          title: const Text('Install a model'),
          content: SizedBox(
            width: 420,
            child: Column(mainAxisSize: MainAxisSize.min, children: [
              RadioGroup<String>(
                groupValue: chosen,
                onChanged: (v) => setD(() => chosen = v),
                child: Column(mainAxisSize: MainAxisSize.min, children: [
                  for (final m in catalog)
                    RadioListTile<String>(
                      dense: true,
                      value: '${m['slug']}',
                      title: Text('${m['slug']}'),
                      subtitle: Text(
                          '${m['family']} - ${m['quant'] ?? '?'} - ${m['size_mb']} MB',
                          style: const TextStyle(fontSize: 12)),
                    ),
                ]),
              ),
              const SizedBox(height: 8),
              TextField(
                controller: destCtl,
                decoration: const InputDecoration(
                    labelText: 'Destination (optional)',
                    hintText: 'e.g. D:\\pai-models - leave empty for internal',
                    border: OutlineInputBorder()),
              ),
            ]),
          ),
          actions: [
            TextButton(
                onPressed: () => Navigator.pop(ctx, false),
                child: const Text('Cancel')),
            FilledButton(
                onPressed: () => Navigator.pop(ctx, true),
                child: const Text('Install')),
          ],
        ),
      ),
    );
    final dest = destCtl.text.trim();
    destCtl.dispose();
    if (ok != true || chosen == null || !mounted) return;
    final slug = chosen!;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(dest.isEmpty
            ? 'Installing $slug - this can take a while'
            : 'Installing $slug to $dest - this can take a while'),
        duration: const Duration(seconds: 6)));
    final r = await widget.bridge.modelsInstall(slug, dest: dest);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? 'Install failed: ${r['error']}'
            : 'Installed ${r['installed']} -> ${r['path']}')));
    _scan();
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final endpoints =
        (_detect?['endpoints'] as List? ?? const []);
    final binaries =
        (_detect?['binaries'] as Map?)?.cast<String, dynamic>() ?? const {};
    return Scaffold(
      appBar: AppBar(title: const Text('Devices'), actions: [
        IconButton(icon: const Icon(Icons.refresh), onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : ListView(padding: const EdgeInsets.all(12), children: [
              if (_error != null)
                Padding(
                    padding: const EdgeInsets.only(bottom: 8),
                    child: Row(children: [
                      Icon(Icons.error_outline,
                          size: 16,
                          color: Theme.of(context).colorScheme.error),
                      const SizedBox(width: 6),
                      Expanded(
                          child: Text(_error!,
                              style: TextStyle(
                                  color: Theme.of(context)
                                      .colorScheme
                                      .error))),
                      TextButton(
                          onPressed: _load, child: const Text('Retry')),
                    ])),
              Text('This device',
                  style: Theme.of(context).textTheme.titleMedium),
              const SizedBox(height: 4),
              if (endpoints.isEmpty)
                Card(
                    child: ListTile(
                  leading: Icon(Icons.cloud_off_outlined,
                      color: cs.onSurfaceVariant),
                  title: const Text('No local inference endpoints'),
                  subtitle: const Text(
                      'Start llama-server, Ollama, or LM Studio — chat uses the echo stub until then.'),
                ))
              else
                for (final e in endpoints)
                  Card(
                      child: Padding(
                    padding: const EdgeInsets.fromLTRB(16, 12, 16, 12),
                    child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Row(children: [
                            Icon(Icons.bolt, color: cs.primary, size: 20),
                            const SizedBox(width: 8),
                            Expanded(
                                child: Text(
                                    '${e['provider']} · ${e['base_url']}',
                                    style: Theme.of(context)
                                        .textTheme
                                        .titleSmall)),
                          ]),
                          const SizedBox(height: 8),
                          Wrap(spacing: 6, runSpacing: 6, children: [
                            for (final m
                                in (e['models'] as List? ?? const []))
                              _ModelChip(
                                name: '$m',
                                serving: m == _status['model'],
                                chatPick: m == e['chat_model'],
                                embedOnly: (e['embed_only'] as List? ??
                                        const [])
                                    .contains(m),
                                onTap: () => _switchTo(e, '$m'),
                              ),
                            if ((e['models'] as List? ?? const []).isEmpty)
                              Text('No models reported',
                                  style: TextStyle(
                                      color: cs.onSurfaceVariant,
                                      fontSize: 12)),
                          ]),
                        ]),
                  )),
              const SizedBox(height: 4),
              for (final name in ['llama-server', 'ollama', 'lms'])
                ListTile(
                    dense: true,
                    leading: Icon(
                        binaries[name] != null
                            ? Icons.check_circle_outline
                            : Icons.highlight_off,
                        size: 18,
                        color: binaries[name] != null
                            ? cs.primary
                            : cs.onSurfaceVariant),
                    title: Text(name),
                    subtitle: Text(binaries[name] as String? ?? 'not found',
                        maxLines: 1, overflow: TextOverflow.ellipsis)),
              const SizedBox(height: 16),
              Row(children: [
                Text('Model packs',
                    style: Theme.of(context).textTheme.titleMedium),
                const Spacer(),
                IconButton(
                    icon: _scanning
                        ? const SizedBox(
                            width: 16,
                            height: 16,
                            child:
                                CircularProgressIndicator(strokeWidth: 2))
                        : const Icon(Icons.sync),
                    tooltip: 'Rescan drives for pai-models/ packs',
                    onPressed: _scanning ? null : _scan),
                IconButton(
                    icon: const Icon(Icons.download_outlined),
                    tooltip: 'Install a model (internal or to a drive)',
                    onPressed: _installDialog),
              ]),
              const SizedBox(height: 4),
              if (_models.isEmpty)
                Card(
                    child: ListTile(
                  leading: Icon(Icons.sd_storage_outlined,
                      color: cs.onSurfaceVariant),
                  title: const Text('No models installed'),
                  subtitle: const Text(
                      '`pai models install <slug> --to D:` builds a portable '
                      'pack — plug the drive in and rescan.'),
                ))
              else
                for (final m in _models)
                  Card(
                      child: ListTile(
                    leading: Icon(
                        m['online'] == true
                            ? Icons.sd_storage
                            : Icons.sd_storage_outlined,
                        color: m['online'] == true
                            ? cs.primary
                            : cs.onSurfaceVariant),
                    title: Text('${m['slug']}'),
                    subtitle: Text([
                      '${m['family'] ?? ''}',
                      if (m['quant'] != null) '${m['quant']}',
                      '${m['size_mb'] ?? '?'} MB',
                      if (m['path'] != null) '${m['path']}',
                    ].join(' · '),
                        maxLines: 2, overflow: TextOverflow.ellipsis),
                    trailing: m['serving'] == true
                        ? Chip(
                            label: Text('serving',
                                style: TextStyle(
                                    fontSize: 10, color: cs.primary)),
                            visualDensity: VisualDensity.compact)
                        : _serving == m['slug']
                            ? const SizedBox(
                                width: 20,
                                height: 20,
                                child: CircularProgressIndicator(
                                    strokeWidth: 2))
                            : m['online'] == true
                                ? TextButton(
                                    onPressed: () =>
                                        _serve('${m['slug']}'),
                                    child: const Text('Serve'))
                                : Tooltip(
                                    message:
                                        'Drive not mounted — plug it in and rescan',
                                    child: Chip(
                                        label: Text('offline',
                                            style: TextStyle(
                                                fontSize: 10,
                                                color: cs
                                                    .onSurfaceVariant)),
                                        visualDensity:
                                            VisualDensity.compact)),
                  )),
              const SizedBox(height: 16),
              Text('Paired devices',
                  style: Theme.of(context).textTheme.titleMedium),
              const SizedBox(height: 4),
              if (_peers.isEmpty)
                Card(
                    child: ListTile(
                  leading: Icon(Icons.phonelink_off_outlined,
                      color: cs.onSurfaceVariant),
                  title: const Text('No paired devices'),
                  subtitle: const Text(
                      'Pair another machine with `pai pair` — media jobs and app runs can then route to it.'),
                ))
              else
                for (final p in _peers)
                  Card(
                      child: ListTile(
                    leading: const Icon(Icons.devices),
                    title: Text('${p['name']}'),
                    subtitle: Text(
                        '${p['platform']} · ${(p['id'] as String).substring(0, 8)}'),
                  )),
            ]),
    );
  }
}



/// Alerts — agent-raised notifications (reminders, watch hits, proactive
/// notes). Tapping marks read; read-state syncs to peers.
class NotificationsScreen extends StatefulWidget {
  const NotificationsScreen(
      {super.key, required this.bridge, this.onChanged});
  final PaiBridge bridge;
  final VoidCallback? onChanged;
  @override
  State<NotificationsScreen> createState() => _NotificationsScreenState();
}

class _NotificationsScreenState extends State<NotificationsScreen> {
  List<dynamic> _items = const [];
  bool _unreadOnly = false;
  bool _loading = true;
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    setState(() => _loading = true);
    try {
      final r = await widget.bridge.notifyList(unreadOnly: _unreadOnly);
      if (!mounted) return;
      setState(() {
        _items = (r['notifications'] as List? ?? const []);
        _loading = false;
        _error = r['error'] as String?;
      });
    } catch (e) {
      if (mounted) {
        setState(() {
          _loading = false;
          _error = '$e';
        });
      }
    }
  }

  Future<void> _markRead(Map<String, dynamic> n) async {
    if (n['read_at'] != null) return;
    await widget.bridge.notifyMarkRead(n['id'] as String);
    widget.onChanged?.call();
    _load();
  }

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(title: const Text('Alerts'), actions: [
        TextButton.icon(
            onPressed: () => setState(() {
                  _unreadOnly = !_unreadOnly;
                  _load();
                }),
            icon: Icon(
                _unreadOnly
                    ? Icons.filter_alt
                    : Icons.filter_alt_off_outlined,
                size: 18),
            label: Text(_unreadOnly ? 'Unread' : 'All')),
        IconButton(icon: const Icon(Icons.refresh), onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _items.isEmpty
                  ? Center(
                      child: Text(
                          _unreadOnly
                              ? 'No unread notifications'
                              : 'Nothing yet — the agent posts reminders and proactive notes here.',
                          style: TextStyle(color: cs.onSurfaceVariant)))
                  : ListView.builder(
                      itemCount: _items.length,
                      itemBuilder: (ctx, i) {
                        final n = _items[i] as Map<String, dynamic>;
                        final unread = n['read_at'] == null;
                        return ListTile(
                          leading: Icon(
                              unread
                                  ? Icons.circle
                                  : Icons.circle_outlined,
                              size: 12,
                              color: unread
                                  ? cs.primary
                                  : cs.onSurfaceVariant),
                          title: Text('${n['title'] ?? ''}',
                              style: TextStyle(
                                  fontWeight: unread
                                      ? FontWeight.w600
                                      : FontWeight.normal)),
                          subtitle: Text(
                              '${n['body'] ?? ''}\n${n['source'] ?? ''} · ${_fmtTs(n['created_at'])}',
                              maxLines: 3,
                              overflow: TextOverflow.ellipsis),
                          isThreeLine: true,
                          onTap: () => _markRead(n),
                        );
                      }),
    );
  }
}

/// Activity — the audit log: every permission-gated op, deploy, share,
/// migration, and run, with its outcome.
class ActivityScreen extends StatefulWidget {
  const ActivityScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<ActivityScreen> createState() => _ActivityScreenState();
}

class _ActivityScreenState extends State<ActivityScreen> {
  List<dynamic> _events = const [];
  bool _loading = true;
  String? _error;

  @override
  void initState() {
    super.initState();
    _load();
  }

  Future<void> _load() async {
    setState(() => _loading = true);
    try {
      final r = await widget.bridge.audit();
      if (!mounted) return;
      setState(() {
        _events = r;
        _loading = false;
      });
    } catch (e) {
      if (mounted) {
        setState(() {
          _loading = false;
          _error = '$e';
        });
      }
    }
  }

  IconData _iconFor(String outcome) => switch (outcome) {
        'ok' => Icons.check_circle_outline,
        'denied' => Icons.block,
        'error' => Icons.error_outline,
        _ => Icons.info_outline,
      };

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    return Scaffold(
      appBar: AppBar(title: const Text('Activity'), actions: [
        IconButton(icon: const Icon(Icons.refresh), onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _events.isEmpty
                  ? Center(
                      child: Text('Nothing yet — every permission-gated action is logged here.',
                          style: TextStyle(color: cs.onSurfaceVariant)))
                  : ListView.builder(
                      itemCount: _events.length,
                      itemBuilder: (ctx, i) {
                        final e = _events[i] as Map<String, dynamic>;
                        final outcome = '${e['outcome'] ?? ''}';
                        final tool = e['tool'];
                        final detail = e['detail'];
                        return ListTile(
                          dense: true,
                          leading: Icon(_iconFor(outcome),
                              size: 18,
                              color: outcome == 'ok'
                                  ? cs.primary
                                  : cs.error),
                          title: Text(
                              '${e['kind'] ?? 'event'}${tool != null ? ' · $tool' : ''}',
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis),
                          subtitle: Text(
                              [
                                if (detail != null &&
                                    '$detail' != '{}' &&
                                    '$detail' != 'null')
                                  '$detail',
                                _fmtTs(e['at']),
                              ].join(' · '),
                              maxLines: 2,
                              overflow: TextOverflow.ellipsis),
                        );
                      }),
    );
  }
}

/// A reported model name on an inference endpoint, annotated with how
/// the runtime treats it: [serving] = the model chat is actually using,
/// [chatPick] = what auto-detect would choose, [embedOnly] = embedding-
/// only (cannot serve chat completions).
class _ModelChip extends StatelessWidget {
  const _ModelChip(
      {required this.name,
      required this.serving,
      required this.chatPick,
      required this.embedOnly,
      required this.onTap});
  final String name;
  final bool serving;
  final bool chatPick;
  final bool embedOnly;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final tag = serving
        ? 'serving'
        : chatPick
            ? 'chat'
            : embedOnly
                ? 'embeddings'
                : null;
    final chip = Container(
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
      decoration: BoxDecoration(
        color: serving
            ? cs.primaryContainer
            : embedOnly
                ? cs.surfaceContainerHighest
                : cs.secondaryContainer.withValues(alpha: 0.5),
        borderRadius: BorderRadius.circular(6),
      ),
      child: Row(mainAxisSize: MainAxisSize.min, children: [
        Text(name,
            style: TextStyle(
                fontSize: 12,
                color: embedOnly ? cs.onSurfaceVariant : null)),
        if (tag != null) ...[
          const SizedBox(width: 5),
          Text(tag,
              style: TextStyle(
                  fontSize: 10,
                  fontWeight: FontWeight.w600,
                  color: serving ? cs.primary : cs.onSurfaceVariant)),
        ],
      ]),
    );
    if (embedOnly) {
      return Tooltip(
          message: 'Embeddings only — cannot answer chat', child: chip);
    }
    return Tooltip(
      message: serving ? 'Serving chat' : 'Tap to serve chat from $name',
      child: InkWell(
          borderRadius: BorderRadius.circular(6), onTap: onTap, child: chip),
    );
  }
}
