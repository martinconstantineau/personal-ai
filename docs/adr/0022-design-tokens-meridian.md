# ADR 0022: Design tokens — the "Meridian" palette

- **Status**: Accepted
- **Date**: 2026-09-18

## Context

The desktop app grew from a bare `ColorScheme.fromSeed` plus ~75 ad-hoc
`fontSize:`/`EdgeInsets` literals scattered across ~5,800 lines of UI.
Two symptoms followed: surfaces disagreed with each other (five
hand-rolled empty states, three error strips), and every aesthetic
decision was un-answerable without a sweep through the file.

The app targets AI/HRTech/BusinessTech use — it needs to look like
infrastructure, not a toy. The brand direction chosen: timeless,
premium, restrained; one accent doing the talking, warm paper under
it, near-black navy after dark.

## Decision

A single token file, `apps/desktop/lib/theme.dart`, owns every visual
value. Palette "Meridian" — five brand anchors:

| Token | Hex | Role |
|---|---|---|
| `ink` | `#0A111F` | near-black navy — dark canvas |
| `navy` | `#152239` | dark raised surface (with `navyHigh`/`navyEdge`) |
| `ivory` | `#F4F0E6` | warm text on dark (with `ivoryDim`) |
| `teal` | `#0E7C74` / `#54C6B9` | primary accent — deep on light, bright on dark |
| `brass` | `#AD8A45` / `#CBA75E` | restrained gold — attention moments, warnings |

Light theme mirrors with `paper`/`paperHigh`/`linen` surfaces, `ink`
text, `umber` secondary text, `linenEdge` hairlines.

**Token layers** (usage guide: `docs/design-tokens.md`):

- `AppColors` — the fixed palette above plus semantic hues
  (`moss`/`oxblood`/`cobalt`, each with a `-Bright` dark variant).
- `AppSpacing` — strict 4-pt grid (`x2`…`x5`) with named `page`/`card`
  insets. Magic paddings are banned.
- `AppRadii`, `AppMotion`, `AppText.mono` — radius/duration constants
  and the monospace voice for ids, JSON, and URLs.
- `BrandColors` — a `ThemeExtension` for values that *change* between
  themes (surface ramp, hairlines, muted text, success/warning/
  info/gold). Reached via `context.brand`.
- `AppTheme.light()` / `.dark()` — hand-tuned `ColorScheme`s plus ~25
  component themes so rail, cards, inputs, dialogs, chips, snackbars,
  and segmented buttons never need local styling.

**Accents** (`AppAccent`): teal is the default; `brass` and `cobalt`
are alternates. An accent swaps only the primary family (primary,
onPrimary, containers, inverse, tint) — surfaces and semantics never
move, which keeps the brand stable while users personalize.

**Rules for contributors**:

- Never write a raw `Color(0x…)`, `fontSize:`, or `EdgeInsets.all(n)`
  in screen code — reach for the nearest token.
- Theme-varying colors go through `context.brand` or `colorScheme`,
  never `AppColors` directly (those are fixed values).
- Brass/gold is reserved — if everything is gold, nothing is.

## Consequences

- Theming is a one-file change; a new accent is ~12 lines.
- `test/theme_test.dart` enforces the isolation rule: accent variants
  must not move surfaces or semantics.
- Shared primitives (`_EmptyState`, `_TagChip`, `_InlineError`) replaced
  the copy-pasted variants — the same class of inconsistency can't
  regrow through copy-paste.
- `MediaQuery.textScaler` gives a persisted text-size pref for free;
  density stays on the 4-pt grid.
