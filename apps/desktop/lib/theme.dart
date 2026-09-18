/// Design tokens + theme assembly.
///
/// Palette "Meridian" — a 5-color brand system built to age well:
///   ink (near-black navy) · navy · ivory · teal · brass.
/// Dark theme sits on ink/navy with ivory text and bright teal;
/// light theme sits on ivory with navy text and deep teal.
/// Brass is reserved for premium/attention moments and warnings.
library;

import 'package:flutter/material.dart';

// ── Color tokens ────────────────────────────────────────────────

abstract final class AppColors {
  // Brand anchors
  static const ink = Color(0xFF0A111F); // near-black navy — dark canvas
  static const navy = Color(0xFF152239); // deep navy — dark raised surface
  static const navyHigh = Color(0xFF1D2F4E); // navy highlight / overlay
  static const navyEdge = Color(0xFF2C4266); // hairline on dark
  static const steel = Color(0xFF8CA3C4); // muted blue-grey — info on dark
  static const ivory = Color(0xFFF4F0E6); // warm ivory — text on dark
  static const ivoryDim = Color(0xFFBBB5A6); // secondary text on dark
  static const paper = Color(0xFFF6F3EC); // light canvas
  static const paperHigh = Color(0xFFFCFAF4); // light card
  static const linen = Color(0xFFEDE8DB); // light overlay / sunken
  static const linenEdge = Color(0xFFDCD4C1); // hairline on light
  static const umber = Color(0xFF6B6557); // secondary text on light
  static const teal = Color(0xFF0E7C74); // primary accent (light)
  static const tealDeep = Color(0xFF0A5B56); // pressed / strong accent
  static const tealBright = Color(0xFF54C6B9); // primary accent (dark)
  static const brass = Color(0xFFAD8A45); // restrained gold (light)
  static const brassBright = Color(0xFFCBA75E); // gold on dark
  static const oxblood = Color(0xFFAF4A41); // error (light)
  static const oxbloodBright = Color(0xFFD9756B); // error (dark)
  static const moss = Color(0xFF2E7D5B); // success (light)
  static const mossBright = Color(0xFF5BC493); // success (dark)
  static const cobalt = Color(0xFF3A66A8); // info (light)
}

// ── Spacing tokens (4-pt grid) ──────────────────────────────────

abstract final class AppSpacing {
  static const x2 = 2.0;
  static const xs = 4.0;
  static const sm = 8.0;
  static const md = 12.0;
  static const lg = 16.0;
  static const xl = 20.0;
  static const xxl = 24.0;
  static const x3 = 32.0;
  static const x4 = 40.0;
  static const x5 = 48.0;

  /// Standard page gutter.
  static const page = EdgeInsets.symmetric(horizontal: xxl, vertical: lg);

  /// Standard card padding.
  static const card = EdgeInsets.all(lg);
}

// ── Radius tokens ───────────────────────────────────────────────

abstract final class AppRadii {
  static const sm = 8.0;
  static const md = 12.0;
  static const lg = 16.0;
  static const xl = 24.0;

  static final rSm = BorderRadius.circular(sm);
  static final rMd = BorderRadius.circular(md);
  static final rLg = BorderRadius.circular(lg);
  static final rXl = BorderRadius.circular(xl);
  static const rPill = BorderRadius.all(Radius.circular(999));
}

// ── Motion tokens ───────────────────────────────────────────────

abstract final class AppMotion {
  static const fast = Duration(milliseconds: 120);
  static const med = Duration(milliseconds: 220);
}

// ── Semantic/surface tokens (theme-varying) ─────────────────────

/// Tokens that change value between light and dark — surface ramp,
/// hairlines, semantic hues, muted text. Reach via `context.brand`.
class BrandColors extends ThemeExtension<BrandColors> {
  const BrandColors({
    required this.surfaceCard,
    required this.surfaceRaised,
    required this.surfaceOverlay,
    required this.hairline,
    required this.textMuted,
    required this.success,
    required this.warning,
    required this.info,
    required this.gold,
  });

