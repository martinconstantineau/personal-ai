import 'dart:async';
import 'dart:math' as math;
import 'dart:convert';
import 'platform_config.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'nav.dart';
import 'pai_bridge.dart';
import 'theme.dart';

void main() => runApp(const PaiApp());

class PaiApp extends StatefulWidget {
  const PaiApp({super.key});
  @override
  State<PaiApp> createState() => _PaiAppState();
}

class _PaiAppState extends State<PaiApp> {
  String _dataDir = '';
  Map<String, dynamic> _prefs = const {};
  ThemeMode _mode = ThemeMode.system;
  AppAccent _accent = AppAccent.teal;

  /// 'compact' | 'standard' | 'large' — drives MediaQuery.textScaler.
  String _textScale = 'standard';
  double get _scaleFactor => switch (_textScale) {
        'compact' => 0.92,
        'large' => 1.12,
        _ => 1.0,
      };

  @override
  void initState() {
    super.initState();
    _dataDir = appDataDir();
    _prefs = loadPrefs(_dataDir);
    _applyPrefs();
  }

  /// Re-derive typed state from the prefs map.
  void _applyPrefs() {
    _mode = switch ('${_prefs['theme_mode'] ?? 'system'}') {
      'light' => ThemeMode.light,
      'dark' => ThemeMode.dark,
      _ => ThemeMode.system,
    };
    _accent = switch ('${_prefs['accent']}') {
      'brass' => AppAccent.brass,
      'cobalt' => AppAccent.cobalt,
      _ => AppAccent.teal,
    };
    final ts = '${_prefs['text_scale'] ?? 'standard'}';
    _textScale = ts == 'compact' || ts == 'large' ? ts : 'standard';
  }

  /// Persist + apply one preference — the single funnel every settings
  /// control writes through.
  void _setPref(String key, Object? value) {
    setState(() {
      _prefs = Map<String, dynamic>.of(_prefs)..[key] = value;
      _applyPrefs();
    });
    savePrefs(_dataDir, _prefs);
  }

  /// Theme mode — shared by the rail toggle and Settings.
  void _setTheme(ThemeMode m) => _setPref('theme_mode', m.name);

  /// Rail toggle — system → light → dark.
  void _cycleTheme() => _setTheme(switch (_mode) {
        ThemeMode.system => ThemeMode.light,
        ThemeMode.light => ThemeMode.dark,
        ThemeMode.dark => ThemeMode.system,
      });

  @override
  Widget build(BuildContext context) => MaterialApp(
        title: 'Personal AI',
        debugShowCheckedModeBanner: false,
        theme: AppTheme.light(accent: _accent),
        darkTheme: AppTheme.dark(accent: _accent),
        themeMode: _mode,
        // Text scale is a pref, not the OS's — applied app-wide here.
        builder: (context, child) => MediaQuery(
            data: MediaQuery.of(context)
                .copyWith(textScaler: TextScaler.linear(_scaleFactor)),
            child: child ?? const SizedBox.shrink()),
        home: HomeShell(
            themeMode: _mode,
            onThemeCycle: _cycleTheme,
            onThemeMode: _setTheme,
            prefs: _prefs,
            onPref: _setPref),
      );
}

/// App shell — one shared `PaiBridge`, a labeled rail for every surface,
/// and lazy tab construction (each screen builds on first visit, keeps
/// state after).
class HomeShell extends StatefulWidget {
  const HomeShell(
      {super.key,
      required this.themeMode,
      required this.onThemeCycle,
      required this.onThemeMode,
      required this.prefs,
      required this.onPref});
  final ThemeMode themeMode;
  final VoidCallback onThemeCycle;
  final ValueChanged<ThemeMode> onThemeMode;

  /// Live prefs map + writer — Settings edits ride through these, and
  /// the shell reads badge toggles from the same source.
  final Map<String, dynamic> prefs;
  final void Function(String key, Object? value) onPref;
  @override
  State<HomeShell> createState() => _HomeShellState();
}

/// The surfaces, in rail order — shared by the wide rail, the
/// Nav destinations, slash commands, and rail shortcuts live in
/// `nav.dart` — flat const lists, so the icon font tree-shaker sees
/// every glyph and tests can assert the wiring.
class _HomeShellState extends State<HomeShell> {
  PaiBridge? _pai;
  String? _error;
  String _dataDir = '';
  int _index = 0;
  int _unread = 0;
  int _unreadMail = 0;
  /// Notification ids already seen — the first poll seeds this, later
  /// polls diff against it so genuinely new rows raise a toast.
  Set<String>? _seenNotifyIds;
  Map<String, dynamic> _status = const {};
  StreamSubscription? _statusSub;
  StreamSubscription? _uiSub;
  /// Heartbeat for badge + toast state — notifications can arrive from
  /// background sync with no `ui:` event, so poll on a slow cadence.
  Timer? _pollTimer;
  final _visited = <int>{0};
  final _chatKey = GlobalKey<_ChatScreenState>();

  @override
  void initState() {
    super.initState();
    _init();
  }

  Future<void> _init() async {
    final dataDir = appDataDir();
    // 'auto' probes llama-server / Ollama / LM Studio, falls back to echo.
    _dataDir = dataDir;
    final provider = appProvider();
    try {
      final bridge = await PaiBridge.start(
          {'data_dir': dataDir, 'provider': provider});
      if (!mounted) return;
      setState(() => _pai = bridge);
      _statusSub = bridge.statusStream.listen((st) {
        if (mounted) setState(() => _status = st);
      });
      _uiSub = bridge.uiEvents.listen((ev) {
        if (!mounted) return;
        if (ev['kind'] == 'ui:model_packs') {
          _refreshUnread();
          final n = (ev['scanned'] as num? ?? 0).toInt();
          _toast('Model drive detected',
              '$n model(s) known — serve one from Devices.',
              Icons.sd_storage_outlined);
        }
      });
      try {
        await bridge.status();
      } catch (_) {}
      _refreshUnread();
      _pollTimer = Timer.periodic(
          const Duration(seconds: 30), (_) => _refreshUnread());
      _maybeWelcome();
    } catch (e) {
      setState(() => _error = 'Core init failed: $e\n$platformInitHint');
    }
  }

  Future<void> _refreshUnread() async {
    if (_pai == null) return;
    try {
      final r = await _pai!.notifyList(unreadOnly: true);
      if (!mounted) return;
      setState(() => _unread = (r['unread'] as num? ?? 0).toInt());
      // Toast notifications that appeared since the last poll — the
      // first poll just seeds the seen-set so a backlog doesn't burst.
      final items = (r['notifications'] as List? ?? const [])
          .whereType<Map<String, dynamic>>()
          .toList();
      final ids = items.map((n) => '${n['id']}').toSet();
      final seen = _seenNotifyIds;
      _seenNotifyIds = ids;
      if (seen != null) {
        for (final n in items) {
          if (seen.contains('${n['id']}')) continue;
          _toast('${n['title'] ?? 'Notification'}', '${n['body'] ?? ''}',
              Icons.notifications_outlined);
          break; // one toast per poll is enough — the badge counts the rest
        }
      }
    } catch (_) {}
    try {
      final m = await _pai!.emailSearch(unreadOnly: true, limit: 50);
      if (mounted) {
        setState(() =>
            _unreadMail = (m['results'] as List? ?? const []).length);
      }
    } catch (_) {}
  }

