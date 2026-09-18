import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:pai_app/theme.dart';

/// Meridian tokens — accent variants swap only the primary family;
/// surfaces and semantics must stay on the navy/ivory structure.
void main() {
  test('default accent is teal, both brightnesses', () {
    expect(AppTheme.light().colorScheme.primary, AppColors.teal);
    expect(AppTheme.dark().colorScheme.primary, AppColors.tealBright);
    expect(AppTheme.light().brightness, Brightness.light);
    expect(AppTheme.dark().brightness, Brightness.dark);
  });

  test('accent variants swap the primary family', () {
    expect(AppTheme.light(accent: AppAccent.brass).colorScheme.primary,
        AppColors.brass);
    expect(AppTheme.light(accent: AppAccent.cobalt).colorScheme.primary,
        AppColors.cobalt);
    expect(AppTheme.dark(accent: AppAccent.brass).colorScheme.primary,
        AppColors.brassBright);
  });

  test('surfaces and semantics do not leak the accent', () {
    final teal = AppTheme.light();
    for (final accent in AppAccent.values) {
      final t = AppTheme.light(accent: accent);
      expect(t.colorScheme.surface, teal.colorScheme.surface,
          reason: '$accent changed the canvas');
      expect(t.colorScheme.error, teal.colorScheme.error,
          reason: '$accent changed the error color');
      expect(t.extension<BrandColors>()!.surfaceCard,
          teal.extension<BrandColors>()!.surfaceCard,
          reason: '$accent changed the card surface');
    }
  });

  test('brand extension resolves on both themes', () {
    expect(AppTheme.light().extension<BrandColors>(), isNotNull);
    expect(AppTheme.dark().extension<BrandColors>(), isNotNull);
  });
}