  /// Card / filled-input surface.
  final Color surfaceCard;

  /// Dialogs, menus, sheets.
  final Color surfaceRaised;

  /// Floating scrims, snackbars, tooltips, hover fills.
  final Color surfaceOverlay;

  /// 1-px separators and card borders.
  final Color hairline;

  /// Secondary text — captions, subtitles, hints.
  final Color textMuted;

  final Color success;
  final Color warning;
  final Color info;
  final Color gold;

  @override
  BrandColors copyWith({
    Color? surfaceCard,
    Color? surfaceRaised,
    Color? surfaceOverlay,
    Color? hairline,
    Color? textMuted,
    Color? success,
    Color? warning,
    Color? info,
    Color? gold,
  }) =>
      BrandColors(
        surfaceCard: surfaceCard ?? this.surfaceCard,
        surfaceRaised: surfaceRaised ?? this.surfaceRaised,
        surfaceOverlay: surfaceOverlay ?? this.surfaceOverlay,
        hairline: hairline ?? this.hairline,
        textMuted: textMuted ?? this.textMuted,
        success: success ?? this.success,
        warning: warning ?? this.warning,
        info: info ?? this.info,
        gold: gold ?? this.gold,
      );

  @override
  BrandColors lerp(ThemeExtension<BrandColors>? other, double t) {
    if (other is! BrandColors) return this;
    return BrandColors(
      surfaceCard: Color.lerp(surfaceCard, other.surfaceCard, t)!,
      surfaceRaised: Color.lerp(surfaceRaised, other.surfaceRaised, t)!,
      surfaceOverlay: Color.lerp(surfaceOverlay, other.surfaceOverlay, t)!,
      hairline: Color.lerp(hairline, other.hairline, t)!,
      textMuted: Color.lerp(textMuted, other.textMuted, t)!,
      success: Color.lerp(success, other.success, t)!,
      warning: Color.lerp(warning, other.warning, t)!,
      info: Color.lerp(info, other.info, t)!,
      gold: Color.lerp(gold, other.gold, t)!,
    );
  }
}

extension BrandX on BuildContext {
  BrandColors get brand => Theme.of(this).extension<BrandColors>()!;
}

/// Text helpers beyond the TextTheme — the mono voice for ids, JSON,
/// and code snippets. Uses the platform mono face.
abstract final class AppText {
  static TextStyle mono(BuildContext context,
      {double size = 12.5, Color? color}) {
    final theme = Theme.of(context);
    return (theme.textTheme.bodySmall ?? const TextStyle()).copyWith(
      fontFamily: 'monospace',
      fontSize: size,
      color: color ?? theme.colorScheme.onSurface,
      height: 1.45,
    );
  }
}

// ── Theme assembly ──────────────────────────────────────────────

abstract final class AppTheme {
  static ThemeData dark() => _base(_darkScheme(), _darkBrand());
  static ThemeData light() => _base(_lightScheme(), _lightBrand());

  static ColorScheme _darkScheme() => const ColorScheme(
        brightness: Brightness.dark,
        primary: AppColors.tealBright,
        onPrimary: Color(0xFF052B28),
        primaryContainer: Color(0xFF124A44),
        onPrimaryContainer: Color(0xFFB9EFE7),
        secondary: AppColors.steel,
        onSecondary: AppColors.ink,
        secondaryContainer: AppColors.navyHigh,
        onSecondaryContainer: AppColors.ivory,
        tertiary: AppColors.brassBright,
        onTertiary: AppColors.ink,
        error: AppColors.oxbloodBright,
        onError: AppColors.ink,
        errorContainer: Color(0xFF5A2622),
        onErrorContainer: Color(0xFFF5C9C4),
        surface: AppColors.ink,
        onSurface: AppColors.ivory,
        onSurfaceVariant: AppColors.ivoryDim,
        surfaceContainerLowest: Color(0xFF070D18),
        surfaceContainerLow: Color(0xFF0D1626),
        surfaceContainer: Color(0xFF101A2D),
        surfaceContainerHigh: AppColors.navy,
        surfaceContainerHighest: AppColors.navyHigh,
        outline: AppColors.navyEdge,
        outlineVariant: Color(0xFF22345A),
        inverseSurface: AppColors.ivory,
        onInverseSurface: AppColors.ink,
        inversePrimary: AppColors.teal,
        scrim: Color(0xB3000000),
        shadow: Color(0xFF000000),
        surfaceTint: AppColors.tealBright,
      );