  /// In-app toast — the snackbar picks up `snackBarTheme`; icon + title
  /// line carry the semantics.
  void _toast(String title, String body, IconData icon) {
    final tt = Theme.of(context).textTheme;
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(
        duration: const Duration(seconds: 4),
        content: Row(children: [
          Icon(icon, size: 18),
          const SizedBox(width: AppSpacing.sm),
          Expanded(
            child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                mainAxisSize: MainAxisSize.min,
                children: [
                  Text(title,
                      style: tt.titleSmall, overflow: TextOverflow.ellipsis),
                  if (body.isNotEmpty)
                    Text(body,
                        style: tt.bodySmall,
                        maxLines: 2,
                        overflow: TextOverflow.ellipsis),
                ]),
          ),
        ]),
      ));
  }

  /// First run: show the welcome once — a marker file in the data dir
  /// is enough; no account, nothing leaves the device.
  void _maybeWelcome() {
    if (onboardingSeen(_dataDir)) return;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _helpDialog(firstRun: true);
    });
  }

  /// Feature tour + shortcuts — shown on first run, and reachable any
  /// time via the rail help button or F1.
  void _helpDialog({bool firstRun = false}) {
    showDialog(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(firstRun ? 'Welcome to Personal AI' : 'Personal AI'),
        content: SizedBox(
          width: 440,
          child: Column(mainAxisSize: MainAxisSize.min, children: [
            Text('local-first - private - auditable',
                style: Theme.of(ctx).textTheme.bodySmall),
            const SizedBox(height: AppSpacing.lg),
            const _FeatureRow(
                icon: Icons.chat_bubble_outline,
                title: 'Chat on local models',
                body: 'llama-server, Ollama, and LM Studio are detected '
                    'automatically - switch models from Devices.'),
            const _FeatureRow(
                icon: Icons.sd_storage_outlined,
                title: 'Portable model packs',
                body: 'Install models to a flash drive - plug it into '
                    'another machine, rescan, serve.'),
            const _FeatureRow(
                icon: Icons.music_note_outlined,
                title: 'Media generation',
                body: 'Generate audio on this device or a paired mesh '
                    'peer advertising media-run.'),
            const _FeatureRow(
                icon: Icons.policy_outlined,
                title: 'Audited and permissioned',
                body: 'Every action lands in Activity; app capabilities '
                    'live under Permissions.'),
            const Divider(height: AppSpacing.xxl),
            Align(
                alignment: Alignment.centerLeft,
                child: Text('Shortcuts',
                    style: Theme.of(ctx).textTheme.titleSmall)),
            const SizedBox(height: AppSpacing.sm),
            const _ShortcutRow('Ctrl+1-9,0', 'jump to a section'),
            const _ShortcutRow('Ctrl+P', 'command palette — navigate & search'),
            const _ShortcutRow('Ctrl+G ,', 'GitLab · Settings'),
            const _ShortcutRow('/', 'slash commands — /help lists them'),
            const _ShortcutRow('Ctrl+K', 'focus the message field'),
            const _ShortcutRow('Ctrl+N', 'new chat'),
            const _ShortcutRow('F1', 'this panel'),
          ]),
        ),
        actions: [
          FilledButton(
              onPressed: () {
                if (firstRun) {
                  Navigator.of(ctx).pop();
                  _dismissMarker();
                } else {
                  Navigator.of(ctx).pop();
                }
              },
              child: Text(firstRun ? 'Get started' : 'Close')),
        ],
      ),
    );
  }

  void _dismissMarker() {
    try {
      markOnboardingSeen(_dataDir);
    } catch (_) {}
  }

  void _select(int i) {
    setState(() {
      _index = i;
      _visited.add(i);
    });
    if (i == 3 || i == 6 || _unread > 0 || _unreadMail > 0) {
      _refreshUnread();
    }
  }

  /// Ctrl+P — destinations, actions, and a federated search over
  /// docs, chats, and memories.
  void _palette() {
    if (_pai == null) return;
    showDialog(
        context: context,
        builder: (ctx) => _CommandPalette(
            bridge: _pai!,
            onNavigate: (i) {
              Navigator.of(ctx).pop();
              _select(i);
            },
            onConversation: (id) {
              Navigator.of(ctx).pop();
              _select(0);
              _chatKey.currentState?.openConversation(id);
            },
            onAction: (a) {
              Navigator.of(ctx).pop();
              _paletteAction(a);
            }));
  }

  Future<void> _paletteAction(String action) async {
    switch (action) {
      case 'new':
        _select(0);
        _chatKey.currentState?.newConversation();
      case 'focus':
        _select(0);
        _chatKey.currentState?.focusInput();
      case 'sync':
        final r = await _pai!.syncNow();
        if (!mounted) return;
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(
            content: Text(r['error'] != null
                ? 'Sync failed: ${r['error']}'
                : 'Synced — pushed ${r['pushed']}, pulled '
                    '${r['pulled']}, skipped ${r['skipped']}.')));
      case 'theme':
        widget.onThemeCycle();
      case 'help':
        _helpDialog();
    }
  }

  @override
  void dispose() {
    _pollTimer?.cancel();
    _statusSub?.cancel();
    _uiSub?.cancel();
    super.dispose();
  }

  /// Rail-bottom provider health: green when a real model serves chat,
  /// amber on the echo fallback, grey until status lands.
  Color _healthColor(ColorScheme cs) {
    final brand = context.brand;
    if (_status.isEmpty) return brand.textMuted.withValues(alpha: 0.4);
    if (_status['provider'] == 'echo') return brand.warning;
    return brand.success;
  }

  /// Destination icon — Alerts and Email carry unread-count badges,
  /// each gated by its Notifications pref.
  Widget _navIcon(int i) {
    final icon = Icon(destIcons[i]);
    if (i == 3 && widget.prefs['badge_alerts'] != false) {
      return Badge.count(
          count: _unread, isLabelVisible: _unread > 0, child: icon);
    }
    if (i == 6 && widget.prefs['badge_email'] != false) {
      return Badge.count(
          count: _unreadMail,
          isLabelVisible: _unreadMail > 0,
          child: icon);
    }
    return icon;
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
        return ChatScreen(key: _chatKey, bridge: pai, onNavigate: _select);
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
      case 10:
        return GitLabScreen(bridge: pai);
      case 11:
        return SettingsScreen(
            bridge: pai,
            themeMode: widget.themeMode,
            onThemeMode: widget.onThemeMode,
            prefs: widget.prefs,
            onPref: widget.onPref,
            onNavigate: _select,
            onHelp: _helpDialog);
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
                      padding: const EdgeInsets.all(AppSpacing.xxl),
                      child: Text(_error!,
                          textAlign: TextAlign.center,
                          style: TextStyle(color: cs.error)))
                  : const Column(mainAxisSize: MainAxisSize.min, children: [
                      CircularProgressIndicator(),
                      SizedBox(height: AppSpacing.lg),
                      Text('Starting core…'),
                    ])));
    }
    return CallbackShortcuts(
      bindings: {
        for (var i = 0; i < railKeys.length; i++)
          SingleActivator(railKeys[i], control: true): () => _select(i),
        for (final MapEntry(:key, :value) in extraNavKeys.entries)
          key: () => _select(value),
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
        const SingleActivator(LogicalKeyboardKey.keyP, control: true):
            _palette,
        const SingleActivator(LogicalKeyboardKey.f1): _helpDialog,
      },
      child: LayoutBuilder(
        builder: (_, c) {
          final content = IndexedStack(
              index: _index,
              children: List.generate(destLabels.length, _tab));
          // Narrow windows (and the Android build): bottom bar with
          // labels on the selected destination only — all twelve fit.
          if (c.maxWidth < 640) {
            return Scaffold(
              body: content,
              bottomNavigationBar: NavigationBar(
                selectedIndex: _index,
                onDestinationSelected: _select,
                labelBehavior:
                    NavigationDestinationLabelBehavior.onlyShowSelected,
                destinations: [
                  for (var i = 0; i < destLabels.length; i++)
                    NavigationDestination(
                        icon: _navIcon(i), label: destLabels[i]),
                ],
              ),
            );
          }
          final wide = c.maxWidth > 1120;
          // The destinations outgrow short windows — let the rail scroll.
          // SizedBox keeps height bounded so the trailing health dot can
          // still dock at the bottom on tall windows.
          final rail = LayoutBuilder(
            builder: (_, rc) {
              final overflow = rc.maxHeight < destLabels.length * 72 + 144;
              final scrollable = SingleChildScrollView(
                child: SizedBox(
                  height: math.max(
                      rc.maxHeight, destLabels.length * 72 + 144),
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
                          padding: const EdgeInsets.only(bottom: AppSpacing.md + 2),
                          child: Column(mainAxisSize: MainAxisSize.min,
                              children: [
                            IconButton(
                                icon: Icon(
                                    switch (widget.themeMode) {
                                      ThemeMode.light =>
                                        Icons.light_mode_outlined,
                                      ThemeMode.dark =>
                                        Icons.dark_mode_outlined,
                                      ThemeMode.system =>
                                        Icons.brightness_auto_outlined,
                                    },
                                    size: 18),
                                tooltip:
                                    'Theme: ${widget.themeMode.name} — tap to switch',
                                onPressed: widget.onThemeCycle),
                            IconButton(
                                icon: const Icon(Icons.help_outline,
                                    size: 18),
                                tooltip: 'About & shortcuts (F1)',
                                onPressed: _helpDialog),
                            const SizedBox(height: AppSpacing.sm - 2),
                            Tooltip(
                            message: _healthMsg(),
                            child: Icon(Icons.circle,
                                size: 10,
                                color: _healthColor(
                                    Theme.of(context).colorScheme)),
                            ),
                          ]),
                        ),
                      ),
                    ),
                    destinations: [
                      for (var i = 0; i < destLabels.length; i++)
                        NavigationRailDestination(
                            icon: wide
                                ? _navIcon(i)
                                : Tooltip(
                                    message: destLabels[i],
                                    child: _navIcon(i)),
                            label: Text(destLabels[i])),
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
  const ChatScreen({super.key, required this.bridge, this.onNavigate});
  final PaiBridge? bridge;

  /// Rail navigation target — slash commands like /devices land here.
  final void Function(int)? onNavigate;
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

  /// Palette entry point — open a conversation by id.
  void openConversation(String id) => _selectConversation(id);
  final _scroll = ScrollController();
  PaiBridge? get _pai => widget.bridge;
  bool _ready = false;
  String? _error;
  bool _sending = false;
  String _lastUserText = '';
  bool _listening = false;
  bool _handsFree = false;
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
    _handsFree = false;
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
      if (mounted) {
        setState(() {
          _voice = voice;
          _speakReplies =
              loadPrefs(appDataDir())['speak_replies'] == true;
        });
      }
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
        .where((c) => c is Map && (c['type'] == 'text' || c['kind'] == 'text'))
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
  /// Slash-command dispatch — verbs map to bridge ops or rail jumps;
  /// the result lands as a system entry in the transcript.
  Future<void> _runCommand(String text) async {
    final sp = text.indexOf(' ');
    final verb =
        (sp < 0 ? text.substring(1) : text.substring(1, sp)).toLowerCase();
    final arg = sp < 0 ? '' : text.substring(sp + 1).trim();
    String result;
    switch (verb) {
      case 'new':
        await _newConversation();
        result = 'Started a new chat.';
      case 'rename':
        if (arg.isEmpty) {
          result = 'Usage: /rename <title>';
          break;
        }
        final id = _convs
            .firstWhere((c) => c['active'] == true,
                orElse: () => const <String, dynamic>{})['id'];
        if (id == null) {
          result = 'No active chat to rename.';
          break;
        }
        final r = await _pai!.conversationRename('$id', arg);
        await _refreshConvs();
        result = r['error'] != null
            ? 'Rename failed: ${r['error']}'
            : "Renamed to '$arg'.";
      case 'model':
        if (arg.isEmpty) {
          result = 'Usage: /model <slug>';
          break;
        }
        final r = await _pai!.modelsServe(arg);
        result = r['error'] != null
            ? '${r['error']}'
            : 'Serving $arg — chat switched to it.';
      case 'sync':
        final r = await _pai!.syncNow();
        result = r['error'] != null
            ? 'Sync failed: ${r['error']}'
            : 'Synced — pushed ${r['pushed']}, pulled ${r['pulled']}, '
                'skipped ${r['skipped']}.';
      case 'export':
        _copyTranscript();
        result = 'Transcript copied to the clipboard.';
      case 'help':
        result = cmds
            .map((c) =>
                '/${c.$1}${c.$2.isEmpty ? '' : ' ${c.$2}'} — ${c.$3}')
            .join('\n');
      default:
        final nav = navCmds[verb];
        if (nav != null) {
          widget.onNavigate?.call(nav);
          return;
        }
        result = "Unknown command '/$verb' — try /help.";
    }
    if (!mounted) return;
    setState(() => _entries.add(_Entry(role: 'system', text: result)));
    _scrollDown();
  }

  /// Commands matching the verb currently being typed — shown while
  /// the input is still a single `/…` token.
  List<(String, String, String)> get _cmdSuggestions {
    final t = _input.text;
    if (!t.startsWith('/') || t.contains(' ')) return const [];
    final verb = t.substring(1).toLowerCase();
    return cmds
        .where((c) => c.$1.startsWith(verb) && c.$1 != verb)
        .toList();
  }

  void _fillCommand(String name, bool noArgs) {
    _input.text = noArgs ? '/$name' : '/$name ';
    _input.selection =
        TextSelection.collapsed(offset: _input.text.length);
    _inputFocus.requestFocus();
  }

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
    if (text.startsWith('/')) {
      _input.clear();
      await _runCommand(text);
      return;
    }
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

  /// Continuous hands-free mode: listen → send → (speak reply) →
  /// listen again until toggled off. No barge-in — a capture in flight
  /// finishes on its own (silence or max duration), then the flag is
  /// checked. Enabling it also turns on speak-replies when TTS is up.
  Future<void> _toggleHandsFree() async {
    if (_handsFree) {
      setState(() => _handsFree = false);
      return;
    }
    if (_pai == null) return;
    setState(() {
      _handsFree = true;
      if (_voice['tts'] == true && _voice['speaker'] == true) {
        _speakReplies = true;
      }
    });
    while (_handsFree && mounted && _pai != null) {
      setState(() => _listening = true);
      final res = await _pai!.voiceListen();
      if (!_handsFree || !mounted) break;
      setState(() => _listening = false);
      if (res['error'] != null) {
        setState(() {
          _error = 'voice: ${res['error']}';
          _handsFree = false;
        });
        break;
      }
      final text = (res['text'] as String?)?.trim() ?? '';
      if (res['heard'] == true && text.isNotEmpty) {
        _input.text = text;
        await _send();
      }
    }
    if (mounted) setState(() => _listening = false);
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
    final tt = Theme.of(context).textTheme;
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
                  style: tt.bodySmall?.copyWith(color: cs.primary),
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
          if (_voice['mic'] == true && _voice['stt'] == true)
            IconButton(
              icon: Icon(_handsFree
                  ? Icons.hearing
                  : Icons.hearing_disabled_outlined),
              tooltip: _handsFree
                  ? 'Hands-free on — tap to stop listening'
                  : 'Hands-free voice loop (mic → agent → spoken reply)',
              onPressed: _toggleHandsFree,
            ),
          if (_voice['tts'] == true && _voice['speaker'] == true)
            IconButton(
              icon: Icon(_speakReplies
                  ? Icons.volume_up
                  : Icons.volume_off_outlined),
              tooltip: _speakReplies
                  ? 'Speaking replies — tap to mute'
                  : 'Speak replies aloud (piper)',
              onPressed: () {
                final next = !_speakReplies;
                setState(() => _speakReplies = next);
                final dir = appDataDir();
                final p = Map<String, dynamic>.of(loadPrefs(dir))
                  ..['speak_replies'] = next;
                savePrefs(dir, p);
              },
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
              ? _EmptyState(
                  icon: Icons.forum_outlined,
                  title: _pai == null
                      ? (_error == null ? 'Starting the core…' : '')
                      : 'No messages yet — ask anything.',
                  hint: _pai != null
                      ? 'local-first · private · auditable'
                      : null,
                  children: [
                    if (_pai != null)
                      Wrap(
                          spacing: AppSpacing.sm,
                          runSpacing: AppSpacing.sm,
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
                )
              : Semantics(
                  label: 'Conversation transcript',
                  child: ListView.builder(
                  controller: _scroll,
                  padding: const EdgeInsets.symmetric(
                      horizontal: AppSpacing.lg,
                      vertical: AppSpacing.md),
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
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(AppSpacing.lg, 0,
              AppSpacing.lg, AppSpacing.sm),
          child: Column(mainAxisSize: MainAxisSize.min, children: [
            if (_cmdSuggestions.isNotEmpty)
              Semantics(
                container: true,
                liveRegion: true,
                label: 'Command suggestions',
                child: Card(
                  margin: const EdgeInsets.only(bottom: AppSpacing.sm),
                clipBehavior: Clip.antiAlias,
                child: Column(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      for (final c in _cmdSuggestions)
                        ListTile(
                          dense: true,
                          leading: const Icon(Icons.bolt, size: 18),
                          title: Text(
                              '/${c.$1}${c.$2.isEmpty ? '' : ' ${c.$2}'}'),
                          subtitle: Text(c.$3),
                          onTap: () {
                            _fillCommand(c.$1, c.$2.isEmpty);
                            if (c.$2.isEmpty) _send();
                          },
                        ),
                    ]),
                ),
              ),
            Row(children: [
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
                  isDense: true,
                ),
              ),
            ),
            const SizedBox(width: AppSpacing.sm),
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
                      _handsFree ||
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
          ]),
        ),
      ]),
    );
  }
}

/// Shared skeleton rows shown while a list screen loads.
Widget _listSkeleton(BuildContext context) {
  final c = context.brand.surfaceCard;
  return Semantics(
    label: 'Loading',
    child: ListView(
        padding: const EdgeInsets.all(AppSpacing.md), children: [
      for (var i = 0; i < 5; i++)
        Container(
            margin: const EdgeInsets.only(bottom: AppSpacing.sm + 2),
            height: 56,
            decoration: BoxDecoration(
                color: c,
                borderRadius: AppRadii.rMd,
                border: Border.all(color: context.brand.hairline))),
    ]),
  );
}

/// Shared error block with a Retry action.
Widget _errorView(BuildContext context, String err, VoidCallback onRetry) {
  final cs = Theme.of(context).colorScheme;
  final tt = Theme.of(context).textTheme;
  return Center(
      child: Column(mainAxisSize: MainAxisSize.min, children: [
    Container(
        width: 56,
        height: 56,
        decoration: BoxDecoration(
            color: cs.errorContainer.withValues(alpha: 0.5),
            shape: BoxShape.circle),
        child: Icon(Icons.error_outline, color: cs.error, size: 26)),
    const SizedBox(height: AppSpacing.md),
    Padding(
        padding: const EdgeInsets.symmetric(horizontal: AppSpacing.x3),
        child: Text(err,
            textAlign: TextAlign.center,
            style: tt.bodyMedium?.copyWith(color: cs.error))),
    const SizedBox(height: AppSpacing.md),
    TextButton.icon(
        onPressed: onRetry,
        icon: const Icon(Icons.refresh, size: 16),
        label: const Text('Retry')),
  ]));
}

/// Shared empty-state: tinted icon disc, title, hint, optional extras
/// (e.g. suggestion chips).
class _EmptyState extends StatelessWidget {
  const _EmptyState(
      {required this.icon,
      required this.title,
      this.hint,
      this.children = const []});
  final IconData icon;
  final String title;
  final String? hint;
  final List<Widget> children;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final tt = Theme.of(context).textTheme;
    final brand = context.brand;
    return Center(
      child: Padding(
        padding: const EdgeInsets.all(AppSpacing.x3),
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          Container(
              width: 64,
              height: 64,
              decoration: BoxDecoration(
                  color: cs.primary.withValues(alpha: 0.10),
                  shape: BoxShape.circle,
                  border: Border.all(
                      color: cs.primary.withValues(alpha: 0.22))),
              child: Icon(icon, size: 28, color: cs.primary)),
          const SizedBox(height: AppSpacing.lg),
          Text(title,
              textAlign: TextAlign.center, style: tt.titleMedium),
          if (hint != null) ...[
            const SizedBox(height: AppSpacing.xs),
            Text(hint!,
                textAlign: TextAlign.center,
                style: tt.bodySmall?.copyWith(color: brand.textMuted)),
          ],
          if (children.isNotEmpty) ...[
            const SizedBox(height: AppSpacing.xl),
            ...children,
          ],
        ]),
      ),
    );
  }
}

/// Compact metadata chip — scope/source/state tags on list rows.
class _TagChip extends StatelessWidget {
  const _TagChip(this.label, {this.color, this.tooltip});
  final String label;
  final Color? color;
  final String? tooltip;

