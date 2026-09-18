import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:pai_app/main.dart';

void main() {
  testWidgets('app builds and shows the chat shell', (tester) async {
    // The bridge spawns a real isolate — runAsync so the handshake can
    // actually complete (fake-async pumpAndSettle never sees it).
    await tester.runAsync(() async {
      await tester.pumpWidget(const PaiApp());
      // FFI init can take several seconds under a loaded host — poll
      // for the shell instead of sleeping a fixed duration.
      for (var i = 0; i < 60; i++) {
        await Future.delayed(const Duration(milliseconds: 500));
        await tester.pump();
        if (find.text('Personal AI').evaluate().isNotEmpty) break;
      }
      // First run opens the welcome tour — dismiss it to reach the shell.
      final getStarted = find.text('Get started');
      if (getStarted.evaluate().isNotEmpty) {
        await tester.tap(getStarted);
        await tester.pump();
      }
      expect(find.text('Personal AI'), findsOneWidget);
      expect(find.byType(TextField), findsOneWidget);
    });
  });
}
