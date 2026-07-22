import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    alias(libs.plugins.android.library)
    alias(libs.plugins.kotlin.android)
    id("maven-publish")
}

repositories {
    google()
    mavenCentral()
}

dependencies {
    implementation(libs.kotlin.stdlib)
    implementation(libs.androidx.annotation)
    compileOnly(libs.jna) // element x provides JNA

    // Required by the bundled `blew` BLE manager classes (org.jakebot.blew.*),
    // which back the iroh-over-BLE federation transport. Versions match blew's
    // own android module (blew-0.2.3/android/build.gradle.kts).
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.7.3")
    implementation("androidx.core:core-ktx:1.9.0")
}

android {
    namespace = "io.element.neutrino"
    compileSdk = 36

    defaultConfig {
        minSdk = 21
        // Keep the JNI-only entry points (org.jakebot.blew.*, io.element.neutrino.*)
        // in minified consuming apps — see consumer-rules.pro.
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlin {
        compilerOptions {
            jvmTarget = JvmTarget.JVM_17
        }
    }

    publishing {
        singleVariant("release")
    }
}

base {
    archivesName.set("neutrino")
}


publishing {
    publications {
        create<MavenPublication>("release") {
            groupId = "io.element.neutrino"
            artifactId = "bindings"
            version = (findProperty("neutrinoVersion") as String?) ?: "0.1.0-SNAPSHOT"

            afterEvaluate {
                from(components["release"])
            }

            // The upstream neutrino commit this .aar was compiled against,
            // passed by build-aar.sh from Cargo.lock. Baked into the POM so a
            // published artifact carries its own source provenance.
            val neutrinoCommit = (findProperty("neutrinoCommit") as String?) ?: "unknown"

            pom {
                name.set("Neutrino")
                description.set("Lightweight, embeddable homeserver written in Rust (neutrino @ $neutrinoCommit)")

                // Machine-readable copy of the same commit for tooling.
                properties.set(mapOf("neutrino.commit" to neutrinoCommit))

                scm {
                    url.set("https://github.com/element-hq/neutrino")
                    connection.set("scm:git:https://github.com/element-hq/neutrino.git")
                    tag.set(neutrinoCommit)
                }

                licenses {
                    license {
                        // AGPL only — this .aar bundles the AGPL-3.0-or-later
                        // blew/iroh-ble-transport stack, so unlike the main
                        // repo's LAN .aar there is no commercial option.
                        name.set("AGPL-3.0-only")
                        url.set("https://www.gnu.org/licenses/agpl-3.0.txt")
                    }
                }
            }
        }
    }

    repositories {
        maven {
            name = "GitHubPackages"
            url = uri("https://maven.pkg.github.com/element-hq/neutrino-iroh")
            credentials {
                username = System.getenv("GITHUB_ACTOR")
                password = System.getenv("GITHUB_TOKEN")
            }
        }
    }
}