  @override
  Widget build(BuildContext context) {
    final tt = Theme.of(context).textTheme;
    final brand = context.brand;
    final fg = color ?? brand.textMuted;
    final chip = Container(
      padding: const EdgeInsets.symmetric(
          horizontal: AppSpacing.sm, vertical: AppSpacing.x2),
      decoration: BoxDecoration(
          color: fg.withValues(alpha: 0.10),
          borderRadius: AppRadii.rSm,
          border: Border.all(color: fg.withValues(alpha: 0.25))),
      child: Text(label,
          style: tt.labelSmall?.copyWith(color: fg, letterSpacing: 0.3)),
    );
    return tooltip == null ? chip : Tooltip(message: tooltip!, child: chip);
  }
}

/// Inline error strip for list headers — icon + message + retry.
class _InlineError extends StatelessWidget {
  const _InlineError(this.msg, {required this.onRetry, this.action});
  final String msg;
  final VoidCallback onRetry;
  final Widget? action;

  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final tt = Theme.of(context).textTheme;
    return Padding(
      padding: const EdgeInsets.symmetric(
          horizontal: AppSpacing.md, vertical: AppSpacing.xs),
      child: Row(children: [
        Icon(Icons.error_outline, size: 16, color: cs.error),
        const SizedBox(width: AppSpacing.sm - 2),
        Expanded(
            child: Text(msg,
                style: tt.bodySmall?.copyWith(color: cs.error))),
        ?action,
        TextButton(onPressed: onRetry, child: const Text('Retry')),
      ]),
    );
  }
}

