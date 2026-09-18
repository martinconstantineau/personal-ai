import 'dart:convert';
import 'package:web/web.dart' as web;

/// The runtime lives server-side behind `pai serve --bridge`; the web
/// client has no data dir of its own.
String appDataDir() => '';
String appProvider() => 'auto';

/// First-run marker rides in localStorage on web.
bool onboardingSeen(String _) =>
    web.window.localStorage.getItem('pai.onboarded') == 'seen';
void markOnboardingSeen(String _) {
  web.window.localStorage.setItem('pai.onboarded', 'seen');
}

/// Small persisted preferences — `pai.prefs` JSON in localStorage.
/// One flat map; readers default missing keys, so older blobs stay
/// valid.
Map<String, dynamic> loadPrefs(String _) {
  try {
    final raw = web.window.localStorage.getItem('pai.prefs');
    final p = raw == null ? null : jsonDecode(raw);
    if (p is Map<String, dynamic>) return p;
  } catch (_) {}
  // One-time migration: the original theme toggle wrote
  // `pai.theme-mode`.
  final legacy = web.window.localStorage.getItem('pai.theme-mode');
  if (legacy != null && legacy.isNotEmpty) {
    return {'theme_mode': legacy};
  }
  return const {};
}

void savePrefs(String _, Map<String, dynamic> prefs) {
  web.window.localStorage.setItem('pai.prefs', jsonEncode(prefs));
}

/// Theme preference — 'system' | 'light' | 'dark' in the prefs map.
String themeMode(String dataDir) =>
    '${loadPrefs(dataDir)['theme_mode'] ?? 'system'}';

void saveThemeMode(String dataDir, String mode) {
  final p = Map<String, dynamic>.of(loadPrefs(dataDir));
  p['theme_mode'] = mode;
  savePrefs(dataDir, p);
}

/// The data dir is server-side; nothing to reveal in the browser.
void revealDataDir(String _) {}

/// Shown when the bridge is unreachable — the runtime is server-side.
const platformInitHint =
    '(start the gateway: pai serve --bridge — then reload)';
