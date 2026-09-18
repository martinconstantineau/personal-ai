import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:pai_app/theme.dart';

/// Visual regression for Meridian — a representative surface under
/// all six theme variants. Goldens are host-pinned to Windows: font
/// rasterization differs across platforms, so CI skips these (the
/// token-level invariants in theme_test.dart still run everywhere).
Widget _showcase(ThemeData theme) => MaterialApp(
      theme: theme,
      debugShowCheckedModeBanner: false,
      home: Builder(
        builder: (context) {
          final tt = Theme.of(context).textTheme;
          final brand = context.brand;
          return Scaffold(
            appBar: AppBar(title: const Text('Meridian'), actions: [
              IconButton(
                  onPressed: () {}, icon: const Icon(Icons.refresh)),
            ]),
            body: Padding(
              padding: const EdgeInsets.all(AppSpacing.lg),
              child: Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Card(
                      child: ListTile(
                        leading: const Icon(Icons.bolt_outlined),
                        title: const Text('Card surface'),
                        subtitle: Text('Hairline + ramp',
                            style: tt.bodySmall
                                ?.copyWith(color: brand.textMuted)),
                        trailing: const Icon(Icons.chevron_right),
                      ),
                    ),
                    const SizedBox(height: AppSpacing.md),
                    Wrap(spacing: AppSpacing.sm, children: [
                      FilledButton(
                          onPressed: () {},
                          child: const Text('Primary')),
                      FilledButton.tonal(
                          onPressed: () {},
                          child: const Text('Tonal')),
                      OutlinedButton(
                          onPressed: () {},
                          child: const Text('Outline')),
                      TextButton(
                          onPressed: () {},
                          child: const Text('Text')),
                    ]),
                    const SizedBox(height: AppSpacing.md),
                    Wrap(spacing: AppSpacing.sm, children: [
                      const Chip(label: Text('chip')),
                      ChoiceChip(
                          label: const Text('selected'),
                          selected: true,
                          onSelected: (_) {}),
                      Badge(
                          label: Text('3'),
                          child: Icon(Icons.notifications_outlined)),
                    ]),
                    const SizedBox(height: AppSpacing.md),
                    Text('Title medium', style: tt.titleMedium),
                    Text('Body text on the canvas', style: tt.bodyLarge),
                    Text('Muted helper text',
                        style:
                            tt.bodySmall?.copyWith(color: brand.textMuted)),
                    const SizedBox(height: AppSpacing.sm),
                    Text('mono 0x1F4A · {id: "abc"}',
                        style: AppText.mono(context, size: 12)),
                    const SizedBox(height: AppSpacing.md),
                    Row(children: [
                      Icon(Icons.check_circle_outline,
                          size: 16, color: brand.success),
                      const SizedBox(width: AppSpacing.xs),
                      Icon(Icons.warning_amber_outlined,
                          size: 16, color: brand.warning),
                      const SizedBox(width: AppSpacing.xs),
                      Icon(Icons.error_outline,
                          size: 16, color: Theme.of(context)
                              .colorScheme
                              .error),
                    ]),
                  ]),
            ),
          );
        },
      ),
    );

void main() {
  for (final brightness in ['light', 'dark']) {
    for (final accent in AppAccent.values) {
      testWidgets('showcase — $brightness/${accent.name}', (tester) async {
        tester.view.physicalSize = const Size(560, 560);
        tester.view.devicePixelRatio = 1.0;
        addTearDown(tester.view.reset);
        final theme = brightness == 'light'
            ? AppTheme.light(accent: accent)
            : AppTheme.dark(accent: accent);
        await tester.pumpWidget(_showcase(theme));
        await tester.pump();
        await expectLater(find.byType(MaterialApp),
            matchesGoldenFile('goldens/${brightness}_${accent.name}.png'));
      }, skip: !Platform.isWindows);
    }
  }
}