/// Approval-sheet risk styling.
Color _riskColor(BrandColors b, ColorScheme cs, String risk) =>
    switch (risk) {
      'high' || 'critical' => cs.error,
      'medium' || 'moderate' => b.warning,
      _ => b.success,
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
    final tt = Theme.of(context).textTheme;
    final brand = context.brand;
    final e = widget.entry;
    final isYou = e.role == 'you';
    final isSys = e.role == 'system';
    final metaColor = isYou
        ? cs.onPrimaryContainer.withValues(alpha: 0.75)
        : brand.textMuted;
    return MouseRegion(
      onEnter: (_) => setState(() => _hov = true),
      onExit: (_) => setState(() => _hov = false),
      child: Align(
      alignment: isYou ? Alignment.centerRight : Alignment.centerLeft,
      child: Container(
        margin: const EdgeInsets.symmetric(vertical: AppSpacing.xs),
        padding: const EdgeInsets.symmetric(
            horizontal: AppSpacing.md + 2, vertical: AppSpacing.md - 2),
        constraints: const BoxConstraints(maxWidth: 560),
        decoration: BoxDecoration(
          color: isYou
              ? cs.primaryContainer
              : isSys
                  ? brand.surfaceOverlay.withValues(alpha: 0.5)
                  : brand.surfaceCard,
          borderRadius: BorderRadius.only(
            topLeft: const Radius.circular(AppRadii.lg),
            topRight: const Radius.circular(AppRadii.lg),
            bottomLeft: isYou
                ? const Radius.circular(AppRadii.lg)
                : const Radius.circular(AppRadii.sm),
            bottomRight: isYou
                ? const Radius.circular(AppRadii.sm)
                : const Radius.circular(AppRadii.lg),
          ),
          border: isYou
              ? null
              : Border.all(color: brand.hairline),
        ),
        child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
          Row(mainAxisSize: MainAxisSize.min, children: [
            Text(
              '${isYou ? 'You' : isSys ? 'Event' : 'Assistant'} · ${_fmtHm(e.at)}',
              style: tt.labelSmall?.copyWith(color: metaColor),
            ),
            if (_hov && !e.streaming && e.text.isNotEmpty)
              Padding(
                padding: const EdgeInsets.only(left: AppSpacing.sm),
                child: IconButton(
                  iconSize: 12,
                  visualDensity: VisualDensity.compact,
                  tooltip: 'Copy message',
                  onPressed: _copy,
                  icon: Icon(Icons.copy_outlined, color: metaColor),
                ),
              ),
          ]),
          if (e.text.isNotEmpty || !e.streaming)
            Padding(
                padding: const EdgeInsets.only(top: AppSpacing.x2),
                child: SelectableText(e.text, style: tt.bodyMedium)),
          if (e.isError && e.text.contains('provider'))
            Padding(
                padding: const EdgeInsets.only(top: AppSpacing.xs),
                child: Text(
                    'Check the provider endpoint — the Devices tab shows what\'s live',
                    style: tt.bodySmall?.copyWith(color: metaColor))),
          if (e.isError && widget.onRetry != null)
            TextButton.icon(
                style: TextButton.styleFrom(
                    visualDensity: VisualDensity.compact,
                    padding: EdgeInsets.zero,
                    minimumSize: const Size(0, 28)),
                onPressed: widget.onRetry,
                icon: const Icon(Icons.replay, size: 14),
                label: const Text('Retry')),
          if (e.streaming && e.text.isEmpty)
            const Padding(
                padding: EdgeInsets.only(top: AppSpacing.xs),
                child: SizedBox(
                    height: 16,
                    width: 16,
                    child: CircularProgressIndicator(strokeWidth: 2))),
          if (e.sub.isNotEmpty)
            Padding(
                padding: const EdgeInsets.only(top: AppSpacing.x2),
                child: Text(e.sub,
                    style: tt.labelSmall
                        ?.copyWith(color: metaColor.withValues(alpha: 0.8)))),
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
    final tt = Theme.of(context).textTheme;
    final brand = context.brand;
    final risk = _riskColor(brand, cs, '${req['risk']}');
    final perms = (req['permissions'] as List? ?? []).join(', ');
    return Padding(
      padding: const EdgeInsets.all(AppSpacing.xxl),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(children: [
            Icon(_riskIcon('${req['risk']}'), color: risk),
            const SizedBox(width: AppSpacing.sm),
            Text('Approval needed', style: tt.titleLarge),
          ]),
          const SizedBox(height: AppSpacing.lg),
          Text('${req['tool']}', style: tt.titleSmall),
          Text('${req['summary']}', style: tt.bodyMedium),
          const SizedBox(height: AppSpacing.sm),
          Wrap(spacing: AppSpacing.sm - 2, children: [
            _TagChip('Risk: ${req['risk']}', color: risk),
            if (perms.isNotEmpty)
              _TagChip(perms, tooltip: 'Permissions requested'),
          ]),
          const SizedBox(height: AppSpacing.xl),
          Row(mainAxisAlignment: MainAxisAlignment.end, children: [
            TextButton(
              onPressed: () => Navigator.of(context).pop(false),
              child: const Text('Deny'),
            ),
            const SizedBox(width: AppSpacing.sm),
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
            padding: const EdgeInsets.all(AppSpacing.lg),
            child: Row(children: [
              Text('Chats',
                  style: Theme.of(context).textTheme.titleLarge),
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
            padding: const EdgeInsets.symmetric(
                horizontal: AppSpacing.lg),
            child: TextField(
              decoration: const InputDecoration(
                hintText: 'Filter chats',
                prefixIcon: Icon(Icons.search, size: 18),
                isDense: true,
              ),
              onChanged: (v) =>
                  setState(() => _q = v.trim().toLowerCase()),
            ),
          ),
          const SizedBox(height: AppSpacing.sm),
          if (convs.isEmpty && widget.convs.isNotEmpty)
            Padding(
              padding: const EdgeInsets.all(AppSpacing.lg),
              child: Text('No chats match',
                  style: Theme.of(context).textTheme.bodySmall),
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
                  '${c['memory']} · ${_fmtTs(c['created_at'])}'),
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
            const Divider(height: AppSpacing.xl),
            Padding(
                padding: const EdgeInsets.symmetric(
                    horizontal: AppSpacing.lg,
                    vertical: AppSpacing.xs),
                child: Text('Interrupted runs',
                    style:
                        Theme.of(context).textTheme.titleSmall)),
            for (final r in widget.interrupted)
              ListTile(
                leading: const Icon(Icons.replay),
                title: Text('${(r['id'] as String).substring(0, 8)} · ${r['state']}'),
                subtitle: Text('${r['started_at']}'),
                trailing: IconButton(
                    icon: const Icon(Icons.play_arrow),
                    tooltip: 'Resume interrupted run',
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
        IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _items.isEmpty
                  ? const _EmptyState(
                      icon: Icons.psychology_outlined,
                      title: 'Nothing remembered yet',
                      hint: 'Tell the agent something worth keeping.')
              : ListView.builder(
                  padding: const EdgeInsets.symmetric(
                      horizontal: AppSpacing.md,
                      vertical: AppSpacing.sm),
                  itemCount: _items.length,
                  itemBuilder: (_, i) {
                    final m = _items[i];
                    final conv = m['conversation'];
                    return Card(
                      margin: const EdgeInsets.only(
                          bottom: AppSpacing.sm),
                      child: Padding(
                        padding:
                            const EdgeInsets.all(AppSpacing.md),
                        child: Column(
                            crossAxisAlignment:
                                CrossAxisAlignment.start,
                            children: [
                              Row(crossAxisAlignment:
                                  CrossAxisAlignment.start,
                                  children: [
                                Expanded(
                                    child: Text('${m['content']}',
                                        style: Theme.of(context)
                                            .textTheme
                                            .bodyLarge)),
                                IconButton(
                                  icon: Icon(Icons.delete_outline,
                                      size: 18, color: cs.error),
                                  tooltip: 'Forget',
                                  onPressed: () => _forget(m),
                                ),
                              ]),
                              const SizedBox(height: AppSpacing.sm),
                              Wrap(
                                  spacing: AppSpacing.sm - 2,
                                  runSpacing: AppSpacing.xs,
                                  children: [
                                    _TagChip('${m['scope']}'),
                                    _TagChip('${m['source']}'),
                                    _TagChip(conv == null
                                        ? 'global'
                                        : 'chat ${(conv as String).substring(0, 8)}'),
                                    _TagChip('${m['privacy']}'),
                                    if (m['created_at'] != null)
                                      _TagChip(
                                          _fmtTs(m['created_at'])),
                                  ]),
                            ]),
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
                _InlineError(_error!, onRetry: _load),
              Padding(
                padding: const EdgeInsets.fromLTRB(AppSpacing.lg,
                    AppSpacing.md, AppSpacing.lg, AppSpacing.xs),
                child: Row(children: [
                  Expanded(
                    child: TextField(
                      controller: _pathCtrl,
                      decoration: const InputDecoration(
                          labelText: 'Ingest file path',
                          hintText:
                              r'C:\path\to\doc — .txt .md .html .pdf .epub .docx'),
                      onSubmitted: (_) => _ingest(),
                    ),
                  ),
                  const SizedBox(width: AppSpacing.sm),
                  FilledButton.icon(
                      onPressed: _ingesting ? null : _ingest,
                      icon: _ingesting
                          ? const SizedBox(
                              width: 14,
                              height: 14,
                              child: CircularProgressIndicator(
                                  strokeWidth: 2))
                          : const Icon(Icons.upload_file, size: 18),
                      label:
                          Text(_ingesting ? 'Ingesting…' : 'Ingest')),
                ]),
              ),
              Padding(
                padding: const EdgeInsets.fromLTRB(AppSpacing.lg, 0,
                    AppSpacing.lg, AppSpacing.sm),
                child: Row(children: [
                  Expanded(
                    child: TextField(
                      controller: _searchCtrl,
                      decoration: const InputDecoration(
                          labelText: 'Search sections',
                          hintText: 'Keyword or phrase',
                          prefixIcon:
                              Icon(Icons.search, size: 18)),
                      onSubmitted: (_) => _search(),
                    ),
                  ),
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
                            style: AppText.mono(context,
                                color: cs.primary)),
                        title: Text(
                            '${h['title'] ?? 'Untitled'} §${h['section']}'
                            '${h['page'] != null ? ' p.${h['page']}' : ''}',
                            style: Theme.of(context)
                                .textTheme
                                .titleSmall),
                        subtitle: Text('${h['snippet']}',
                            maxLines: 2, overflow: TextOverflow.ellipsis),
                      );
                    },
                  ),
                ),
              const Divider(height: AppSpacing.lg),
              Expanded(
                child: _items.isEmpty
                    ? const _EmptyState(
                        icon: Icons.description_outlined,
                        title: 'No documents yet',
                        hint: 'Ingest a file to search it.')
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

  Future<void> _configure() async {
    final hostCtl = TextEditingController(text: 'imap.gmail.com');
    final portCtl = TextEditingController(text: '993');
    final userCtl = TextEditingController();
    final passCtl = TextEditingController();
    final smtpHostCtl = TextEditingController(text: 'smtp.gmail.com');
    final smtpPortCtl = TextEditingController(text: '465');
    String smtpTls = 'tls';
    final ok = await showDialog<bool>(
        context: context,
        builder: (ctx) => StatefulBuilder(
            builder: (ctx, setD) => AlertDialog(
                    title: const Text('Mail account'),
                    content: SizedBox(
                        width: 440,
                        child: SingleChildScrollView(
                            child: Column(
                                mainAxisSize: MainAxisSize.min,
                                children: [
                              TextField(
                                  controller: hostCtl,
                                  autofocus: true,
                                  decoration: const InputDecoration(
                                      labelText: 'IMAP host')),
                              TextField(
                                  controller: portCtl,
                                  keyboardType: TextInputType.number,
                                  decoration: const InputDecoration(
                                      labelText: 'IMAP port')),
                              TextField(
                                  controller: userCtl,
                                  decoration: const InputDecoration(
                                      labelText: 'Email address')),
                              TextField(
                                  controller: passCtl,
                                  obscureText: true,
                                  decoration: const InputDecoration(
                                      labelText:
                                          'Password (app password for Gmail/Outlook)',
                                      helperText:
                                          'Stored in the OS keystore — never in a file')),
                              const Divider(
                                  height: AppSpacing.xxl),
                              TextField(
                                  controller: smtpHostCtl,
                                  decoration: const InputDecoration(
                                      labelText:
                                          'SMTP host (empty = drafts only)')),
                              TextField(
                                  controller: smtpPortCtl,
                                  keyboardType: TextInputType.number,
                                  decoration: const InputDecoration(
                                      labelText: 'SMTP port')),
                              DropdownButtonFormField<String>(
                                  initialValue: smtpTls,
                                  decoration: const InputDecoration(
                                      labelText: 'SMTP security'),
                                  items: const [
                                    DropdownMenuItem(
                                        value: 'tls', child: Text('TLS')),
                                    DropdownMenuItem(
                                        value: 'starttls',
                                        child: Text('STARTTLS')),
                                    DropdownMenuItem(
                                        value: 'none',
                                        child: Text('None')),
                                  ],
                                  onChanged: (v) =>
                                      setD(() => smtpTls = v ?? 'tls')),
                            ]))),
                    actions: [
                      TextButton(
                          onPressed: () => Navigator.of(ctx).pop(false),
                          child: const Text('Cancel')),
                      FilledButton(
                          onPressed: () => Navigator.of(ctx).pop(true),
                          child: const Text('Save')),
                    ])));
    if (ok != true || !mounted) return;
    final r = await widget.bridge.emailConfigure(
        host: hostCtl.text.trim(),
        port: int.tryParse(portCtl.text.trim()) ?? 993,
        user: userCtl.text.trim(),
        password: passCtl.text.isEmpty ? null : passCtl.text,
        smtpHost: smtpHostCtl.text.trim(),
        smtpPort: int.tryParse(smtpPortCtl.text.trim()) ?? 465,
        smtpTls: smtpTls);
    hostCtl.dispose();
    portCtl.dispose();
    userCtl.dispose();
    passCtl.dispose();
    smtpHostCtl.dispose();
    smtpPortCtl.dispose();
    if (!mounted) return;
    if (r['error'] != null) {
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text('${r['error']}')));
      return;
    }
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['password_stored'] == true
            ? 'Account configured — password saved to the OS keystore'
            : 'Account configured — set PAI_EMAIL_PASSWORD or re-save with a password')));
    _search();
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
                  icon: const Icon(Icons.settings_outlined),
                  tooltip: 'Configure account',
                  onPressed: _configure),
              IconButton(
                  icon: const Icon(Icons.edit_outlined),
                  tooltip: 'New draft',
                  onPressed: () => _compose()),
              IconButton(
                  icon: const Icon(Icons.refresh),
                  tooltip: 'Refresh',
                  onPressed: _search),
            ]),
        body: Column(children: [
          Padding(
              padding: const EdgeInsets.fromLTRB(AppSpacing.lg,
                  AppSpacing.md, AppSpacing.lg, AppSpacing.xs),
              child: TextField(
                  controller: _searchCtrl,
                  decoration: const InputDecoration(
                      hintText: 'Search mail',
                      prefixIcon: Icon(Icons.search, size: 18)),
                  onSubmitted: (_) => _search())),
          if (_error != null)
            _InlineError(_error!,
                onRetry: _search,
                action: _error!.contains('not configured')
                    ? TextButton(
                        onPressed: _configure,
                        child: const Text('Set up'))
                    : null),
          Expanded(
              child: _loading
                  ? _listSkeleton(context)
                  : _hits.isEmpty && _error == null
                      ? _EmptyState(
                          icon: Icons.mail_outline,
                          title: _searchCtrl.text.trim().isEmpty
                              ? 'Inbox is empty'
                              : 'No mail matches that search')
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

/// GitLab view — issues, merge requests, and pipelines via the gitlab
/// connector. Merge/trigger stay approval-gated tool-side; here they run
/// behind an explicit confirm dialog.
class GitLabScreen extends StatefulWidget {
  const GitLabScreen({super.key, required this.bridge});
  final PaiBridge bridge;
  @override
  State<GitLabScreen> createState() => _GitLabScreenState();
}

class _GitLabScreenState extends State<GitLabScreen> {
  int _tab = 0; // 0 issues · 1 MRs · 2 pipelines
  List<dynamic> _rows = const [];
  bool _loading = true;
  String? _error;
  bool _configured = true;
  final _searchCtrl = TextEditingController();
  String _state = 'opened';

  @override
  void initState() {
    super.initState();
    _refresh();
  }

  @override
  void dispose() {
    _searchCtrl.dispose();
    super.dispose();
  }

  Future<void> _refresh() async {
    setState(() => _loading = true);
    final search = _searchCtrl.text.trim();
    final r = switch (_tab) {
      1 => await widget.bridge.gitlabMrs(
        state: _state,
        search: search.isEmpty ? null : search,
      ),
      2 => await widget.bridge.gitlabPipelines(),
      _ => await widget.bridge.gitlabIssues(
        state: _state,
        search: search.isEmpty ? null : search,
      ),
    };
    if (!mounted) return;
    final err = r['error'] as String?;
    setState(() {
      _error = err;
      _configured = !(err?.contains('not configured') ?? false);
      _rows = switch (_tab) {
        1 => r['merge_requests'] as List<dynamic>? ?? const [],
        2 => r['pipelines'] as List<dynamic>? ?? const [],
        _ => r['issues'] as List<dynamic>? ?? const [],
      };
      _loading = false;
    });
  }

  void _snack(String msg) =>
      ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(msg)));

  Future<void> _openIssue(dynamic m) async {
    final r = await widget.bridge.gitlabIssue(m['iid'] as int);
    if (!mounted) return;
    final i = r['issue'] as Map<String, dynamic>?;
    if (i == null) {
      _snack('${r['error'] ?? 'no issue'}');
      return;
    }
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text('#${i['iid']}  ${i['title'] ?? ''}'),
        content: SizedBox(
          width: 480,
          child: SingleChildScrollView(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(
                  '${i['state']} · ${i['author'] ?? ''} · ${i['project'] ?? ''}',
                  style: Theme.of(ctx).textTheme.bodySmall,
                ),
                const SizedBox(height: AppSpacing.md),
                Text(i['description'] ?? '(no description)'),
                const SizedBox(height: AppSpacing.md),
                Text(
                  '${i['web_url'] ?? ''}',
                  style: AppText.mono(ctx,
                      color: Theme.of(ctx).colorScheme.primary),
                ),
              ],
            ),
          ),
        ),
        actions: [
          TextButton(
            onPressed: () {
              Navigator.of(ctx).pop();
              _comment('issue', i['iid'] as int);
            },
            child: const Text('Comment'),
          ),
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(),
            child: const Text('Close'),
          ),
        ],
      ),
    );
  }

  Future<void> _openMr(dynamic m) async {
    final r = await widget.bridge.gitlabMr(m['iid'] as int);
    if (!mounted) return;
    final mr = r['merge_request'] as Map<String, dynamic>?;
    if (mr == null) {
      _snack('${r['error'] ?? 'no merge request'}');
      return;
    }
    showDialog<void>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text('!${mr['iid']}  ${mr['title'] ?? ''}'),
        content: SizedBox(
          width: 480,
          child: SingleChildScrollView(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                Text(
                  '${mr['state']} · ${mr['source_branch']} → ${mr['target_branch']} · ${mr['author'] ?? ''}',
                  style: Theme.of(ctx).textTheme.bodySmall,
                ),
                if (mr['head_pipeline_status'] != null)
                  Text('pipeline: ${mr['head_pipeline_status']}'),
                const SizedBox(height: AppSpacing.md),
                Text(mr['description'] ?? '(no description)'),
                const SizedBox(height: AppSpacing.md),
                Text(
                  '${mr['web_url'] ?? ''}',
                  style: AppText.mono(ctx,
                      color: Theme.of(ctx).colorScheme.primary),
                ),
              ],
            ),
          ),
        ),
        actions: [
          if (mr['state'] == 'opened')
            TextButton(
              onPressed: () {
                Navigator.of(ctx).pop();
                _mergeMr(mr['iid'] as int, '${mr['title']}');
              },
              child: const Text('Merge'),
            ),
          TextButton(
            onPressed: () {
              Navigator.of(ctx).pop();
              _comment('mr', mr['iid'] as int);
            },
            child: const Text('Comment'),
          ),
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(),
            child: const Text('Close'),
          ),
        ],
      ),
    );
  }

  Future<void> _comment(String kind, int iid) async {
    final bodyCtrl = TextEditingController();
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text('Comment on $kind ${kind == 'mr' ? '!' : '#'}$iid'),
        content: SizedBox(
          width: 480,
          child: TextField(
            controller: bodyCtrl,
            maxLines: 6,
            autofocus: true,
            decoration: const InputDecoration(labelText: 'Comment'),
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(ctx).pop(true),
            child: const Text('Post'),
          ),
        ],
      ),
    );
    if (ok != true || !mounted) return;
    final r = await widget.bridge.gitlabComment(
      kind: kind,
      iid: iid,
      body: bodyCtrl.text,
    );
    bodyCtrl.dispose();
    if (!mounted) return;
    _snack(r['error'] != null ? '${r['error']}' : 'Comment posted');
  }

  Future<void> _mergeMr(int iid, String title) async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Merge merge request?'),
        content: Text('!$iid  $title\n\nThis cannot be undone.'),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(ctx).pop(true),
            child: const Text('Merge'),
          ),
        ],
      ),
    );
    if (ok != true || !mounted) return;
    final r = await widget.bridge.gitlabMrMerge(iid);
    if (!mounted) return;
    _snack(
      r['error'] != null
          ? '${r['error']}'
          : 'Merged !${(r['merge_request'] as Map?)?['iid'] ?? iid}',
    );
    _refresh();
  }

  Future<void> _newIssue() async {
    final titleCtrl = TextEditingController();
    final descCtrl = TextEditingController();
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('New issue'),
        content: SizedBox(
          width: 480,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              TextField(
                controller: titleCtrl,
                autofocus: true,
                decoration: const InputDecoration(labelText: 'Title'),
              ),
              TextField(
                controller: descCtrl,
                maxLines: 6,
                decoration: const InputDecoration(labelText: 'Description'),
              ),
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(ctx).pop(true),
            child: const Text('Open issue'),
          ),
        ],
      ),
    );
    if (ok != true || !mounted) return;
    final r = await widget.bridge.gitlabIssueCreate(
      title: titleCtrl.text.trim(),
      description: descCtrl.text.trim().isEmpty ? null : descCtrl.text,
    );
    titleCtrl.dispose();
    descCtrl.dispose();
    if (!mounted) return;
    _snack(
      r['error'] != null
          ? '${r['error']}'
          : 'Opened #${(r['issue'] as Map?)?['iid'] ?? ''}',
    );
    _refresh();
  }

  Future<void> _configure() async {
    final hostCtl = TextEditingController(text: 'https://gitlab.com');
    final projCtl = TextEditingController();
    final tokenCtl = TextEditingController();
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('GitLab account'),
        content: SizedBox(
          width: 440,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              TextField(
                controller: hostCtl,
                autofocus: true,
                decoration: const InputDecoration(
                  labelText: 'GitLab host',
                  helperText: 'gitlab.com or a self-managed instance',
                ),
              ),
              TextField(
                controller: projCtl,
                decoration: const InputDecoration(
                  labelText: 'Default project (group/repo)',
                  helperText: 'Optional — per-op override',
                ),
              ),
              TextField(
                controller: tokenCtl,
                obscureText: true,
                decoration: const InputDecoration(
                  labelText: 'Personal access token (api scope)',
                  helperText: 'Stored in the OS keystore — never in a file',
                ),
              ),
            ],
          ),
        ),
        actions: [
          TextButton(
            onPressed: () => Navigator.of(ctx).pop(false),
            child: const Text('Cancel'),
          ),
          FilledButton(
            onPressed: () => Navigator.of(ctx).pop(true),
            child: const Text('Save'),
          ),
        ],
      ),
    );
    if (ok != true || !mounted) return;
    final r = await widget.bridge.gitlabConfigure(
      host: hostCtl.text.trim(),
      token: tokenCtl.text.isEmpty ? null : tokenCtl.text,
      project: projCtl.text.trim().isEmpty ? null : projCtl.text.trim(),
    );
    hostCtl.dispose();
    projCtl.dispose();
    tokenCtl.dispose();
    if (!mounted) return;
    if (r['error'] != null) {
      _snack('${r['error']}');
      return;
    }
    _snack(
      r['token_stored'] == true
          ? 'GitLab configured — token saved to the OS keystore'
          : 'GitLab configured — set PAI_GITLAB_TOKEN or re-save with a token',
    );
    _refresh();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('GitLab'),
        actions: [
          IconButton(
            icon: const Icon(Icons.settings_outlined),
            tooltip: 'Configure account',
            onPressed: _configure,
          ),
          if (_tab == 0)
            IconButton(
              icon: const Icon(Icons.add),
              tooltip: 'New issue',
              onPressed: _newIssue,
            ),
          IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _refresh,
          ),
        ],
      ),
      body: Column(
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(AppSpacing.lg,
                AppSpacing.md, AppSpacing.lg, 0),
            child: SegmentedButton<int>(
              segments: const [
                ButtonSegment(
                  value: 0,
                  icon: Icon(Icons.adjust_outlined),
                  label: Text('Issues'),
                ),
                ButtonSegment(
                  value: 1,
                  icon: Icon(Icons.merge_type_outlined),
                  label: Text('Merge requests'),
                ),
                ButtonSegment(
                  value: 2,
                  icon: Icon(Icons.play_circle_outline),
                  label: Text('Pipelines'),
                ),
              ],
              selected: {_tab},
              onSelectionChanged: (s) {
                _tab = s.first;
                _refresh();
              },
            ),
          ),
          if (_tab < 2)
            Padding(
              padding: const EdgeInsets.fromLTRB(AppSpacing.lg,
                  AppSpacing.sm, AppSpacing.lg, AppSpacing.xs),
              child: Row(
                children: [
                  Expanded(
                    child: TextField(
                      controller: _searchCtrl,
                      decoration: const InputDecoration(
                        hintText: 'Search',
                        prefixIcon: Icon(Icons.search, size: 18),
                      ),
                      onSubmitted: (_) => _refresh(),
                    ),
                  ),
                  const SizedBox(width: AppSpacing.sm),
                  DropdownButton<String>(
                    value: _state,
                    items: [
                      for (final s
                          in _tab == 0
                              ? const ['opened', 'closed', 'all']
                              : const ['opened', 'merged', 'closed', 'all'])
                        DropdownMenuItem(value: s, child: Text(s)),
                    ],
                    onChanged: (v) {
                      _state = v ?? 'opened';
                      _refresh();
                    },
                  ),
                ],
              ),
            ),
          if (_error != null)
            _InlineError(_error!,
                onRetry: _refresh,
                action: !_configured
                    ? TextButton(
                        onPressed: _configure,
                        child: const Text('Set up'))
                    : null),
          Expanded(
            child: _loading
                ? _listSkeleton(context)
                : _rows.isEmpty && _error == null
                ? _EmptyState(
                    icon: switch (_tab) {
                      1 => Icons.merge_type_outlined,
                      2 => Icons.play_circle_outline,
                      _ => Icons.adjust_outlined,
                    },
                    title: switch (_tab) {
                      1 => 'No merge requests',
                      2 => 'No pipelines',
                      _ => 'No issues',
                    })
                : ListView.builder(
                    itemCount: _rows.length,
                    itemBuilder: (ctx, i) {
                      final m = _rows[i] as Map<String, dynamic>;
                      return switch (_tab) {
                        1 => ListTile(
                          leading: Icon(
                            m['state'] == 'merged'
                                ? Icons.merge
                                : m['state'] == 'closed'
                                ? Icons.close
                                : Icons.merge_type_outlined,
                          ),
                          title: Text(
                            m['title'] ?? '',
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                          ),
                          subtitle: Text(
                            '!${m['iid']} · ${m['source_branch']} → ${m['target_branch']} · ${m['author'] ?? ''}',
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                          ),
                          onTap: () => _openMr(m),
                        ),
                        2 => ListTile(
                          leading: Icon(switch ('${m['status']}') {
                            'success' => Icons.check_circle_outline,
                            'failed' => Icons.error_outline,
                            'running' => Icons.play_circle_outline,
                            _ => Icons.schedule_outlined,
                          }),
                          title: Text(
                            '#${m['id']} · ${m['status']}',
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                          ),
                          subtitle: Text(
                            '${m['ref'] ?? ''} · ${_fmtTs(m['created_at'])}',
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                          ),
                        ),
                        _ => ListTile(
                          leading: Icon(
                            m['state'] == 'opened'
                                ? Icons.radio_button_checked_outlined
                                : Icons.check_circle_outline,
                          ),
                          title: Text(
                            m['title'] ?? '',
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                          ),
                          subtitle: Text(
                            '#${m['iid']} · ${m['author'] ?? ''}'
                            '${(m['labels'] as List?)?.isNotEmpty == true ? ' · ${(m['labels'] as List).join(", ")}' : ''}',
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis,
                          ),
                          onTap: () => _openIssue(m),
                        ),
                      };
                    },
                  ),
          ),
        ],
      ),
    );
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
                padding: const EdgeInsets.fromLTRB(AppSpacing.lg,
                    AppSpacing.md, AppSpacing.lg, AppSpacing.xs),
                child: Text(
                    'What the agent may do without asking — '
                    'changes apply immediately.',
                    style: Theme.of(context).textTheme.bodySmall),
              ),
              for (final r in _rows) _policyRow(r),
            ]),
    );
  }

  Widget _policyRow(dynamic r) {
    return ListTile(
      title: Text('${r['permission']}',
          style: AppText.mono(context, size: 13)),
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
                    Text('Not a token JSON',
                        style: TextStyle(
                            color: Theme.of(ctx).colorScheme.error)),
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
                    Text('Not a token JSON',
                        style: TextStyle(
                            color: Theme.of(ctx).colorScheme.error)),
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
              padding: const EdgeInsets.all(AppSpacing.sm),
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
                  padding: EdgeInsets.all(AppSpacing.xxl),
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
                    style: AppText.mono(ctx)),
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
                  ? const _EmptyState(
                      icon: Icons.widgets_outlined,
                      title: 'No apps installed',
                      hint: '`pai deploy <dir>` on any paired device '
                          'syncs them here.')
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
                          style: AppText.mono(context)),
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


