import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:pai_app/platform_config_io.dart';

/// Native prefs store — `.prefs.json` in the data dir, with the
/// one-time `.theme-mode` migration.
void main() {
  late Directory dir;

  setUp(() => dir = Directory.systemTemp.createTempSync('pai-prefs-'));
  tearDown(() => dir.deleteSync(recursive: true));

  test('round-trips a prefs map', () {
    savePrefs(dir.path, {'theme_mode': 'dark', 'accent': 'brass'});
    final p = loadPrefs(dir.path);
    expect(p['theme_mode'], 'dark');
    expect(p['accent'], 'brass');
  });

  test('missing file yields an empty map', () {
    expect(loadPrefs(dir.path), isEmpty);
    expect(themeMode(dir.path), 'system');
  });

  test('legacy .theme-mode migrates into the prefs map', () {
    File('${dir.path}/.theme-mode').writeAsStringSync('dark\n');
    expect(loadPrefs(dir.path)['theme_mode'], 'dark');
    expect(themeMode(dir.path), 'dark');
  });

  test('saveThemeMode preserves the other keys', () {
    savePrefs(dir.path, {'accent': 'cobalt'});
    saveThemeMode(dir.path, 'light');
    final p = loadPrefs(dir.path);
    expect(p['theme_mode'], 'light');
    expect(p['accent'], 'cobalt');
  });

  test('corrupt prefs fall back to empty, not a crash', () {
    File('${dir.path}/.prefs.json').writeAsStringSync('{not json');
    expect(loadPrefs(dir.path), isEmpty);
  });
}
