import 'dart:html' as html;

/// The runtime lives server-side behind `pai serve --bridge`; the web
/// client has no data dir of its own.
String appDataDir() => '';
String appProvider() => 'auto';

/// First-run marker rides in localStorage on web.
bool onboardingSeen(String _) =>
    html.window.localStorage['pai.onboarded'] == 'seen';
void markOnboardingSeen(String _) {
  html.window.localStorage['pai.onboarded'] = 'seen';
}

/// Shown when the bridge is unreachable — the runtime is server-side.
const platformInitHint =
    '(start the gateway: pai serve --bridge — then reload)';
