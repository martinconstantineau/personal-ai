# Design tokens — using "Meridian" in UI code

Everything visual lives in `apps/desktop/lib/theme.dart`. This page is
the working reference; the rationale is in
[ADR 0022](adr/0022-design-tokens-meridian.md).

## The golden rules

1. **No literals.** No `Color(0x…)`, `fontSize:`, `EdgeInsets.all(n)`,
   `BorderRadius.circular(n)`, or `Duration(milliseconds:)` in screen
   code — there is a token for each.
2. **Theme-varying → `context.brand` / `colorScheme`.** `AppColors`
   holds *fixed* values; if a color should differ light↔dark, it goes
   through `context.brand` or `Theme.of(context).colorScheme`.
3. **Component themes do the styling.** Cards, inputs, dialogs,
   buttons, chips, snackbars, segmented buttons, rail/bar — declare
   the widget, don't decorate it.

## Palette

| Token | Hex | Use |
|---|---|---|
| `ink` | `#0A111F` | dark canvas |
| `navy` / `navyHigh` / `navyEdge` | `#152239` / `#1D2F4E` / `#2C4266` | dark surfaces, overlay, hairline |
| `ivory` / `ivoryDim` | `#F4F0E6` / `#BBB5A6` | dark-theme text / secondary text |
| `steel` | `#8CA3C4` | info on dark |
| `paper` / `paperHigh` / `linen` | `#F6F3EC` / `#FCFAF4` / `#EDE8DB` | light canvas / card / overlay |
| `linenEdge` | `#DCD4C1` | light hairline |
| `umber` | `#6B6557` | light secondary text |
| `teal` / `tealBright` / `tealDeep` | `#0E7C74` / `#54C6B9` / `#0A5B56` | accent (light / dark / pressed) |
| `brass` / `brassBright` | `#AD8A45` / `#CBA75E` | gold — attention & warnings only |
| `moss` / `mossBright` | `#2E7D5B` / `#5BC493` | success |
| `oxblood` / `oxbloodBright` | `#AF4A41` / `#D9756B` | error |
| `cobalt` | `#3A66A8` | info on light |

Prefer `colorScheme.*` and `brand.*` over these in widgets — the table
is for the theme file itself and docs like this one.

## `context.brand` (theme-varying tokens)

`surfaceCard` · `surfaceRaised` · `surfaceOverlay` · `hairline` ·
`textMuted` · `success` · `warning` · `info` · `gold`

```dart
final brand = context.brand;
Container(color: brand.surfaceCard,
    decoration: BoxDecoration(border: Border.all(color: brand.hairline)));
```

## Spacing — strict 4-pt grid

`x2`(2) `xs`(4) `sm`(8) `md`(12) `lg`(16) `xl`(20) `xxl`(24) `x3`(32) `x4`(40) `x5`(48)

Named insets: `AppSpacing.page` (screen gutter), `AppSpacing.card`
(card padding).

```dart
padding: const EdgeInsets.all(AppSpacing.md)
```

## Radii & motion

`AppRadii.sm/md/lg/xl` doubles; `rSm`/`rMd`/`rLg`/`rXl`/`rPill` as
`BorderRadius`. `AppMotion.fast` (120 ms) / `AppMotion.med` (220 ms).

## Type

The `TextTheme` is token-sized — use `tt.titleSmall`, `tt.bodySmall`,
etc. For ids, JSON, paths, URLs — the mono voice:

```dart
Text(path, style: AppText.mono(context, size: 12))
```

## Accents

`AppAccent.{teal, brass, cobalt}` — persisted in prefs (`accent` key),
applied in `PaiApp` via `AppTheme.light(accent:)` / `.dark(accent:)`.
An accent only swaps the primary family; surfaces and semantics are
fixed. Adding a fourth accent = one entry in `_accent()` in
`theme.dart` plus a `ButtonSegment` in Settings → Appearance.

## Shared primitives (main.dart)

- `_EmptyState` — tinted icon disc + title + hint; use for every
  empty list.
- `_TagChip(label, color:, tooltip:)` — metadata chips on rows.
- `_InlineError` / `_errorView` — error strips and full retry blocks.
- `_listSkeleton` — loading placeholders.

If you find yourself styling the same thing a second time, lift it
here instead.
