import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:pai_app/main.dart';

void main() {
  testWidgets('app builds and shows the chat shell', (tester) async {
    await tester.pumpWidget(const PaiApp());
    expect(find.text('Personal AI'), findsOneWidget);
    expect(find.byType(TextField), findsOneWidget);
  });
}
