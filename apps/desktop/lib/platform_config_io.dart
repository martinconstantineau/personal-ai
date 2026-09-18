import 'dart:convert';
import 'dart:io';

/// Local data dir — `PAI_DATA_DIR` wins; Android uses the app's private
/// files dir (path_provider is absent: this host lacks symlink privilege).
String appDataDir() => Platform.environment['PAI_DATA_DIR'] ??
    (Platform.isAndroid
        ? '/data/data/com.example.pai_app/files'
        : '${Directory.current.path}/.pai-data');

/// 'auto' probes llama-server / Ollama / LM Studio, falls back to echo.
String appProvider() => Platform.environment['PAI_PROVIDER'] ?? 'auto';

/// First-run marker lives in the data dir on native.
bool onboardingSeen(String dataDir) => File('$dataDir/.onboarded').existsSync();
void markOnboardingSeen(String dataDir) =>
    File('$dataDir/.onboarded').writeAsStringSync('seen');

/// Small persisted preferences — `.prefs.json` in the data dir. One
/// flat map; readers default missing keys, so older files stay valid.
Map<String, dynamic> loadPrefs(String dataDir) {
  try {
    final raw = jsonDecode(File('$dataDir/.prefs.json').readAsStringSync());
    if (raw is Map<String, dynamic>) return raw;
  } catch (_) {}
  // One-time migration: the original theme toggle wrote `.theme-mode`.
  try {
    final legacy = File('$dataDir/.theme-mode').readAsStringSync().trim();
    if (legacy.isNotEmpty) return {'theme_mode': legacy};
  } catch (_) {}
  return const {};
}

void savePrefs(String dataDir, Map<String, dynamic> prefs) =>
    File('$dataDir/.prefs.json').writeAsStringSync(jsonEncode(prefs));

/// Theme preference — 'system' | 'light' | 'dark' in the prefs map.
String themeMode(String dataDir) =>
    '${loadPrefs(dataDir)['theme_mode'] ?? 'system'}';

void saveThemeMode(String dataDir, String mode) {
  final p = Map<String, dynamic>.of(loadPrefs(dataDir));
  p['theme_mode'] = mode;
  savePrefs(dataDir, p);
}

/// Opens the data dir in the OS file manager (best effort).
void revealDataDir(String dataDir) {
  try {
    if (Platform.isWindows) {
      Process.start('explorer', [dataDir]);
    } else if (Platform.isMacOS) {
      Process.start('open', [dataDir]);
    } else {
      Process.start('xdg-open', [dataDir]);
    }
  } catch (_) {}
}

/// Shown when the bridge fails to start.
const platformInitHint = '(build the core: cargo build -p pai-ffi)';