/// Feature row used by the welcome/help dialog.
class _FeatureRow extends StatelessWidget {
  const _FeatureRow(
      {required this.icon, required this.title, required this.body});
  final IconData icon;
  final String title;
  final String body;
  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final tt = Theme.of(context).textTheme;
    return Padding(
      padding: const EdgeInsets.only(bottom: AppSpacing.md),
      child: Row(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Icon(icon, size: 20, color: cs.primary),
        const SizedBox(width: AppSpacing.md),
        Expanded(
            child:
                Column(crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
              Text(title, style: tt.titleSmall),
              Text(body, style: tt.bodySmall),
            ])),
      ]),
    );
  }
}

/// Shortcut row used by the welcome/help dialog.
class _ShortcutRow extends StatelessWidget {
  const _ShortcutRow(this.keys, this.action);
  final String keys;
  final String action;
  @override
  Widget build(BuildContext context) {
    final cs = Theme.of(context).colorScheme;
    final tt = Theme.of(context).textTheme;
    return Padding(
      padding: const EdgeInsets.symmetric(
          vertical: AppSpacing.x2),
      child: Row(children: [
        SizedBox(
            width: 110,
            child: Text(keys,
                style: AppText.mono(context,
                    size: 12, color: cs.primary))),
        Text(action, style: tt.bodySmall),
      ]),
    );
  }
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
    final sizeCtl = TextEditingController(text: '512');
    String kind = 'audio';
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, setDialog) => AlertDialog(
          title: const Text('Generate media'),
          content: Column(mainAxisSize: MainAxisSize.min, children: [
            Padding(
              padding:
                  const EdgeInsets.only(bottom: AppSpacing.md - 2),
              child: Text(
                  'Runs on this device, or on a paired mesh peer when it '
                  'advertises media-run.',
                  style: Theme.of(ctx).textTheme.bodySmall),
            ),
            DropdownButtonFormField<String>(
              initialValue: kind,
              decoration: const InputDecoration(labelText: 'Kind'),
              items: const [
                DropdownMenuItem(value: 'audio', child: Text('Audio')),
                DropdownMenuItem(value: 'image', child: Text('Image')),
                DropdownMenuItem(value: 'video', child: Text('Video')),
              ],
              onChanged: (v) => setDialog(() => kind = v ?? 'audio'),
            ),
            const SizedBox(height: AppSpacing.md),
            TextField(
              controller: promptCtl,
              autofocus: true,
              maxLines: 3,
              minLines: 1,
              decoration: const InputDecoration(
                  labelText: 'Prompt',
                  hintText: 'e.g. calm lo-fi rain ambience'),
            ),
            const SizedBox(height: AppSpacing.md),
            if (kind == 'audio')
              TextField(
                controller: secsCtl,
                keyboardType: TextInputType.number,
                decoration: const InputDecoration(
                    labelText: 'Duration (seconds, 1-300)'),
              )
            else
              TextField(
                controller: sizeCtl,
                keyboardType: TextInputType.number,
                decoration: const InputDecoration(
                    labelText: 'Size (px, square)'),
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
      ),
    );
    final prompt = promptCtl.text.trim();
    final secs = int.tryParse(secsCtl.text.trim()) ?? 10;
    final size = int.tryParse(sizeCtl.text.trim()) ?? 512;
    promptCtl.dispose();
    secsCtl.dispose();
    sizeCtl.dispose();
    if (ok != true || prompt.isEmpty || !mounted) return;
    try {
      final r = await widget.bridge.mediaGen(prompt,
          kind: kind,
          seconds: secs,
          width: kind == 'audio' ? null : size,
          height: kind == 'audio' ? null : size);
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
    final ext = switch (job['kind'] as String? ?? 'text_to_audio') {
      'text_to_image' || 'image_edit' || 'upscale' => 'png',
      'text_to_video' => 'mp4',
      _ => 'wav',
    };
    final destCtl = TextEditingController(
        text: 'media-${(job['id'] as String? ?? 'job').substring(0, 8)}.$ext');
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('Export result'),
        content: TextField(
          controller: destCtl,
          autofocus: true,
          decoration: const InputDecoration(
              labelText: 'Destination path',
              hintText: 'e.g. clip.wav'),
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

  Color _stateColor(String state) {
    final cs = Theme.of(context).colorScheme;
    final brand = context.brand;
    return switch (state) {
      'done' => brand.success,
      'running' => cs.primary,
      'failed' => cs.error,
      _ => brand.textMuted,
    };
  }

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
          padding: const EdgeInsets.only(right: AppSpacing.md),
          child: FilledButton.icon(
              icon: const Icon(Icons.add, size: 18),
              label: const Text('Generate'),
              onPressed: _generate),
        ),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _jobs.isEmpty
                  ? const _EmptyState(
                      icon: Icons.auto_awesome_outlined,
                      title: 'No media jobs yet',
                      hint: 'Generate media from a prompt — it lands here.')
                  : ListView.builder(
                      padding: const EdgeInsets.symmetric(
                          horizontal: AppSpacing.md,
                          vertical: AppSpacing.sm),
                      itemCount: _jobs.length,
                      itemBuilder: (_, i) {
                        final j = _jobs[i];
                        final state = '${j['state']}';
                        final kind = '${j['kind']}';
                        final err = j['error'] as String?;
                        final done =
                            state == 'done' && j['result_blob'] != null;
                        return Card(
                          margin: const EdgeInsets.only(
                              bottom: AppSpacing.sm),
                          child: Padding(
                            padding:
                                const EdgeInsets.all(AppSpacing.md),
                            child: Column(
                              crossAxisAlignment:
                                  CrossAxisAlignment.start,
                              children: [
                                Row(children: [
                                  Icon(_kindIcon(kind),
                                      size: 20,
                                      color: _stateColor(state)),
                                  const SizedBox(
                                      width: AppSpacing.sm),
                                  Expanded(
                                      child: Text('${j['prompt']}',
                                          maxLines: 1,
                                          overflow:
                                              TextOverflow.ellipsis,
                                          style: Theme.of(context)
                                              .textTheme
                                              .titleSmall)),
                                  if (done)
                                    IconButton(
                                        icon: const Icon(
                                            Icons.save_alt,
                                            size: 18),
                                        tooltip: 'Export result',
                                        onPressed: () => _export(j)),
                                ]),
                                const SizedBox(
                                    height: AppSpacing.sm),
                                Wrap(
                                    spacing: AppSpacing.sm - 2,
                                    runSpacing: AppSpacing.xs,
                                    children: [
                                      _TagChip(state,
                                          color:
                                              _stateColor(state)),
                                      _TagChip(_kindLabel(kind)),
                                      if (j['worker'] != null)
                                        _TagChip(
                                            'worker ${(j['worker'] as String).substring(0, 8)}'),
                                      _TagChip(
                                          _fmtTs(j['created_at'])),
                                    ]),
                                if (err != null && err.isNotEmpty)
                                  Padding(
                                    padding: const EdgeInsets.only(
                                        top: AppSpacing.xs),
                                    child: Text(err,
                                        style: Theme.of(context)
                                            .textTheme
                                            .bodySmall
                                            ?.copyWith(
                                                color: cs.error)),
                                  ),
                              ],
                            ),
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
  List<dynamic> _placement = const [];
  List<dynamic> _models = const [];
  Map<String, dynamic> _status = const {};
  bool _loading = true;
  bool _scanning = false;
  String? _serving;
  String? _error;
  StreamSubscription<Map<String, dynamic>>? _packSub;

  @override
  void initState() {
    super.initState();
    _load();
    // The runtime's drive watcher rescans on mount/unmount and pushes
    // `ui:model_packs` — refresh the list live instead of waiting for
    // a manual Rescan.
    _packSub = widget.bridge.uiEvents.listen((ev) {
      if (ev['kind'] != 'ui:model_packs' || !mounted) return;
      _load();
      ScaffoldMessenger.of(context).showSnackBar(SnackBar(
          content: Text(
              'Model storage changed — ${ev['scanned']} model(s) known'),
          duration: const Duration(seconds: 3)));
    });
  }

  @override
  void dispose() {
    _packSub?.cancel();
    super.dispose();
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
      List<dynamic> placement = const [];
      try {
        st = await widget.bridge.status();
      } catch (_) {}
      try {
        models = await widget.bridge.modelsList();
      } catch (_) {}
      try {
        final pl = await widget.bridge.devicesPlacement();
        placement = (pl['devices'] as List? ?? const []);
      } catch (_) {}
      if (!mounted) return;
      setState(() {
        _detect = d;
        _peers = (p['peers'] as List? ?? const []);
        _placement = placement;
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

  bool _syncing = false;

  /// Placement view for one device: name/platform, the ops it
  /// advertises (`bcap`), load + score the broker would see, and the
  /// local placement weight. `announced=false` means no `bcap` object
  /// has synced yet — the device isn't currently routable.
  Widget _deviceCard(ColorScheme cs, Map dev) {
    final tt = Theme.of(context).textTheme;
    final brand = context.brand;
    final ops = (dev['ops'] as List? ?? const []).cast<String>();
    final load = dev['load'] as Map?;
    final announced = dev['announced'] == true;
    final fresh = dev['fresh'] == true;
    final weight = (dev['weight'] as num?)?.toInt() ?? 0;
    final score = dev['score'] as num?;
    final age = dev['age_secs'] as num?;
    final isSelf = dev['self'] == true;
    final loadBits = <String>[
      if (load != null) ...[
        'busy ${load['busy'] ?? 0}',
        '${((load['ram_bytes'] as num? ?? 0) / (1 << 30)).toStringAsFixed(0)} GB',
        '${load['cpu_cores'] ?? '?'} cores',
        if (load['on_battery'] == true) 'on battery',
        if (load['thermal_throttled'] == true) 'throttled',
      ],
    ].join(' · ');
    return Card(
        margin: const EdgeInsets.only(bottom: AppSpacing.sm),
        child: Padding(
      padding: const EdgeInsets.symmetric(
          horizontal: AppSpacing.lg, vertical: AppSpacing.md),
      child: Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
        Row(children: [
          Icon(isSelf ? Icons.computer : Icons.devices,
              size: 20, color: cs.onSurfaceVariant),
          const SizedBox(width: AppSpacing.sm),
          Expanded(
              child: Text('${dev['name']}',
                  style: tt.titleSmall,
                  overflow: TextOverflow.ellipsis)),
          if (isSelf) _TagChip('local', color: cs.primary),
          if (announced)
            Tooltip(
                message: fresh
                    ? 'Announced ${age}s ago — routable'
                    : 'Announcement ${age}s old — past TTL, not routable',
                child: Icon(
                    fresh ? Icons.check_circle_outline : Icons.schedule,
                    size: 16,
                    color: fresh ? brand.success : cs.error)),
        ]),
        const SizedBox(height: AppSpacing.x2),
        Text(
            '${dev['platform']} · ${(dev['id'] as String).substring(0, 8)}',
            style: tt.bodySmall),
        if (ops.isNotEmpty || weight != 0 || score != null)
          const SizedBox(height: AppSpacing.sm),
        if (ops.isNotEmpty)
          Wrap(
              spacing: AppSpacing.sm - 2,
              runSpacing: AppSpacing.xs,
              children: [
                for (final op in ops)
                  _TagChip(op, tooltip: 'advertised op'),
              ]),
        if (score != null || weight != 0)
          Padding(
            padding: const EdgeInsets.only(top: AppSpacing.xs),
            child: Text(
                [
                  if (score != null) 'score $score',
                  if (weight != 0)
                    'weight ${weight > 0 ? '+$weight' : '$weight'}',
                  if (score != null && weight != 0)
                    'effective ${score + weight}',
                ].join(' · '),
                style: tt.bodySmall),
          ),
        if (loadBits.isNotEmpty)
          Padding(
            padding: const EdgeInsets.only(top: AppSpacing.x2),
            child: Text(loadBits, style: tt.bodySmall),
          ),
        if (!announced && !isSelf)
          Padding(
            padding: const EdgeInsets.only(top: AppSpacing.xs),
            child: Text('No capability announcement synced yet',
                style: tt.bodySmall),
          ),
      ]),
    ));
  }

  /// File-based pairing: this device offers a signed `offer.pai`, the
  /// other device accepts it (vault key sealed to the offerer), and the
  /// offerer completes. Any file transport works — flash drive, shared
  /// folder, message attachment.
  Future<void> _pairDialog() async {
    await showDialog(
      context: context,
      builder: (ctx) => SimpleDialog(
        title: const Text('Pair a device'),
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(AppSpacing.xxl, 0,
                AppSpacing.xxl, AppSpacing.sm),
            child: Text(
                'A signed offer travels one way, the sealed accept '
                'travels back. Any file transport works - flash drive, '
                'shared folder, attachment.',
                style: Theme.of(ctx).textTheme.bodySmall),
          ),
          SimpleDialogOption(
            onPressed: () {
              Navigator.pop(ctx);
              _pairQr();
            },
            child: const ListTile(
                leading: Icon(Icons.qr_code_2),
                title: Text('Pair via QR'),
                subtitle: Text(
                    'Show a code the other device scans - or paste an '
                    'offer payload to show the accept code back')),
          ),
          SimpleDialogOption(
            onPressed: () {
              Navigator.pop(ctx);
              _pairFolder();
            },
            child: const ListTile(
                leading: Icon(Icons.folder_shared),
                title: Text('Exchange via sync folder'),
                subtitle: Text(
                    'Publish and process pairing files in the configured '
                    'shared folder - run the same step on the other device')),
          ),
          SimpleDialogOption(
            onPressed: () {
              Navigator.pop(ctx);
              _pairOffer();
            },
            child: const ListTile(
                leading: Icon(Icons.north_east),
                title: Text('Create an offer'),
                subtitle: Text('This device invites another')),
          ),
          SimpleDialogOption(
            onPressed: () {
              Navigator.pop(ctx);
              _pairAccept();
            },
            child: const ListTile(
                leading: Icon(Icons.south_west),
                title: Text('Accept an offer'),
                subtitle: Text("An offer file from the other device")),
          ),
          SimpleDialogOption(
            onPressed: () {
              Navigator.pop(ctx);
              _pairComplete();
            },
            child: const ListTile(
                leading: Icon(Icons.check_circle_outline),
                title: Text('Complete pairing'),
                subtitle: Text('Finish with the returned accept file')),
          ),
        ],
      ),
    );
  }

  /// QR pairing (render side): show this device's offer as a QR the
  /// other device scans; or paste a scanned offer payload to render
  /// the accept QR to show back. No camera plugin needed - the
  /// payload text is also copyable for file/manual transport.
  Future<void> _pairQr() async {
    final r = await widget.bridge.pairQr('offer');
    if (!mounted) return;
    if (r['error'] != null) {
      ScaffoldMessenger.of(context).showSnackBar(
          SnackBar(content: Text('Offer QR failed: ${r['error']}')));
      return;
    }
    final acceptCtl = TextEditingController();
    String? acceptErr;
    Map<String, dynamic>? acceptQr;
    await showDialog(
      context: context,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, setDialog) => AlertDialog(
          title: Text(acceptQr == null
              ? 'Pair via QR - offer'
              : 'Show this to the offerer'),
          content: SingleChildScrollView(
            child: Column(mainAxisSize: MainAxisSize.min, children: [
              if (acceptQr == null) ...[
                _QrView(r, size: 260),
                const SizedBox(height: AppSpacing.sm),
                Text('This device is offering - the other device scans '
                    'this code (or pastes the payload).',
                    style: Theme.of(ctx).textTheme.bodySmall),
                const SizedBox(height: AppSpacing.sm),
                Row(children: [
                  Expanded(
                      child: Text('${r['payload']}',
                          maxLines: 2,
                          overflow: TextOverflow.ellipsis,
                          style: AppText.mono(ctx, size: 9))),
                  IconButton(
                      icon: const Icon(Icons.copy, size: 16),
                      tooltip: 'Copy offer payload',
                      onPressed: () {
                        Clipboard.setData(ClipboardData(
                            text: '${r['payload']}'));
                      }),
                ]),
                const Divider(height: AppSpacing.xl),
                TextField(
                  controller: acceptCtl,
                  maxLines: 2,
                  decoration: InputDecoration(
                    labelText: 'Or paste an offer payload',
                    hintText: '{"kind":"offer",…}',
                    errorText: acceptErr,
                  ),
                ),
                const SizedBox(height: AppSpacing.sm),
                FilledButton.tonal(
                  onPressed: () async {
                    final offer = acceptCtl.text.trim();
                    if (offer.isEmpty) return;
                    final a = await widget.bridge
                        .pairQr('accept', offer: offer);
                    if (a['error'] != null) {
                      setDialog(() => acceptErr = '${a['error']}');
                    } else {
                      setDialog(() {
                        acceptQr = a;
                        acceptErr = null;
                      });
                    }
                  },
                  child: const Text('Accept & show code'),
                ),
              ] else ...[
                _QrView(acceptQr!, size: 260),
                const SizedBox(height: AppSpacing.sm),
                Text('Accepted - the offerer scans this (or the payload) '
                    'and completes pairing.',
                    style: Theme.of(ctx).textTheme.bodySmall),
                const SizedBox(height: AppSpacing.sm),
                Row(children: [
                  Expanded(
                      child: Text('${acceptQr!['payload']}',
                          maxLines: 2,
                          overflow: TextOverflow.ellipsis,
                          style: AppText.mono(ctx, size: 9))),
                  IconButton(
                      icon: const Icon(Icons.copy, size: 16),
                      tooltip: 'Copy accept payload',
                      onPressed: () {
                        Clipboard.setData(ClipboardData(
                            text: '${acceptQr!['payload']}'));
                      }),
                ]),
              ],
            ]),
          ),
          actions: [
            TextButton(
                onPressed: () => Navigator.pop(ctx),
                child: const Text('Done')),
          ],
        ),
      ),
    );
    acceptCtl.dispose();
    _load();
  }

  Future<void> _pairOffer() async {
    final r = await _pathPrompt(
        'Create pairing offer', 'offer.pai', 'Where to write the offer');
    if (r == null) return;
    final out = await widget.bridge.pairOffer(r);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(out['error'] != null
            ? 'Offer failed: ${out['error']}'
            : 'Offer written to ${out['offer']} - send it to the other device')));
    _load();
  }

  Future<void> _pairAccept() async {
    final offer = await _pathPrompt(
        'Accept an offer', '', 'Path to the received offer.pai');
    if (offer == null || offer.isEmpty || !mounted) return;
    final out = await _pathPrompt(
        'Write the accept file', 'accept.pai', 'Where to write accept.pai');
    if (out == null || out.isEmpty || !mounted) return;
    final r = await widget.bridge.pairAccept(offer, out);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? 'Accept failed: ${r['error']}'
            : "Paired with ${r['peer']} - return ${r['accept']} to the offerer")));
    _load();
  }

  Future<void> _pairComplete() async {
    final accept = await _pathPrompt(
        'Complete pairing', '', 'Path to the returned accept.pai');
    if (accept == null || accept.isEmpty || !mounted) return;
    final r = await widget.bridge.pairComplete(accept);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? 'Complete failed: ${r['error']}'
            : 'Paired with ${r['paired']} - vault key installed')));
    _load();
  }

  /// Pairing through the configured shared sync folder - publishes our
  /// offer, accepts pending offers, completes accepts addressed to us.
  /// The same step on the other device finishes the exchange.
  Future<void> _pairFolder() async {
    final r = await widget.bridge.pairFolder();
    if (!mounted) return;
    _load();
    ScaffoldMessenger.of(context)
        .showSnackBar(SnackBar(content: Text(_pairFolderSummary(r))));
  }

  String _pairFolderSummary(Map<String, dynamic> r) {
    if (r['error'] != null) return 'Folder pairing failed: ${r['error']}';
    final accepted = (r['accepted'] as List? ?? const []);
    final completed = (r['completed'] as List? ?? const []);
    final rejected = (r['rejected'] as List? ?? const []);
    final parts = <String>['Offer published'];
    if (accepted.isNotEmpty) {
      parts.add('accepted ${accepted.join(', ')}');
    }
    if (completed.isNotEmpty) {
      parts.add('paired with ${completed.join(', ')}');
    }
    if (rejected.isNotEmpty) {
      parts.add('${rejected.length} file(s) rejected');
    }
    if (accepted.isEmpty && completed.isEmpty) {
      parts.add('run the same step on the other device to finish');
    }
    return parts.join(' - ');
  }

  /// One-field path prompt; null on cancel.
  Future<String?> _pathPrompt(
      String title, String initial, String label) async {
    final ctl = TextEditingController(text: initial);
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(title),
        content: TextField(
          controller: ctl,
          autofocus: true,
          decoration: InputDecoration(labelText: label),
          onSubmitted: (_) => Navigator.pop(ctx, true),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx, false),
              child: const Text('Cancel')),
          FilledButton(
              onPressed: () => Navigator.pop(ctx, true),
              child: const Text('OK')),
        ],
      ),
    );
    final v = ctl.text.trim();
    ctl.dispose();
    return ok == true ? v : null;
  }

  /// Sync-target dialog — LAN (zero-config mesh), a shared folder, or
  /// a relay URL. The choice persists under sync.* meta, so a second
  /// run needs no args.
  Future<void> _syncDialog() async {
    final st = await widget.bridge.syncStatus();
    if (!mounted) return;
    var transport = st['lan'] == true
        ? 'lan'
        : st['dir'] != null
            ? 'dir'
            : st['relay'] != null
                ? 'relay'
                : 'lan';
    var mode = 'run';
    var auto = (st['auto_minutes'] as num?)?.toInt() ?? 0;
    final pathCtl = TextEditingController(
        text: transport == 'dir'
            ? '${st['dir'] ?? ''}'
            : transport == 'relay'
                ? '${st['relay'] ?? ''}'
                : '');
    final ok = await showDialog<bool>(
      context: context,
      builder: (ctx) => StatefulBuilder(
        builder: (ctx, setD) => AlertDialog(
          title: const Text('Sync now'),
          content: SizedBox(
            width: 420,
            child: Column(mainAxisSize: MainAxisSize.min, children: [
              SegmentedButton<String>(
                segments: const [
                  ButtonSegment(value: 'lan', label: Text('This LAN')),
                  ButtonSegment(
                      value: 'dir', label: Text('Shared folder')),
                  ButtonSegment(value: 'relay', label: Text('Relay')),
                ],
                selected: {transport},
                onSelectionChanged: (v) =>
                    setD(() => transport = v.first),
              ),
              const SizedBox(height: AppSpacing.md),
              SegmentedButton<String>(
                segments: const [
                  ButtonSegment(value: 'run', label: Text('Both')),
                  ButtonSegment(value: 'push', label: Text('Push')),
                  ButtonSegment(value: 'pull', label: Text('Pull')),
                ],
                selected: {mode},
                onSelectionChanged: (v) => setD(() => mode = v.first),
              ),
              if (transport != 'lan') ...[
                const SizedBox(height: AppSpacing.md),
                TextField(
                  controller: pathCtl,
                  decoration: InputDecoration(
                      labelText: transport == 'dir'
                          ? 'Shared folder path'
                          : 'Relay URL (and token, url#token)',
                      hintText: transport == 'dir'
                          ? r'X:\pai-sync or \\nas\pai-sync'
                          : 'http://host:port'),
                ),
              ],
              const SizedBox(height: AppSpacing.md),
              DropdownButtonFormField<int>(
                initialValue: auto,
                decoration: const InputDecoration(
                    labelText: 'Auto-sync',
                    helperText:
                        'Background syncs on this target while the app runs'),
                items: const [
                  DropdownMenuItem(value: 0, child: Text('Off')),
                  DropdownMenuItem(
                      value: 5, child: Text('Every 5 minutes')),
                  DropdownMenuItem(
                      value: 15, child: Text('Every 15 minutes')),
                  DropdownMenuItem(value: 60, child: Text('Hourly')),
                ],
                onChanged: (v) => setD(() => auto = v ?? 0),
              ),
            ]),
          ),
          actions: [
            TextButton(
                onPressed: () => Navigator.pop(ctx, false),
                child: const Text('Cancel')),
            FilledButton(
                onPressed: () => Navigator.pop(ctx, true),
                child: const Text('Sync')),
          ],
        ),
      ),
    );
    final path = pathCtl.text.trim();
    pathCtl.dispose();
    if (ok != true || !mounted) return;
    String? dir, relay, token;
    if (transport == 'dir') dir = path;
    if (transport == 'relay') {
      final parts = path.split('#');
      relay = parts.first;
      if (parts.length > 1) token = parts[1];
    }
    setState(() => _syncing = true);
    final r = await widget.bridge.syncNow(
        mode: mode,
        dir: dir,
        relay: relay,
        token: token,
        lan: transport == 'lan',
        autoMinutes: auto);
    if (!mounted) return;
    setState(() => _syncing = false);
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? 'Sync failed: ${r['error']}'
            : 'Synced - pushed ${r['pushed']}, pulled ${r['pulled']}, '
                'skipped ${r['skipped']}')));
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
    final refCtl = TextEditingController();
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
                          '${m['family']} - ${m['quant'] ?? '?'} - ${m['size_mb']} MB'),
                    ),
                ]),
              ),
              const SizedBox(height: AppSpacing.sm),
              TextField(
                controller: refCtl,
                decoration: const InputDecoration(
                    labelText: 'Custom reference (optional)',
                    hintText:
                        'hf://owner/repo/file.gguf — overrides the pick above'),
              ),
              const SizedBox(height: AppSpacing.sm),
              TextField(
                controller: destCtl,
                decoration: const InputDecoration(
                    labelText: 'Destination (optional)',
                    hintText:
                        'e.g. D:\\pai-models - leave empty for internal'),
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
    final ref = refCtl.text.trim();
    destCtl.dispose();
    refCtl.dispose();
    if (ok != true || (chosen == null && ref.isEmpty) || !mounted) return;
    final slug = ref.isNotEmpty ? ref : chosen!;
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
        IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : ListView(padding: AppSpacing.page, children: [
              if (_error != null)
                _InlineError(_error!, onRetry: _load),
              Text('This device',
                  style: Theme.of(context).textTheme.titleMedium),
              const SizedBox(height: AppSpacing.xs),
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
                      margin: const EdgeInsets.only(
                          bottom: AppSpacing.sm),
                      child: Padding(
                    padding: const EdgeInsets.all(AppSpacing.md + 2),
                    child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Row(children: [
                            Icon(Icons.bolt, color: cs.primary, size: 20),
                            const SizedBox(width: AppSpacing.sm),
                            Expanded(
                                child: Text(
                                    '${e['provider']} · ${e['base_url']}',
                                    style: Theme.of(context)
                                        .textTheme
                                        .titleSmall)),
                          ]),
                          const SizedBox(height: AppSpacing.sm),
                          Wrap(
                              spacing: AppSpacing.sm - 2,
                              runSpacing: AppSpacing.sm - 2,
                              children: [
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
                                  style: Theme.of(context)
                                      .textTheme
                                      .bodySmall),
                          ]),
                        ]),
                  )),
              const SizedBox(height: AppSpacing.xs),
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
              const SizedBox(height: AppSpacing.xl),
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
              const SizedBox(height: AppSpacing.xs),
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
                      margin: const EdgeInsets.only(
                          bottom: AppSpacing.sm),
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
                        ? _TagChip('serving', color: cs.primary)
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
                                : _TagChip('offline',
                                    tooltip: 'Drive not mounted — '
                                        'plug it in and rescan'),
                  )),
              const SizedBox(height: AppSpacing.xl),
              Text('Paired devices',
                  style: Theme.of(context).textTheme.titleMedium),
              const SizedBox(height: AppSpacing.xs),
              if (_peers.isEmpty && _placement.isEmpty)
                Card(
                    child: ListTile(
                  leading: Icon(Icons.phonelink_off_outlined,
                      color: cs.onSurfaceVariant),
                  title: const Text('No paired devices'),
                  subtitle: const Text(
                      'Pair another machine with `pai pair` — media jobs and app runs can then route to it.'),
                ))
              else
                for (final dev in _placement.isNotEmpty
                    ? _placement
                    : _peers.map((p) => {
                          'id': p['id'],
                          'name': p['name'],
                          'platform': p['platform'],
                          'self': false,
                        }))
                  _deviceCard(cs, dev as Map),
              const SizedBox(height: AppSpacing.md),
              Align(
                alignment: Alignment.centerLeft,
                child: _syncing
                    ? const Padding(
                        padding: EdgeInsets.all(AppSpacing.sm),
                        child: Row(mainAxisSize: MainAxisSize.min,
                            children: [
                              SizedBox(
                                  width: 16,
                                  height: 16,
                                  child: CircularProgressIndicator(
                                      strokeWidth: 2)),
                              SizedBox(width: AppSpacing.md - 2),
                              Text('Syncing…'),
                            ]),
                      )
                    : Row(mainAxisSize: MainAxisSize.min, children: [
                        FilledButton.tonalIcon(
                            icon: const Icon(Icons.sync, size: 18),
                            label: const Text('Sync now'),
                            onPressed: _syncDialog),
                        const SizedBox(width: AppSpacing.sm),
                        TextButton.icon(
                            icon: const Icon(Icons.add_link, size: 18),
                            label: const Text('Pair a device'),
                            onPressed: _pairDialog),
                      ]),
              ),
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
        IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _items.isEmpty
                  ? _EmptyState(
                      icon: Icons.notifications_outlined,
                      title: _unreadOnly
                          ? 'No unread notifications'
                          : 'Nothing yet',
                      hint: _unreadOnly
                          ? null
                          : 'The agent posts reminders and proactive '
                              'notes here.')
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
        IconButton(
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : _error != null
              ? _errorView(context, _error!, _load)
              : _events.isEmpty
                  ? const _EmptyState(
                      icon: Icons.receipt_long_outlined,
                      title: 'Nothing yet',
                      hint: 'Every permission-gated action is logged here.')
                  : ListView.builder(
                      itemCount: _events.length,
                      itemBuilder: (ctx, i) {
                        final e = _events[i] as Map<String, dynamic>;
                        final outcome = '${e['outcome'] ?? ''}';
                        final tool = e['tool'];
                        final detail = e['detail'];
                        final outcomeColor = switch (outcome) {
                          'ok' => context.brand.success,
                          'denied' => context.brand.warning,
                          'error' => cs.error,
                          _ => context.brand.info,
                        };
                        return ListTile(
                          dense: true,
                          leading: Icon(_iconFor(outcome),
                              size: 18, color: outcomeColor),
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
    final tt = Theme.of(context).textTheme;
    final brand = context.brand;
    final chip = Container(
      padding: const EdgeInsets.symmetric(
          horizontal: AppSpacing.sm, vertical: AppSpacing.xs),
      decoration: BoxDecoration(
        color: serving
            ? cs.primary.withValues(alpha: 0.16)
            : embedOnly
                ? brand.surfaceOverlay.withValues(alpha: 0.6)
                : cs.primary.withValues(alpha: 0.07),
        borderRadius: AppRadii.rSm,
        border: Border.all(
            color: serving
                ? cs.primary.withValues(alpha: 0.5)
                : brand.hairline),
      ),
      child: Row(mainAxisSize: MainAxisSize.min, children: [
        Text(name,
            style: tt.bodySmall?.copyWith(
                color: embedOnly ? brand.textMuted : cs.onSurface)),
        if (tag != null) ...[
          const SizedBox(width: AppSpacing.xs + 1),
          Text(tag,
              style: tt.labelSmall?.copyWith(
                  color: serving ? cs.primary : brand.textMuted)),
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
          borderRadius: AppRadii.rSm, onTap: onTap, child: chip),
    );
  }
}

/// QR bit-matrix painter — renders `{"size": N, "rows": ["0101…"]}`
/// from `pai_pair_qr`. Dark modules on the theme surface; scaled to
/// fit [size] with a quiet-zone border.
class _QrView extends StatelessWidget {
  const _QrView(this.qr, {this.size = 220});
  final Map<String, dynamic> qr;
  final double size;

  @override
  Widget build(BuildContext context) {
    final rows = (qr['rows'] as List? ?? const []).cast<String>();
    if (rows.isEmpty) {
      return const SizedBox.shrink();
    }
    return Container(
      width: size,
      height: size,
      padding: const EdgeInsets.all(AppSpacing.md - 2),
      decoration: BoxDecoration(
          color: Colors.white, borderRadius: AppRadii.rSm),
      child: CustomPaint(painter: _QrPainter(rows)),
    );
  }
}

class _QrPainter extends CustomPainter {
  const _QrPainter(this.rows);
  final List<String> rows;

  @override
  void paint(Canvas canvas, Size size) {
    final n = rows.length;
    if (n == 0) return;
    final cell = math.min(size.width, size.height) / n;
    final paint = Paint()..color = Colors.black;
    for (var y = 0; y < n; y++) {
      final row = rows[y];
      for (var x = 0; x < row.length && x < n; x++) {
        if (row[x] == '1') {
          canvas.drawRect(
              Rect.fromLTWH(x * cell, y * cell, cell + 0.4, cell + 0.4),
              paint);
        }
      }
    }
  }

  @override
  bool shouldRepaint(_QrPainter old) => old.rows != rows;
}

/// Settings — appearance (theme, accent, text scale), notification
/// badges, the chat model picker, sync cadence, and storage tools.
/// Writable knobs go through [onPref]; runtime state is reported with
/// deep-links to the screens that own it.
class SettingsScreen extends StatefulWidget {
  const SettingsScreen(
      {super.key,
      required this.bridge,
      required this.themeMode,
      required this.onThemeMode,
      required this.prefs,
      required this.onPref,
      this.onNavigate,
      this.onHelp});
  final PaiBridge bridge;
  final ThemeMode themeMode;
  final ValueChanged<ThemeMode> onThemeMode;
  final Map<String, dynamic> prefs;
  final void Function(String key, Object? value) onPref;
  final ValueChanged<int>? onNavigate;
  final VoidCallback? onHelp;
  @override
  State<SettingsScreen> createState() => _SettingsScreenState();
}

class _SettingsScreenState extends State<SettingsScreen> {
  Map<String, dynamic> _status = const {};
  Map<String, dynamic> _voice = const {};
  Map<String, dynamic> _sync = const {};
  List<dynamic> _endpoints = const [];
  String _dataDir = '';
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _dataDir = appDataDir();
    _load();
  }

  Future<void> _load() async {
    Map<String, dynamic> st = const {}, voice = const {}, sync = const {};
    List<dynamic> endpoints = const [];
    try {
      st = await widget.bridge.status();
    } catch (_) {}
    try {
      voice = await widget.bridge.voiceStatus();
    } catch (_) {}
    try {
      sync = await widget.bridge.syncStatus();
    } catch (_) {}
    try {
      final d = await widget.bridge.detect();
      endpoints = (d['endpoints'] as List? ?? const []);
    } catch (_) {}
    if (mounted) {
      setState(() {
        _status = st;
        _voice = voice;
        _sync = sync;
        _endpoints = endpoints;
        _loading = false;
      });
    }
  }

  /// Every (endpoint, model) pair detected on this machine — the
  /// provider picker's choices.
  List<(String, String)> get _modelChoices => [
        for (final e in _endpoints)
          for (final m in (e['models'] as List? ?? const []))
            ('${e['base_url']}', '$m'),
      ];

  Future<void> _pickModel(String baseUrl, String model) async {
    final r = await widget.bridge
        .setProvider(serverUrl: baseUrl, model: model);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(
            r['error'] as String? ?? 'Chat now served by $model')));
    _load();
  }

  /// Change just the cadence — syncNow persists auto_minutes under
  /// sync.* meta and reuses the already-configured target.
  Future<void> _setAutoSync(int minutes) async {
    final r = await widget.bridge.syncNow(autoMinutes: minutes);
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(SnackBar(
        content: Text(r['error'] != null
            ? 'Sync failed: ${r['error']}'
            : minutes == 0
                ? 'Auto-sync off — ran one manual sync.'
                : 'Auto-sync every ${minutes}m — ran a sync now.')));
    _load();
  }

  Future<void> _clearConversations() async {
    final ok = await showDialog<bool>(
        context: context,
        builder: (ctx) => AlertDialog(
              title: const Text('Delete all conversations?'),
              content: const Text(
                  'Every chat transcript is removed. Memories, '
                  'documents, and the audit log are kept — the deletes '
                  'are recorded there.'),
              actions: [
                TextButton(
                    onPressed: () => Navigator.pop(ctx, false),
                    child: const Text('Cancel')),
                FilledButton(
                    onPressed: () => Navigator.pop(ctx, true),
                    child: const Text('Delete all')),
              ],
            ));
    if (ok != true || !mounted) return;
    final convs = await widget.bridge.conversations();
    var n = 0;
    for (final c in convs) {
      final r = await widget.bridge.conversationDelete('${c['id']}');
      if (r['error'] == null) n++;
    }
    if (!mounted) return;
    ScaffoldMessenger.of(context).showSnackBar(
        SnackBar(content: Text('Deleted $n conversation(s).')));
  }

  /// Voice capability flags come back as `true`/`false`, except `stt`
  /// which reports the serving provider name when available.
  bool _voiceOn(String k) => _voice[k] == true || _voice[k] is String;

  Widget _section(String title, List<Widget> children) {
    final tt = Theme.of(context).textTheme;
    return Padding(
      padding: const EdgeInsets.only(bottom: AppSpacing.md),
      child: Card(
        child: Padding(
          padding: AppSpacing.card,
          child: Column(crossAxisAlignment: CrossAxisAlignment.start,
              children: [
            Text(title, style: tt.titleSmall),
            const SizedBox(height: AppSpacing.md),
            ...children,
          ]),
        ),
      ),
    );
  }

  @override
  Widget build(BuildContext context) {
    final brand = context.brand;
    final cs = Theme.of(context).colorScheme;
    final tt = Theme.of(context).textTheme;
    final muted = tt.bodySmall?.copyWith(color: brand.textMuted);
    final echo = _status['provider'] == 'echo';
    final auto = (_sync['auto_minutes'] as num?)?.toInt() ?? 0;
    final syncTarget = _sync['lan'] == true
        ? 'This LAN'
        : _sync['dir'] != null
            ? 'Shared folder'
            : _sync['relay'] != null
                ? 'Relay'
                : 'This LAN';
    return Scaffold(
      appBar: AppBar(title: const Text('Settings'), actions: [
        IconButton(
            tooltip: 'Refresh',
            icon: const Icon(Icons.refresh),
            onPressed: _load),
      ]),
      body: _loading
          ? _listSkeleton(context)
          : ListView(padding: AppSpacing.page, children: [
              _section('Appearance', [
                SegmentedButton<ThemeMode>(
                  segments: const [
                    ButtonSegment(
                        value: ThemeMode.system,
                        icon: Icon(Icons.brightness_auto_outlined),
                        label: Text('System')),
                    ButtonSegment(
                        value: ThemeMode.light,
                        icon: Icon(Icons.light_mode_outlined),
                        label: Text('Light')),
                    ButtonSegment(
                        value: ThemeMode.dark,
                        icon: Icon(Icons.dark_mode_outlined),
                        label: Text('Dark')),
                  ],
                  selected: {widget.themeMode},
                  onSelectionChanged: (s) =>
                      widget.onThemeMode(s.first),
                ),
                const SizedBox(height: AppSpacing.md),
                Align(
                    alignment: Alignment.centerLeft,
                    child: Text('Accent', style: tt.labelMedium)),
                const SizedBox(height: AppSpacing.xs),
                SegmentedButton<String>(
                  segments: const [
                    ButtonSegment(
                        value: 'teal', label: Text('Teal')),
                    ButtonSegment(
                        value: 'brass', label: Text('Brass')),
                    ButtonSegment(
                        value: 'cobalt', label: Text('Cobalt')),
                  ],
                  selected: {'${widget.prefs['accent'] ?? 'teal'}'},
                  onSelectionChanged: (s) =>
                      widget.onPref('accent', s.first),
                ),
                const SizedBox(height: AppSpacing.md),
                Align(
                    alignment: Alignment.centerLeft,
                    child: Text('Text size', style: tt.labelMedium)),
                const SizedBox(height: AppSpacing.xs),
                SegmentedButton<String>(
                  segments: const [
                    ButtonSegment(
                        value: 'compact', label: Text('Compact')),
                    ButtonSegment(
                        value: 'standard', label: Text('Standard')),
                    ButtonSegment(
                        value: 'large', label: Text('Large')),
                  ],
                  selected: {'${widget.prefs['text_scale'] ?? 'standard'}'},
                  onSelectionChanged: (s) =>
                      widget.onPref('text_scale', s.first),
                ),
                const SizedBox(height: AppSpacing.sm),
                Text('All three persist across restarts — the rail\'s '
                    'theme button cycles the same modes.', style: muted),
              ]),
              _section('Chat provider', [
                Wrap(spacing: AppSpacing.sm,
                    runSpacing: AppSpacing.xs, children: [
                  _TagChip('${_status['provider'] ?? 'unknown'}',
                      color: echo ? brand.warning : brand.success),
                  if (_status['model'] != null)
                    _TagChip('${_status['model']}'),
                ]),
                const SizedBox(height: AppSpacing.sm),
                if (_modelChoices.isNotEmpty) ...[
                  DropdownMenu<String>(
                    label: const Text('Serve a model'),
                    initialSelection: null,
                    hintText: 'Pick a detected model',
                    dropdownMenuEntries: [
                      for (final (url, model) in _modelChoices)
                        DropdownMenuEntry(value: '$url#$model', label: model),
                    ],
                    onSelected: (v) {
                      if (v == null) return;
                      final i = v.indexOf('#');
                      _pickModel(v.substring(0, i), v.substring(i + 1));
                    },
                  ),
                  const SizedBox(height: AppSpacing.sm),
                ],
                Text(
                    echo
                        ? 'No model is serving — replies are echoes '
                            'until an endpoint or model pack is picked.'
                        : 'Replies stream from the endpoint above.',
                    style: muted),
                const SizedBox(height: AppSpacing.sm),
                TextButton.icon(
                    onPressed: () => widget.onNavigate?.call(2),
                    icon: const Icon(Icons.devices_outlined, size: 16),
                    label: const Text('Manage endpoints in Devices')),
              ]),
              _section('Notifications', [
                SwitchListTile(
                    contentPadding: EdgeInsets.zero,
                    title: const Text('Alerts badge'),
                    subtitle: const Text(
                        'Unread-count badge on the Alerts rail icon'),
                    value: widget.prefs['badge_alerts'] != false,
                    onChanged: (v) =>
                        widget.onPref('badge_alerts', v)),
                SwitchListTile(
                    contentPadding: EdgeInsets.zero,
                    title: const Text('Email badge'),
                    subtitle: const Text(
                        'Unread-count badge on the Email rail icon'),
                    value: widget.prefs['badge_email'] != false,
                    onChanged: (v) =>
                        widget.onPref('badge_email', v)),
              ]),
              _section('Voice', [
                Wrap(spacing: AppSpacing.sm,
                    runSpacing: AppSpacing.xs, children: [
                  for (final (label, key) in const [
                    ('Microphone', 'mic'),
                    ('Speech-to-text', 'stt'),
                    ('Text-to-speech', 'tts'),
                    ('Speaker', 'speaker'),
                  ])
                    _TagChip(label,
                        color: _voiceOn(key)
                            ? brand.success
                            : brand.textMuted,
                        tooltip: _voiceOn(key)
                            ? 'available'
                            : 'not detected'),
                ]),
                const SizedBox(height: AppSpacing.sm),
                Text('Dictation and spoken replies need a whisper '
                    'server (STT) and piper (TTS). The speak-replies '
                    'toggle lives in the Chat header and persists.',
                    style: muted),
              ]),
              _section('Sync', [
                Wrap(spacing: AppSpacing.sm,
                    runSpacing: AppSpacing.xs, children: [
                  _TagChip(syncTarget),
                  if (_sync['dir'] != null)
                    _TagChip('${_sync['dir']}',
                        tooltip: 'Shared folder'),
                  if (_sync['relay'] != null)
                    _TagChip('${_sync['relay']}', tooltip: 'Relay'),
                ]),
                const SizedBox(height: AppSpacing.md),
                DropdownMenu<int>(
                  label: const Text('Auto-sync'),
                  initialSelection: auto,
                  helperText:
                      'Background syncs on the configured target — '
                          'changing this runs one sync now',
                  dropdownMenuEntries: const [
                    DropdownMenuEntry(value: 0, label: 'Off'),
                    DropdownMenuEntry(
                        value: 5, label: 'Every 5 minutes'),
                    DropdownMenuEntry(
                        value: 15, label: 'Every 15 minutes'),
                    DropdownMenuEntry(value: 60, label: 'Hourly'),
                  ],
                  onSelected: (v) {
                    if (v != null) _setAutoSync(v);
                  },
                ),
                const SizedBox(height: AppSpacing.sm),
                TextButton.icon(
                    onPressed: () => widget.onNavigate?.call(2),
                    icon: const Icon(Icons.sync_outlined, size: 16),
                    label: const Text('Sync target in Devices')),
              ]),
              _section('Storage', [
                Row(children: [
                  Expanded(
                      child: Text(_dataDir,
                          style: AppText.mono(context, size: 12),
                          maxLines: 2,
                          overflow: TextOverflow.ellipsis)),
                  IconButton(
                      tooltip: 'Copy path',
                      icon: const Icon(Icons.copy_outlined, size: 18),
                      onPressed: () async {
                        await Clipboard.setData(
                            ClipboardData(text: _dataDir));
                        if (context.mounted) {
                          ScaffoldMessenger.of(context).showSnackBar(
                              const SnackBar(
                                  content:
                                      Text('Data directory copied')));
                        }
                      }),
                  IconButton(
                      tooltip: 'Open folder',
                      icon: const Icon(Icons.folder_open, size: 18),
                      onPressed: () => revealDataDir(_dataDir)),
                ]),
                const SizedBox(height: AppSpacing.xs),
                Text('Chats, memories, documents, and the audit log '
                    'live here — nothing leaves this folder unless you '
                    'sync.', style: muted),
                const SizedBox(height: AppSpacing.sm),
                TextButton.icon(
                    onPressed: _clearConversations,
                    icon: Icon(Icons.delete_outline,
                        size: 16, color: cs.error),
                    label: Text('Delete all conversations',
                        style: TextStyle(color: cs.error))),
              ]),
              _section('About', [
                Text('Personal AI', style: tt.titleMedium),
                const SizedBox(height: AppSpacing.x2),
                Text('Local-first · private by default · every action '
                    'audited.', style: muted),
                const SizedBox(height: AppSpacing.sm),
                Wrap(spacing: AppSpacing.sm, children: [
                  TextButton.icon(
                      onPressed: widget.onHelp,
                      icon: const Icon(Icons.help_outline, size: 16),
                      label: const Text('Tour & shortcuts')),
                  TextButton.icon(
                      onPressed: () => widget.onNavigate?.call(7),
                      icon: const Icon(Icons.receipt_long_outlined,
                          size: 16),
                      label: const Text('Audit log')),
                ]),
              ]),
            ]),
    );
  }
}

/// One selectable row in the command palette.
final class _PaletteItem {
  const _PaletteItem(this.icon, this.title, this.sub, this.run);
  final IconData icon;
  final String title;
  final String? sub;
  final VoidCallback run;
}

/// Ctrl+P palette — destinations and app actions always listed; two
/// or more characters adds a federated search over docs (via
/// docsSearch), chats, and memories (filtered client-side).
class _CommandPalette extends StatefulWidget {
  const _CommandPalette(
      {required this.bridge,
      required this.onNavigate,
      required this.onConversation,
      required this.onAction});
  final PaiBridge bridge;
  final ValueChanged<int> onNavigate;
  final ValueChanged<String> onConversation;
  final ValueChanged<String> onAction;
  @override
  State<_CommandPalette> createState() => _CommandPaletteState();
}

class _CommandPaletteState extends State<_CommandPalette> {
  final _query = TextEditingController();
  List<dynamic> _convs = const [];
  List<dynamic> _mems = const [];
  List<dynamic> _docs = const [];
  int _sel = 0;
  Timer? _debounce;

  static const _actions = <(String, IconData, String)>[
    ('new', Icons.add_comment_outlined, 'New chat'),
    ('focus', Icons.keyboard_outlined, 'Focus message field'),
    ('sync', Icons.sync_outlined, 'Sync with paired devices'),
    ('theme', Icons.brightness_6_outlined, 'Cycle theme mode'),
    ('help', Icons.help_outline, 'Tour & shortcuts'),
  ];

  @override
  void initState() {
    super.initState();
    _cache();
  }

  @override
  void dispose() {
    _debounce?.cancel();
    _query.dispose();
    super.dispose();
  }

  /// Chats and memories are filtered client-side — cache them once
  /// rather than per keystroke.
  Future<void> _cache() async {
    try {
      final c = await widget.bridge.conversations();
      if (mounted) setState(() => _convs = c);
    } catch (_) {}
    try {
      final m = await widget.bridge.memories();
      if (mounted) setState(() => _mems = m);
    } catch (_) {}
  }

  void _onChanged(String q) {
    _debounce?.cancel();
    _sel = 0;
    setState(() {});
    if (q.trim().length < 2) {
      setState(() => _docs = const []);
      return;
    }
    _debounce = Timer(const Duration(milliseconds: 250), () async {
      try {
        final d = await widget.bridge.docsSearch(q.trim());
        if (mounted && _query.text.trim() == q.trim()) {
          setState(() => _docs = d);
        }
      } catch (_) {}
    });
  }

  List<_PaletteItem> _items() {
    final q = _query.text.trim().toLowerCase();
    bool match(String s) => q.isEmpty || s.toLowerCase().contains(q);
    final items = <_PaletteItem>[];
    for (var i = 0; i < destLabels.length; i++) {
      if (match(destLabels[i])) {
        items.add(_PaletteItem(destIcons[i], destLabels[i], 'Go to',
            () => widget.onNavigate(i)));
      }
    }
    for (final (id, icon, label) in _actions) {
      if (match(label)) {
        items.add(
            _PaletteItem(icon, label, 'Action', () => widget.onAction(id)));
      }
    }
    if (q.length >= 2) {
      for (final c in _convs
          .where((c) => match('${c['title'] ?? ''}')).take(3)) {
        items.add(_PaletteItem(
            Icons.chat_bubble_outline,
            '${c['title'] ?? 'Untitled'}',
            'Chat',
            () => widget.onConversation('${c['id']}')));
      }
      for (final m in _mems
          .where((m) => match('${m['content'] ?? ''}')).take(3)) {
        final text = '${m['content'] ?? ''}';
        items.add(_PaletteItem(
            Icons.psychology_outlined,
            text.length > 60 ? '${text.substring(0, 60)}…' : text,
            'Memory',
            () => widget.onNavigate(4)));
      }
      for (final d in _docs.take(3)) {
        items.add(_PaletteItem(
            Icons.description_outlined,
            '${d['title'] ?? 'Untitled'} §${d['section']}',
            'Document',
            () => widget.onNavigate(5)));
      }
    }
    return items;
  }

  void _run(int i) {
    final items = _items();
    if (items.isEmpty) return;
    items[i.clamp(0, items.length - 1)].run();
  }

  @override
  Widget build(BuildContext context) {
    final brand = context.brand;
    final cs = Theme.of(context).colorScheme;
    final items = _items();
    return Dialog(
      child: SizedBox(
        width: 560,
        child: Column(mainAxisSize: MainAxisSize.min, children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(
                AppSpacing.lg, AppSpacing.md, AppSpacing.lg, 0),
            child: CallbackShortcuts(
              bindings: {
                const SingleActivator(LogicalKeyboardKey.arrowDown): () =>
                    setState(() =>
                        _sel = items.isEmpty ? 0 : (_sel + 1) % items.length),
                const SingleActivator(LogicalKeyboardKey.arrowUp): () =>
                    setState(() => _sel = items.isEmpty
                        ? 0
                        : (_sel - 1 + items.length) % items.length),
              },
              child: TextField(
                controller: _query,
                autofocus: true,
                onChanged: _onChanged,
                onSubmitted: (_) => _run(_sel),
                decoration: const InputDecoration(
                    prefixIcon: Icon(Icons.search, size: 18),
                    hintText: 'Go to, run, or search docs · chats · memories'),
              ),
            ),
          ),
          ConstrainedBox(
            constraints: const BoxConstraints(maxHeight: 380),
            child: items.isEmpty
                ? Padding(
                    padding: const EdgeInsets.all(AppSpacing.xl),
                    child: Text('No matches',
                        style: Theme.of(context)
                            .textTheme
                            .bodySmall
                            ?.copyWith(color: brand.textMuted)))
                : ListView.builder(
                    shrinkWrap: true,
                    itemCount: items.length,
                    itemBuilder: (_, i) {
                      final it = items[i];
                      final sel = i == _sel;
                      return ListTile(
                        dense: true,
                        selected: sel,
                        selectedTileColor:
                            cs.primary.withValues(alpha: 0.10),
                        leading: Icon(it.icon,
                            size: 18,
                            color: sel ? cs.primary : brand.textMuted),
                        title: Text(it.title,
                            maxLines: 1,
                            overflow: TextOverflow.ellipsis),
                        trailing: it.sub == null
                            ? null
                            : Text(it.sub!,
                                style: Theme.of(context)
                                    .textTheme
                                    .labelSmall),
                        onTap: it.run,
                      );
                    }),
          ),
        ]),
      ),
    );
  }
}