  static BrandColors _darkBrand() => const BrandColors(
        surfaceCard: Color(0xFF101A2D),
        surfaceRaised: AppColors.navy,
        surfaceOverlay: AppColors.navyHigh,
        hairline: Color(0xFF233759),
        textMuted: AppColors.ivoryDim,
        success: AppColors.mossBright,
        warning: AppColors.brassBright,
        info: AppColors.steel,
        gold: AppColors.brassBright,
      );

  static ColorScheme _lightScheme() => const ColorScheme(
        brightness: Brightness.light,
        primary: AppColors.teal,
        onPrimary: Color(0xFFFDFBF6),
        primaryContainer: Color(0xFFC9EAE3),
        onPrimaryContainer: Color(0xFF073F3B),
        secondary: AppColors.navy,
        onSecondary: AppColors.ivory,
        secondaryContainer: Color(0xFFD8DEE9),
        onSecondaryContainer: AppColors.navy,
        tertiary: AppColors.brass,
        onTertiary: Color(0xFFFDFBF6),
        error: AppColors.oxblood,
        onError: Color(0xFFFDFBF6),
        errorContainer: Color(0xFFF2D5D1),
        onErrorContainer: Color(0xFF571F1A),
        surface: AppColors.paper,
        onSurface: AppColors.ink,
        onSurfaceVariant: AppColors.umber,
        surfaceContainerLowest: Color(0xFFEFEAE0),
        surfaceContainerLow: Color(0xFFF1EDE3),
        surfaceContainer: AppColors.paperHigh,
        surfaceContainerHigh: Color(0xFFF1EDE3),
        surfaceContainerHighest: AppColors.linen,
        outline: AppColors.linenEdge,
        outlineVariant: Color(0xFFE7E0CF),
        inverseSurface: AppColors.navy,
        onInverseSurface: AppColors.ivory,
        inversePrimary: AppColors.tealBright,
        scrim: Color(0x73000000),
        shadow: Color(0xFF000000),
        surfaceTint: AppColors.teal,
      );

  static BrandColors _lightBrand() => const BrandColors(
        surfaceCard: AppColors.paperHigh,
        surfaceRaised: Color(0xFFFDFBF6),
        surfaceOverlay: AppColors.linen,
        hairline: AppColors.linenEdge,
        textMuted: AppColors.umber,
        success: AppColors.moss,
        warning: AppColors.brass,
        info: AppColors.cobalt,
        gold: AppColors.brass,
      );

