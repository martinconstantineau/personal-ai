import 'dart:io';
import 'package:flutter/material.dart';
import 'package:path_provider/path_provider.dart';
import 'pai_bridge.dart';

void main() {
  runApp(const PaiApp());
}

class PaiApp extends StatelessWidget {
  const PaiApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Personal AI',
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: const ChatScreen(),
    );
  }
}

class _Entry {
  _Entry(this.role, this.text);
  final String role; // 'you' | 'ai' | 'system'
  String text;
}

class ChatScreen extends StatefulWidget {
  const ChatScreen({super.key});

  @override
  State<ChatScreen> createState() => _ChatScreenState();
}

class _ChatScreenState extends State<ChatScreen> {
  final _input = TextEditingController();
  final _entries = <_Entry>[];
  PaiBridge? _bridge;
  String? _error;
  bool _busy = false;

  @override
  void initState() {
    super.initState();
    _init();
  }

  Future<void> _init() async {
    try {
      final dir = await getApplicationSupportDirectory();
      // Default to the deterministic offline provider; set provider to
      // 'llama-server' + a running local server for real model output.
      _bridge = await PaiBridge.start({
        'data_dir': '${dir.path}/personal-ai',
        'provider': Platform.environment['PAI_PROVIDER'] ?? 'echo',
        'server_url':
            Platform.environment['PAI_SERVER'] ?? 'http://127.0.0.1:8080',
        if (Platform.environment['PAI_MODEL'] != null)
          'model': Platform.environment['PAI_MODEL'],
      });
      setState(() {
        _entries.add(_Entry('system',
            'Local-first · free models · every action is permission-gated and audited.'));
      });
    } catch (e) {
      setState(() => _error = e.toString());
    }
  }

  Future<void> _send() async {
    final text = _input.text.trim();
    if (text.isEmpty || _bridge == null) return;
    _input.clear();
    setState(() {
      _entries.add(_Entry('you', text));
      _busy = true;
    });
    final result = await _bridge!.send(text);
    setState(() {
      _busy = false;
      for (final e in (result['events'] as List? ?? [])) {
        if (e['kind'] == 'tool_executed') {
          _entries.add(_Entry('system', '⚙ ${e['tool']}: ${e['summary']}'));
        }
        if (e['kind'] == 'tool_denied') {
          _entries.add(_Entry('system', '⛔ denied: ${e['tool']}'));
        }
      }
      _entries.add(_Entry('ai',
          result['answer'] as String? ?? result['error'] as String? ?? '…'));
    });
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Personal AI')),
      body: Column(
        children: [
          if (_error != null)
            MaterialBanner(
              content: Text('Init failed: $_error'),
              actions: [TextButton(onPressed: _init, child: const Text('Retry'))],
            ),
          Expanded(
            child: ListView.builder(
              padding: const EdgeInsets.all(12),
              itemCount: _entries.length,
              itemBuilder: (context, i) {
                final e = _entries[i];
                final isYou = e.role == 'you';
                final isSys = e.role == 'system';
                return Align(
                  alignment:
                      isYou ? Alignment.centerRight : Alignment.centerLeft,
                  child: Container(
                    margin: const EdgeInsets.symmetric(vertical: 4),
                    padding: const EdgeInsets.all(10),
                    constraints: const BoxConstraints(maxWidth: 480),
                    decoration: BoxDecoration(
                      color: isSys
                          ? Theme.of(context).colorScheme.surfaceContainerHighest
                          : isYou
                              ? Theme.of(context).colorScheme.primaryContainer
                              : Theme.of(context).colorScheme.secondaryContainer,
                      borderRadius: BorderRadius.circular(12),
                    ),
                    child: Text(e.text,
                        style: isSys
                            ? Theme.of(context).textTheme.bodySmall
                            : Theme.of(context).textTheme.bodyMedium),
                  ),
                );
              },
            ),
          ),
          SafeArea(
            child: Padding(
              padding: const EdgeInsets.all(8),
              child: Row(
                children: [
                  Expanded(
                    child: TextField(
                      controller: _input,
                      onSubmitted: (_) => _send(),
                      decoration: const InputDecoration(
                        hintText: 'Say something — "remember that…", "what is 2 + 3?"',
                        border: OutlineInputBorder(),
                      ),
                    ),
                  ),
                  const SizedBox(width: 8),
                  IconButton.filled(
                    onPressed: _busy ? null : _send,
                    icon: const Icon(Icons.send),
                  ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }
}
