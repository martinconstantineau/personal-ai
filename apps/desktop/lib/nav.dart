/// Navigation model — the rail/bar destinations, slash-command table,
/// and keyboard shortcuts, kept as flat const lists so the icon font
/// tree-shaker tracks every glyph (const records hid them once) and so
/// tests can assert the wiring stays consistent.
library;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

/// The surfaces, in rail order — shared by the wide rail, the compact
/// icon-rail, and the narrow bottom bar. `destIcons`/`destLabels` are
/// parallel flat lists deliberately: IconData inside const records was
/// dropped by font tree-shaking, so they must stay direct elements.
const destIcons = <IconData>[
  Icons.chat_bubble_outline,
  Icons.apps_outlined,
  Icons.devices_outlined,
  Icons.notifications_outlined,
  Icons.psychology_outlined,
  Icons.description_outlined,
  Icons.mail_outline,
  Icons.receipt_long_outlined,
  Icons.policy_outlined,
  Icons.music_note_outlined,
  Icons.merge_type_outlined,
  Icons.settings_outlined,
];

const destLabels = <String>[
  'Chat',
  'Apps',
  'Devices',
  'Alerts',
  'Memories',
  'Documents',
  'Email',
  'Activity',
  'Permissions',
  'Media',
  'GitLab',
  'Settings',
];

/// Slash commands: verb, argument hint, description — `/help` lists
/// them. Navigation verbs must appear in [navCmds] too.
const cmds = <(String, String, String)>[
  ('new', '', 'Start a new chat'),
  ('rename', '<title>', 'Rename this chat'),
  ('model', '<slug>', 'Serve a model pack for chat'),
  ('sync', '', 'Sync with paired devices now'),
  ('export', '', 'Copy this transcript to the clipboard'),
  ('apps', '', 'Open Apps'),
  ('devices', '', 'Open Devices'),
  ('alerts', '', 'Open Alerts'),
  ('memories', '', 'Open Memories'),
  ('docs', '', 'Open Documents'),
  ('email', '', 'Open Email'),
  ('activity', '', 'Open Activity'),
  ('permissions', '', 'Open Permissions'),
  ('media', '', 'Open Media'),
  ('gitlab', '', 'Open GitLab'),
  ('settings', '', 'Open Settings'),
  ('help', '', 'List these commands'),
];

/// verb → rail destination index for the navigation commands.
const navCmds = {
  'apps': 1,
  'devices': 2,
  'alerts': 3,
  'memories': 4,
  'docs': 5,
  'email': 6,
  'activity': 7,
  'permissions': 8,
  'media': 9,
  'gitlab': 10,
  'settings': 11,
};

/// Ctrl+1..9,0 jump straight to the first ten rail destinations.
const railKeys = [
  LogicalKeyboardKey.digit1,
  LogicalKeyboardKey.digit2,
  LogicalKeyboardKey.digit3,
  LogicalKeyboardKey.digit4,
  LogicalKeyboardKey.digit5,
  LogicalKeyboardKey.digit6,
  LogicalKeyboardKey.digit7,
  LogicalKeyboardKey.digit8,
  LogicalKeyboardKey.digit9,
  LogicalKeyboardKey.digit0,
];

/// Extra shortcuts for destinations beyond the digit keys:
/// Ctrl+G GitLab, Ctrl+, Settings.
const extraNavKeys = <SingleActivator, int>{
  SingleActivator(LogicalKeyboardKey.keyG, control: true): 10,
  SingleActivator(LogicalKeyboardKey.comma, control: true): 11,
};
