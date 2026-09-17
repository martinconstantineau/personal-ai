/// Platform config the app shell needs before the bridge exists —
/// `dart:io` on native, `dart:html` localStorage on web.
library;

export 'platform_config_web.dart' if (dart.library.io) 'platform_config_io.dart';
