pluginManagement {
    // flutter.sdk is read from local.properties, which `flutter build` writes.
    // When it is missing (e.g. IDE Gradle sync before the first build or after
    // `flutter clean`), fall back to FLUTTER_ROOT or a flutter exec on PATH.
    val flutterSdkPath =
        run {
            val properties = java.util.Properties()
            val localPropertiesFile = file("local.properties")
            if (localPropertiesFile.exists()) {
                localPropertiesFile.inputStream().use { properties.load(it) }
            }
            properties.getProperty("flutter.sdk")
                ?: System.getenv("FLUTTER_ROOT")
                ?: (System.getenv("PATH") ?: "")
                    .split(java.io.File.pathSeparator)
                    .asSequence()
                    .flatMap { dir ->
                        sequenceOf("flutter", "flutter.bat", "flutter.exe")
                            .map { java.io.File(dir, it) }
                    }
                    .firstOrNull { it.isFile }
                    ?.canonicalFile?.parentFile?.parent
                ?: sequenceOf(
                        "${System.getProperty("user.home")}/src/flutter",
                        "${System.getProperty("user.home")}/development/flutter",
                        "${System.getProperty("user.home")}/flutter",
                        "C:/src/flutter",
                    )
                    .firstOrNull { java.io.File(it, "bin").isDirectory }
                ?: error(
                    "flutter.sdk not set in local.properties, FLUTTER_ROOT unset, " +
                        "and no Flutter SDK found on PATH or in standard install dirs"
                )
        }
    // Exposed for dev.flutter.flutter-plugin-loader (skips its own
    // local.properties read when this extra is present) and for the project
    // property below.
    extensions.extraProperties["flutterSdkPath"] = flutterSdkPath
    gradle.extensions.extraProperties["flutterSdkPath"] = flutterSdkPath

    includeBuild("$flutterSdkPath/packages/flutter_tools/gradle")

    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

plugins {
    id("dev.flutter.flutter-plugin-loader") version "1.0.0"
    id("com.android.application") version "9.1.0" apply false
    // Not applied — AGP built-in Kotlin uses this classpath KGP instead of its
    // bundled 2.2.10, which is below Flutter's minimum supported Kotlin (2.2.20).
    id("org.jetbrains.kotlin.android") version "2.3.21" apply false
}

// Expose flutter.sdk as a project property so dev.flutter.flutter-gradle-plugin
// resolves it via findProperty without reading local.properties.
gradle.projectsLoaded {
    gradle.rootProject {
        extensions.extraProperties["flutter.sdk"] =
            gradle.extensions.extraProperties["flutterSdkPath"]
    }
}

include(":app")
