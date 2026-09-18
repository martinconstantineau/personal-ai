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

/// Theme preference — 'system' | 'light' | 'dark' in the data dir.
String themeMode(String dataDir) {
  try {
    return File('$dataDir/.theme-mode').readAsStringSync().trim();
  } catch (_) {
    return 'system';
  }
}

void saveThemeMode(String dataDir, String mode) =>
    File('$dataDir/.theme-mode').writeAsStringSync(mode);

/// Shown when the bridge fails to start.
const platformInitHint = '(build the core: cargo build -p pai-ffi)';