  /// Type scale — platform system face, token-sized. Slightly dense
  /// for desktop; muted variants come from colorScheme.onSurfaceVariant.
  static TextTheme _textTheme(ColorScheme cs) {
    final body = cs.onSurface;
    return TextTheme(
      displaySmall:
          TextStyle(fontSize: 28, height: 34 / 28, fontWeight: FontWeight.w600, letterSpacing: -0.4, color: body),
      headlineSmall:
          TextStyle(fontSize: 22, height: 28 / 22, fontWeight: FontWeight.w600, letterSpacing: -0.2, color: body),
      titleLarge:
          TextStyle(fontSize: 17, height: 24 / 17, fontWeight: FontWeight.w600, color: body),
      titleMedium:
          TextStyle(fontSize: 15, height: 22 / 15, fontWeight: FontWeight.w600, color: body),
      titleSmall:
          TextStyle(fontSize: 13.5, height: 18 / 13.5, fontWeight: FontWeight.w600, color: body),
      bodyLarge:
          TextStyle(fontSize: 14.5, height: 22 / 14.5, fontWeight: FontWeight.w400, color: body),
      bodyMedium:
          TextStyle(fontSize: 13.5, height: 20 / 13.5, fontWeight: FontWeight.w400, color: body),
      bodySmall:
          TextStyle(fontSize: 12.5, height: 17 / 12.5, fontWeight: FontWeight.w400, color: cs.onSurfaceVariant),
      labelLarge:
          TextStyle(fontSize: 13.5, height: 18 / 13.5, fontWeight: FontWeight.w600, letterSpacing: 0.1, color: body),
      labelMedium:
          TextStyle(fontSize: 12, height: 16 / 12, fontWeight: FontWeight.w600, letterSpacing: 0.2, color: body),
      labelSmall:
          TextStyle(fontSize: 11, height: 14 / 11, fontWeight: FontWeight.w600, letterSpacing: 0.7, color: cs.onSurfaceVariant),
    );
  }

