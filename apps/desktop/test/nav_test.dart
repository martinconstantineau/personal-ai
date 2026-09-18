import 'package:flutter_test/flutter_test.dart';
import 'package:pai_app/nav.dart';

/// Wiring invariants for the navigation model — the class of bug that
/// once blanked the content pane (IndexedStack children undersized
/// relative to the destinations list) is caught here before it ships.
void main() {
  test('icons and labels stay parallel', () {
    expect(destIcons.length, destLabels.length);
    expect(destLabels, isNotEmpty);
  });

  test('every destination is reachable by a nav command', () {
    // Chat (0) is home — the rest need a verb in navCmds, and that
    // verb needs a cmds entry so /help can list it.
    final covered = navCmds.values.toSet();
    for (var i = 1; i < destLabels.length; i++) {
      expect(covered, contains(i),
          reason: 'destination $i (${destLabels[i]}) has no nav command');
    }
    for (final verb in navCmds.keys) {
      expect(cmds.any((c) => c.$1 == verb), isTrue,
          reason: 'nav verb /$verb missing from cmds');
    }
  });

  test('nav command targets stay in range', () {
    for (final i in navCmds.values) {
      expect(i, inInclusiveRange(0, destLabels.length - 1));
    }
  });

  test('rail digit keys stay in range', () {
    expect(railKeys.length, lessThanOrEqualTo(destLabels.length));
  });

  test('extra nav keys target real destinations and unique slots', () {
    final targets = extraNavKeys.values.toList();
    expect(targets.toSet().length, targets.length,
        reason: 'two shortcuts point at the same destination');
    for (final i in targets) {
      expect(i, inInclusiveRange(0, destLabels.length - 1));
      expect(i, greaterThanOrEqualTo(railKeys.length),
          reason: 'extra key covers a destination digits already reach');
    }
  });
}
