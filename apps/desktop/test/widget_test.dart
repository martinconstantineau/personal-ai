import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:pai_app/main.dart';

void main() {
  testWidgets('app builds and shows the chat shell', (tester) async {
    // The bridge spawns a real isolate — runAsync so the handshake can
    // actually complete (fake-async pumpAndSettle never sees it).
    await tester.runAsync(() async {
      await tester.pumpWidget(const PaiApp());
      await Future.delayed(const Duration(seconds: 3));
      await tester.pump();
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