  static ThemeData _base(ColorScheme cs, BrandColors brand) {
    final tt = _textTheme(cs);
    final dark = cs.brightness == Brightness.dark;
    final hairline =
        OutlineInputBorder(borderRadius: AppRadii.rMd, borderSide: BorderSide(color: brand.hairline));
    return ThemeData(
      useMaterial3: true,
      colorScheme: cs,
      textTheme: tt,
      primaryTextTheme: tt,
      extensions: [brand],
      scaffoldBackgroundColor: cs.surface,
      splashFactory: InkSparkle.splashFactory,
      highlightColor: cs.primary.withValues(alpha: 0.06),
      hoverColor: cs.primary.withValues(alpha: 0.05),
      visualDensity: VisualDensity.standard,
      textSelectionTheme: TextSelectionThemeData(
        cursorColor: cs.primary,
        selectionColor: cs.primary.withValues(alpha: 0.28),
        selectionHandleColor: cs.primary,
      ),
      dividerTheme: DividerThemeData(color: brand.hairline, thickness: 1, space: 1),
      iconTheme: IconThemeData(size: 20, color: cs.onSurfaceVariant),
      appBarTheme: AppBarTheme(
        backgroundColor: cs.surface,
        foregroundColor: cs.onSurface,
        elevation: 0,
        scrolledUnderElevation: 0,
        surfaceTintColor: Colors.transparent,
        centerTitle: false,
        titleTextStyle: tt.titleLarge,
        iconTheme: IconThemeData(size: 20, color: cs.onSurfaceVariant),
      ),
      cardTheme: CardThemeData(
        color: brand.surfaceCard,
        elevation: 0,
        margin: EdgeInsets.zero,
        clipBehavior: Clip.antiAlias,
        shape: RoundedRectangleBorder(
            borderRadius: AppRadii.rMd, side: BorderSide(color: brand.hairline)),
      ),
      dialogTheme: DialogThemeData(
        backgroundColor: brand.surfaceRaised,
        surfaceTintColor: Colors.transparent,
        elevation: dark ? 0 : 4,
        shape: RoundedRectangleBorder(
            borderRadius: AppRadii.rLg,
            side: BorderSide(color: brand.hairline)),
        titleTextStyle: tt.titleLarge,
        contentTextStyle: tt.bodyMedium,
        insetPadding: const EdgeInsets.all(AppSpacing.x3),
      ),
      snackBarTheme: SnackBarThemeData(
        behavior: SnackBarBehavior.floating,
        backgroundColor: dark ? brand.surfaceOverlay : AppColors.navy,
        contentTextStyle:
            tt.bodyMedium?.copyWith(color: dark ? AppColors.ivory : AppColors.ivory),
        shape: RoundedRectangleBorder(borderRadius: AppRadii.rMd),
        insetPadding: const EdgeInsets.all(AppSpacing.lg),
      ),
      tooltipTheme: TooltipThemeData(
        decoration: BoxDecoration(
            color: brand.surfaceOverlay,
            borderRadius: AppRadii.rSm,
            border: Border.all(color: brand.hairline)),
        textStyle: tt.bodySmall?.copyWith(color: cs.onSurface),
        padding: const EdgeInsets.symmetric(
            horizontal: AppSpacing.md, vertical: AppSpacing.sm),
        waitDuration: const Duration(milliseconds: 400),
      ),
      navigationRailTheme: NavigationRailThemeData(
        backgroundColor: dark ? const Color(0xFF0D1626) : brand.surfaceCard,
        indicatorColor: cs.primary.withValues(alpha: 0.18),
        indicatorShape:
            RoundedRectangleBorder(borderRadius: AppRadii.rMd),
        selectedIconTheme: IconThemeData(size: 20, color: cs.primary),
        unselectedIconTheme:
            IconThemeData(size: 20, color: cs.onSurfaceVariant),
        selectedLabelTextStyle:
            tt.labelMedium!.copyWith(color: cs.primary),
        unselectedLabelTextStyle:
            tt.labelMedium!.copyWith(color: cs.onSurfaceVariant),
        elevation: 0,
        useIndicator: true,
      ),
      navigationBarTheme: NavigationBarThemeData(
        backgroundColor: dark ? const Color(0xFF0D1626) : brand.surfaceCard,
        indicatorColor: cs.primary.withValues(alpha: 0.18),
        elevation: 0,
        height: 64,
        iconTheme: WidgetStateProperty.resolveWith((s) => IconThemeData(
            size: 20,
            color: s.contains(WidgetState.selected)
                ? cs.primary
                : cs.onSurfaceVariant)),
        labelTextStyle: WidgetStateProperty.resolveWith((s) =>
            tt.labelSmall!.copyWith(
                color: s.contains(WidgetState.selected)
                    ? cs.primary
                    : cs.onSurfaceVariant)),
      ),
      inputDecorationTheme: InputDecorationThemeData(
        filled: true,
        fillColor: brand.surfaceCard,
        isDense: true,
        contentPadding: const EdgeInsets.symmetric(
            horizontal: AppSpacing.md + 2, vertical: AppSpacing.md),
        hintStyle: tt.bodyMedium?.copyWith(color: brand.textMuted),
        labelStyle: tt.bodyMedium?.copyWith(color: brand.textMuted),
        floatingLabelStyle: tt.labelMedium?.copyWith(color: cs.primary),
        helperStyle: tt.bodySmall,
        border: hairline,
        enabledBorder: hairline,
        focusedBorder: OutlineInputBorder(
            borderRadius: AppRadii.rMd,
            borderSide: BorderSide(color: cs.primary, width: 1.5)),
        errorBorder: OutlineInputBorder(
            borderRadius: AppRadii.rMd,
            borderSide: BorderSide(color: cs.error)),
        focusedErrorBorder: OutlineInputBorder(
            borderRadius: AppRadii.rMd,
            borderSide: BorderSide(color: cs.error, width: 1.5)),
      ),
      filledButtonTheme: FilledButtonThemeData(
        style: FilledButton.styleFrom(
          backgroundColor: cs.primary,
          foregroundColor: cs.onPrimary,
          disabledBackgroundColor: brand.surfaceOverlay,
          disabledForegroundColor: brand.textMuted,
          padding: const EdgeInsets.symmetric(
              horizontal: AppSpacing.xl, vertical: AppSpacing.md + 2),
          shape: RoundedRectangleBorder(borderRadius: AppRadii.rMd),
          textStyle: tt.labelLarge,
          elevation: 0,
        ),
      ),
      elevatedButtonTheme: ElevatedButtonThemeData(
        style: ElevatedButton.styleFrom(
          backgroundColor: brand.surfaceOverlay,
          foregroundColor: cs.onSurface,
          padding: const EdgeInsets.symmetric(
              horizontal: AppSpacing.xl, vertical: AppSpacing.md + 2),
          shape: RoundedRectangleBorder(borderRadius: AppRadii.rMd),
          textStyle: tt.labelLarge,
          elevation: 0,
        ),
      ),
      outlinedButtonTheme: OutlinedButtonThemeData(
        style: OutlinedButton.styleFrom(
          foregroundColor: cs.primary,
          side: BorderSide(color: brand.hairline),
          padding: const EdgeInsets.symmetric(
              horizontal: AppSpacing.xl, vertical: AppSpacing.md + 2),
          shape: RoundedRectangleBorder(borderRadius: AppRadii.rMd),
          textStyle: tt.labelLarge,
        ),
      ),
      textButtonTheme: TextButtonThemeData(
        style: TextButton.styleFrom(
          foregroundColor: cs.primary,
          padding: const EdgeInsets.symmetric(
              horizontal: AppSpacing.md, vertical: AppSpacing.sm),
          shape: RoundedRectangleBorder(borderRadius: AppRadii.rSm),
          textStyle: tt.labelLarge,
        ),
      ),
      iconButtonTheme: IconButtonThemeData(
        style: IconButton.styleFrom(
            foregroundColor: cs.onSurfaceVariant, iconSize: 20),
      ),
      chipTheme: ChipThemeData(
        backgroundColor: brand.surfaceCard,
        selectedColor: cs.primary.withValues(alpha: 0.16),
        disabledColor: brand.surfaceCard,
        side: BorderSide(color: brand.hairline),
        shape: RoundedRectangleBorder(borderRadius: AppRadii.rSm),
        labelStyle: tt.labelMedium!,
        secondaryLabelStyle: tt.labelMedium!.copyWith(color: cs.primary),
        padding: const EdgeInsets.symmetric(
            horizontal: AppSpacing.sm, vertical: AppSpacing.xs),
        iconTheme: IconThemeData(size: 16, color: cs.onSurfaceVariant),
      ),
      listTileTheme: ListTileThemeData(
        dense: true,
        contentPadding: const EdgeInsets.symmetric(
            horizontal: AppSpacing.lg, vertical: AppSpacing.x2),
        iconColor: cs.onSurfaceVariant,
        textColor: cs.onSurface,
        titleTextStyle: tt.bodyLarge,
        subtitleTextStyle: tt.bodySmall,
        shape: RoundedRectangleBorder(borderRadius: AppRadii.rMd),
      ),
      progressIndicatorTheme: ProgressIndicatorThemeData(
        color: cs.primary,
        linearTrackColor: cs.primary.withValues(alpha: 0.16),
        circularTrackColor: cs.primary.withValues(alpha: 0.16),
      ),
      scrollbarTheme: ScrollbarThemeData(
        thumbVisibility: WidgetStateProperty.all(true),
        thickness: WidgetStateProperty.all(6),
        radius: const Radius.circular(999),
        thumbColor:
            WidgetStateProperty.all(brand.textMuted.withValues(alpha: 0.35)),
      ),
      popupMenuTheme: PopupMenuThemeData(
        color: brand.surfaceRaised,
        surfaceTintColor: Colors.transparent,
        elevation: dark ? 0 : 4,
        shape: RoundedRectangleBorder(
            borderRadius: AppRadii.rMd,
            side: BorderSide(color: brand.hairline)),
        textStyle: tt.bodyMedium,
      ),
      dropdownMenuTheme: DropdownMenuThemeData(
        menuStyle: MenuStyle(
          backgroundColor: WidgetStateProperty.all(brand.surfaceRaised),
          surfaceTintColor: WidgetStateProperty.all(Colors.transparent),
          shape: WidgetStateProperty.all(RoundedRectangleBorder(
              borderRadius: AppRadii.rMd,
              side: BorderSide(color: brand.hairline))),
        ),
        inputDecorationTheme: InputDecorationThemeData(
          filled: true,
          fillColor: brand.surfaceCard,
          isDense: true,
          border: hairline,
          enabledBorder: hairline,
        ),
      ),
      badgeTheme: BadgeThemeData(
        backgroundColor: brand.gold,
        textColor: AppColors.ink,
        textStyle: tt.labelSmall?.copyWith(color: AppColors.ink),
      ),
      switchTheme: SwitchThemeData(
        thumbColor: WidgetStateProperty.resolveWith((s) =>
            s.contains(WidgetState.selected) ? cs.primary : brand.textMuted),
        trackColor: WidgetStateProperty.resolveWith((s) =>
            s.contains(WidgetState.selected)
                ? cs.primary.withValues(alpha: 0.35)
                : brand.surfaceOverlay),
        trackOutlineColor: WidgetStateProperty.all(brand.hairline),
      ),
      checkboxTheme: CheckboxThemeData(
        fillColor: WidgetStateProperty.resolveWith((s) =>
            s.contains(WidgetState.selected)
                ? cs.primary
                : Colors.transparent),
        checkColor: WidgetStateProperty.all(cs.onPrimary),
        side: BorderSide(color: brand.hairline, width: 1.5),
        shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(4)),
      ),
      radioTheme: RadioThemeData(
        fillColor: WidgetStateProperty.resolveWith((s) =>
            s.contains(WidgetState.selected) ? cs.primary : brand.textMuted),
      ),
      sliderTheme: SliderThemeData(
        activeTrackColor: cs.primary,
        thumbColor: cs.primary,
        inactiveTrackColor: cs.primary.withValues(alpha: 0.2),
        overlayColor: cs.primary.withValues(alpha: 0.12),
      ),
      expansionTileTheme: ExpansionTileThemeData(
        iconColor: cs.onSurfaceVariant,
        collapsedIconColor: cs.onSurfaceVariant,
        textColor: cs.onSurface,
        collapsedTextColor: cs.onSurface,
        shape: const RoundedRectangleBorder(),
        collapsedShape: const RoundedRectangleBorder(),
      ),
      segmentedButtonTheme: SegmentedButtonThemeData(
        style: ButtonStyle(
          side: WidgetStateProperty.all(
              BorderSide(color: brand.hairline)),
          foregroundColor: WidgetStateProperty.resolveWith((s) =>
              s.contains(WidgetState.selected)
                  ? cs.primary
                  : cs.onSurfaceVariant),
          backgroundColor: WidgetStateProperty.resolveWith((s) =>
              s.contains(WidgetState.selected)
                  ? cs.primary.withValues(alpha: 0.14)
                  : Colors.transparent),
          iconColor: WidgetStateProperty.resolveWith((s) =>
              s.contains(WidgetState.selected)
                  ? cs.primary
                  : cs.onSurfaceVariant),
          textStyle: WidgetStateProperty.all(tt.labelLarge),
          shape: WidgetStateProperty.all(
              RoundedRectangleBorder(borderRadius: AppRadii.rMd)),
          visualDensity: VisualDensity.compact,
        ),
      ),
      tabBarTheme: TabBarThemeData(
        labelColor: cs.primary,
        unselectedLabelColor: cs.onSurfaceVariant,
        labelStyle: tt.labelLarge,
        unselectedLabelStyle: tt.labelLarge,
        indicatorColor: cs.primary,
        dividerColor: brand.hairline,
      ),
      bottomSheetTheme: BottomSheetThemeData(
        backgroundColor: brand.surfaceRaised,
        surfaceTintColor: Colors.transparent,
        shape: RoundedRectangleBorder(
            borderRadius: BorderRadius.vertical(
                top: Radius.circular(AppRadii.lg)),
            side: BorderSide(color: brand.hairline)),
      ),
    );
  }
}
