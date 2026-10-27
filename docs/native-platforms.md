# Native Plattformabnahme

Die Nachweise gehören zu TASK-0126, entwickelt auf `port/task-0126-rust`
und nach Nutzerfreigabe auf `main` übernommen.
Ein Paketbau, ein lokaler Prozesslauf, ein Dienstlauf und eine Hardwareprüfung
sind getrennte Ergebnisse. Die [Paketierungsanleitung](native-packaging.md)
beschreibt Installation, Migration und Rollback; der
[Windows-Messbericht](native-performance.md) enthält die Ressourcenvergleiche.

## Quellcheckout und Standardstart

Der Quellcheckout verwendet nun Rust als Standard: `npm run build` baut Vue,
`npm run build:native` das native Releasebinary; `npm start` und
`npm run cli -- …` verwenden `target/release/kvmhelm`. Die npm-Aufrufe laden
weiterhin eine lokale `.env`. Native Pakete werden unmittelbar gestartet und
benötigen diese Entwicklungswerkzeuge nicht.

Die Umstellung ist mit isolierten Startverzeichnissen und Daten geprüft:
`.env`-Bootstrap ohne Tokenausgabe, leere Geräteliste, Rust-Diagnose,
Flag-Vorrang und CLI-Fehlercodes. Unter Windows beenden echte Ctrl+C-Ereignisse
in einer eigenen versteckten Konsole Wrapper und Rust-Kindprozess mit Exitcode 0;
zwei Start-/Stopzyklen öffnen denselben Store erneut. Unter Linux wartet der
Wrapper bei SIGTERM auf das geordnete native Ende. Das ist ein Konsolen-/Prozess-
nachweis und keine zusätzliche SCM-/launchd-Abnahme.

Die gepinnte Originalreferenz liegt ausschließlich in `target/reference` mit
eigenem Lockfile und allen 38 Originaltests; Quellen werden gegen Git-Blobs und
kompilierte Ausgaben gegen SHA-256 geprüft. Eine gezielte Veränderung einer
kompilierten Referenzdatei wird vor Benutzung abgewiesen. Alle 38 Originaltests,
zehn Sprachvergleiche unter Windows/Linux sowie die vier erhaltenen aktuellen
Vue-Queue-Tests bestehen. Beim ersten umgestellten MCP-Referenzlauf trat ein
Starttimeout auf; der Aufruf verwendet nun ausdrücklich das Referenzverzeichnis
und besteht einschließlich zweier Wiederanläufe. Der aktuelle npm-Graph enthält
ausschließlich Build-/Testabhängigkeiten und 31 Lockeinträge weniger; die
verbliebenen Paketversionen sind unverändert.

## Umgebung und Prüfbefehle

Die Linux-Prüfung auf ENVY verwendet Ubuntu 24.04 in WSL2, x86_64,
Kernel `6.18.33.2-microsoft-standard-WSL2`, glibc 2.39, GCC/G++ 13.3.0,
CMake 3.28.3, Rust 1.99.1 und Node 24.18.0. Quellkopie, Cargo-/npm-Caches,
Browser und synthetischer Zustand liegen in einem eigenen Linux-Arbeitsverzeichnis.
Die vorhandenen Windows-Builds und privaten Anwendungsdaten werden nicht benutzt.
Rust und Node sind separat mit den gepinnten Hashes bereitgestellt. Eine fehlende
Chromium-Audiobibliothek wurde nur ins Testverzeichnis entpackt.
Es wurden keine Systempakete installiert oder vorhandenen Dienste umgestellt.

Auf einem eingerichteten Linux-Buildhost:

```sh
npm ci
npm run build
npm run prepare:reference # ausschließlich für Originalvergleiche/Migration
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features embedded-web -- -D warnings
cargo test --locked --features embedded-web
cargo test --locked --test legacy_compat -- --ignored
npm run package:native -- --target x86_64-unknown-linux-gnu --profile full
KVMHELM_TEST_PACKAGE_ARCHIVE=target/packages/kvmhelm-0.1.0-x86_64-unknown-linux-gnu-full.tar.gz node scripts/test-native-package.mjs
```

Die WSL-Testbuilds verwenden `CARGO_PROFILE_TEST_DEBUG=0`, um Platz für
Debugsymbole auf dem Host zu sparen; Debugassertionen bleiben aktiv.
Releasepakete verwenden die normale optimierte Release-Konfiguration.
Die eigentliche Anwendungs-/Pluginlaufzeit benötigt keine npm-Installation.

## Linux-ABI des geprüften Builds

Der optimierte x64-Daemon ist 23.360.488 Bytes groß. `ldd` nennt
`libgcc_s.so.1`, `libm.so.6`, `libc.so.6` und den GNU-Loader
`ld-linux-x86-64.so.2`; SQLite, TLS und Opus erfordern keine separaten
Shared Objects. `readelf` weist Symbolanforderungen bis **GLIBC_2.39** aus.
Dieses konkrete Ubuntu-24.04-Paket ist damit kein Nachweis für ältere
glibc-Systeme. Dafür auf einem passenden älteren Buildsystem bauen und erneut
prüfen; das GNU-Target allein verspricht keine Mindestdistribution.

Der mitgelieferte Node-Helfer benötigt außerdem `libstdc++.so.6`
(Symbolanforderung bis `GLIBCXX_3.4.21`) sowie die von `ldd` ausgewiesenen
glibc-Kompatibilitätsbibliotheken `libdl.so.2` und `libpthread.so.0`.
Diese Betriebssystembibliotheken sind nicht Teil des Full-Archivs.
FFmpeg und Node bleiben ausdrücklich eigene optionale Prozesse.

## Private POSIX-Dateien

Daten und Secrets brauchen ein Dateisystem, das private Unix-Modi tatsächlich
anwendet: Verzeichnisse `0700`, Dateien `0600`. Der native Pfad liest den
gesetzten Modus nach `chmod` zurück und bricht bei Abweichungen ab, bevor er
einen Masterkey veröffentlicht oder geschützten Zustand weiterverwendet.
Die vorhandenen Linkprüfungen bleiben erhalten.

Der auf ENVY geprüfte `/mnt/c`-Mount ohne POSIX-Metadaten meldet nach `chmod`
weiterhin `0777`. Für Linux-/WSL-Daten daher ein Linux-Dateisystem verwenden.
Ein erfolgreicher Windows-ACL-Nachweis lässt sich nicht auf Unix-Zugriffe über
einen solchen Mount übertragen.

Der zusätzliche negative Test wird gezielt auf einem eigenen Testverzeichnis
eines Dateisystems gestartet, das `chmod` annimmt, aber ignoriert:

```sh
KVMHELM_TEST_NON_POSIX_ROOT=/mnt/c/prj/KVMHelm/target/non-posix-probe \
  cargo test --locked --features embedded-web --test foundation \
  secret_storage_rejects_filesystems_that_ignore_private_permissions -- --ignored
```

Das Verzeichnis vorher selbst anlegen. Der Test kalibriert den ungeeigneten
Mount mit einer leeren Datei und erwartet anschließend die Ablehnung ohne
`master.key`. Ein normales POSIX-Dateisystem ist für diesen negativen Fall
ungeeignet. Die reguläre Foundationprüfung kontrolliert private Modi und
Symlink-Abweisung auf dem normalen Dateisystem.

## Temporärer systemd-Lauf

`scripts/native-systemd-interop.mjs` verlangt Linux, Root für die Teststeuerung,
die bestehende Identität `nobody` und das ausdrückliche Opt-in
`KVMHELM_TEST_SYSTEMD=1`. Der Daemon und seine Node-Helfer laufen als `nobody`.
Das Script erstellt genau eine zufällig benannte transiente
`kvmhelm-test-<UUID>.service` und eigene Paket-/Zustandskopien unter
`/run/kvmhelm-test-*`. Es legt keine dauerhafte Unitdatei an und aktiviert
keinen Autostart.

```sh
sudo env KVMHELM_TEST_SYSTEMD=1 \
  KVMHELM_TEST_HELPER_DIRECTORY=/absolute/extracted/full-package \
  /absolute/node scripts/native-systemd-interop.mjs
```

Die Dienstpolicies werden aus der tatsächlich paketierten `kvmhelm.service`
übernommen. Account, Arbeits-/Schreibpfade und Daten-/Secretumgebung zeigen auf
die Testkopie; das optionale Betreiber-EnvironmentFile wird nicht geladen.
`KillMode=mixed`, Stopfrist, Restartregel, UMask, `NoNewPrivileges`, `PrivateTmp`,
`ProtectSystem` und `ProtectHome` bleiben wirksam.
Die Testkopie liegt außerhalb von `/tmp` und `/var/tmp`, die `PrivateTmp`
im Dienst durch eigene Verzeichnisse ersetzt.
Der Test verwendet einen Simulator und ein eigenes Buffer-Plugin. Er prüft
UID/GID und Dateirechte, Readiness, vorhandenen PAT, Restart mit freigegebenen
Sessions, erhaltene Konfiguration, erneutes Starten nach Stop und das Ende
aller beobachteten Daemon-/Broker-/Pluginprozesse. `systemd-run --wait`
kontrolliert den tatsächlichen Exitstatus; `--collect` räumt die transiente
Unit auf. Die CI enthält denselben opt-in Lauf für ihren Linux-Runner.

## Plattform- und Paketnachweise

Am 26.10.2026 bestanden auf ENVY im zuerst geprüften Paketierungsstand die
folgenden Nachweise. Die anschließende Aufwecknacharbeit steht im letzten
Abschnitt dieser Seite.

| Umgebung                                | Ausgeführter Nachweis                                                                                                                                                                                                 | Getrennt offen                                                                         |
| --------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| Windows x64                             | Optimierter Full-Paketbau, alle Profile, 112 native Tests und zehn Originalvergleiche im Paketierungsstand; Migration, MCP, Medien, Plugins, Vue und 36 Messfenster                                                   | SCM-Dienstlauf, Container und physische Geräte                                         |
| Linux x64 / Ubuntu 24.04 WSL2           | Rustfmt, strenges Clippy; 111 reguläre native Tests und zehn tatsächliche Originalvergleiche sowohl als Root als auch als UID/GID 65534; zusätzlicher negativer Dateirechte-Test                                      | Andere Distributionen/Architekturen, Linux-Ressourcenmessung und physische Geräte      |
| Linux Releasepaket als UID/GID 65534    | Prüfsummen; TypeScript → Native → TypeScript mit vorhandenen PATs, AES-GCM-Secrets, Revisionen und Plugins; zwei Programmverzeichnisse, SIGTERM, Helferende, Rücklesen und Sicherung; zwei bytegleiche Archivmontagen | Andere glibc-Baselines                                                                 |
| Linux Release-Interop als UID/GID 65534 | Native MCP-/stdio-Wiederanläufe, JetKVM/Chromium/H.264/FFmpeg/Opus, Janus-/Vue-Audio einschließlich synthetischem Mikrofon und Plugin-PCM, Plugin-Lifecycle/IPC und vollständige Vue-Verwaltungs-/Touchfixture        | Physische Firmware-/Netzwerkbedingungen                                                |
| Linux systemd 255.4                     | Debug- und Releasebinary in transienter Unit mit UID/GID 65534, privaten Dateien und den beschriebenen Härtungspolicies; Start/Restart/Stop/Wiederöffnung, Sessionfreigabe und beobachtete Helferbereinigung          | Betreiberinstallation mit konkretem Account, EnvironmentFile und dauerhaftem Autostart |
| macOS x64/arm64                         | CI-Definitionen, Paketierungszweige und launchd-Vorlage vorhanden; Syntax der XML-Vorlage gelesen                                                                                                                     | Nativer Build/Lauf und launchd-Ausführung                                              |

Das vor dem abschließenden Vertragsabgleich getestete Linux-Releasebinary hat SHA-256
`c3cebdce38f94244712e2697bda6e012c1a0d14fa6904743d1b81e6bc107d121`.
Das Full-Paket enthält 481 Dateien, Notices für 250 Cargo-Pakete und zehn
Browsermodule. Die drei Profile wurden aus dem sauberen Commit `385682d`
erneut gebaut und geprüft; ihr Binary stimmt mit dem integrationserprobten
Binary überein. Das gilt ebenso für Windows mit dem im Messbericht genannten
Binaryhash. Der nachfolgende Vertragsabgleich ändert HEAD/OPTIONS und den
Offlinebefehl `demo`; seine aktuellen Nachweise stehen in der
[Reviewvorlage](native-review.md).
Paketbau allein beweist keine identischen Compilerergebnisse auf anderen Hosts.

Die normalen Medien-/Plugin-/Browserfixtures verwenden ihre bestehenden
Fehler- und Lebenszyklusprüfungen. Der neue systemd-Test korrigierte zunächst
eine unvollständige Präferenzanfrage in seiner Fixture; die strikte API hatte
sie korrekt abgewiesen. Der negative Dateisystemtest scheiterte vor der
Korrektur am veröffentlichten Masterkey und besteht mit dem Modus-Readback.
Die tatsächliche CLI bricht auf dem ungeeigneten Mount ebenfalls vor Datenbank,
Masterkey und PAT ab, ohne Token auf stdout. Keine Sicherheitsprüfung wurde
für die WSL-Umgebung abgeschaltet.

macOS x64/arm64, Windows-SCM und physische PiKVM-/Comet-/JetKVM-Geräte brauchen
weiter eigene ausgeführte Prüfungen. Die vorhandenen CI-Definitionen und
launchd-/SCM-Einstiege allein sind kein Nachweis ihrer Ausführung.
Docker/Podman beziehungsweise ein Engine-Socket sind auf ENVY und in der
geprüften WSL-Umgebung nicht verfügbar. Der Containerbau/-lauf bleibt separat
offen. WSL2-Ergebnisse gelten für diese Linux-Umgebung und ersetzen keine
Abnahme anderer Distributionen, Architekturen oder realer Geräte.

## Nacharbeit: schlafender realer PiKVM-Host

Der Aufweck-Sonderpfad aus `f4f76cc`/`972e028` wurde auf Nutzerhinweis durch
normale Steuerung ohne Videobild ersetzt. `01e8496` behält Sessions trotz
fehlendem Bild und ergänzt verbindungsbezogene Eingabereferenzen;
`65f1e97`/`0bf98d2` korrigieren bestätigten Sessionverlust und die eigene
Journalrotation in der Oberfläche. Bildbezogene MCP-Aufrufe behalten ihre
bisherigen Prüfungen. Details stehen in der
[Reviewvorlage](native-review.md#nacharbeit-am-realen-pikvm).

Für den geänderten Kern bestehen unter Windows Rustfmt, Clippy mit
`-D warnings`, 122 reguläre Tests und zehn direkte Vergleiche gegen die
Originalimplementierung. Die neuen Treibertests bilden offline gemeldetes HID
für PiKVM und beide Comet-Modelle nach. Vue-Build und die vier Browser-Queue-
Tests bestehen. Der Browsercheck prüft zusätzlich Eingaben ohne Anfangsbild,
unbekannten Ausgang ohne Replay und Kontrollübernahme nach Sessionverlust;
der MCP-HTTP-/stdio-Check verwendet kopierte Binaries mit leerem PATH.
Die abschließende Browserprüfung erzwingt außerdem eine verspätete
Eingabebestätigung nach Fokusverlust und prüft die anschließende Texteingabe.
`4ead283` besteht unter Windows mit dieser Browserprüfung, allen 17
Netzwerk-/HID-Tests und erneutem strengen Clippy über alle Ziele. Die zwei
zusätzlichen nativen Tests prüfen zeitlich getrennte Tastaturzustände bei
USB-Polling und deren Freigabe bei Abbruch.

Die erneute Linuxprüfung unter Ubuntu 24.04/WSL2 als UID/GID 65534 besteht:
Rustfmt, Clippy mit `-D warnings`, Vue-Build, 121 reguläre native Tests, zehn
Originalvergleiche sowie vollständige Browser- und MCP-HTTP-/stdio-Interop.
Der native und Web-Code entspricht `0bf98d2`; die in `e963980` präzisierte
Sessionverlust-Fixture besteht unter Windows und Linux. Die früheren
Linux-Releasepakete wurden in dieser Nacharbeit nicht neu gebaut.

Am Stand `4ead283` bestehen unter Linux zusätzlich Vue-Build/Typecheck,
Rustfmt und Clippy mit `-D warnings`. Der folgende vollständige Testlauf
endete während der Kompilierung mit Exitcode 1 ohne Diagnose und liefert
kein neues Testsuite-Ergebnis. Weitere WSL-/Linux-Läufe auf ENVY bleiben nach
dem RAM-Notfall auf Nutzeranweisung gesperrt. Der frühere vollständige
Linuxnachweis ist deshalb ausdrücklich dem oben genannten Stand zugeordnet.

Der erste abschließende Windows-Release-Lauf aus `87be963` wurde auf AMBOSS mit einem
Cargo-Job begonnen. Nach `native_deadline_exceeded` des Remote-Aufrufs waren
keine eigenen Buildprozesse, kein Releasebinary und kein Paket vorhanden.
Dieser Lauf ist kein bestandener Paketnachweis; ein weiterer Build wurde
erst nach einer neuen ausdrücklichen Nutzerfreigabe ausgeführt.

Der separat freigegebene Einmallauf desselben sauberen Standes `87be963` besteht
auf AMBOSS als unabhängige Windows-Aufgabe mit einem Cargo-Job und
BelowNormal-Priorität, ohne WSL. Das Full-Paket mit 483 Dateien besteht mit
allen 482 Prüfsummen, abgeglichenen Eingabehashes und der vollständigen
Release-Vue- und MCP-HTTP-/stdio-Prüfung ohne Neukompilierung. Der fehlende
Exitcode im Startskript ist anhand des fertigen Pakets, Scheduler-Ergebnis 0
und der bestandenen Laufzeittests abgeglichen. Die temporäre Aufgabe ist
entfernt; der bestehende Testdienst auf ENVY verwendet das geprüfte Release
mit erhaltenen Daten/Secrets und erfolgreicher lokaler/LAN-TLS-Prüfung.
Archiv- und Binaryhash stehen in der [Reviewvorlage](native-review.md).

Der reale Agentlauf (`gpt-6.1-sol`, `medium`) am Debugbinary aus `4ead283`
verwendet alle sechs MCP-Tools: Normale Shift-Eingabe und Mausbewegung ohne
Anfangsbild führen zu einem frischen Sperrbildschirm; das autorisierte
Entsperren funktioniert. Die neue Notepad-Zeile kommt vollständig mit
korrekter Groß-/Kleinschreibung und Wiederholungen an; die Auswahl von genau
„Blue“ per Maus ist im Bild bestätigt. Kontrolle und eigene Session wurden
freigegeben. Die verbliebenen Zeichenersetzungen entsprechen einem deutschen
Geräteprofil bei einer Zielbelegung wie US-International.

Nach Nutzerbestätigung von US-International wurde am 27. Oktober das
KVMHelm-Geräteprofil ohne aktive Kontrolllease von `de` auf `us` abgeglichen;
andere Konfiguration und Secrets blieben erhalten. Der erneute unabhängige
Agentlauf am finalen Windows-Release verwendet alle sechs MCP-Tools
(19 Aufrufe): normale Eingaben ohne Anfangsbild, anschließendes echtes Bild,
Entsperren, einmalige exakte ASCII-Eingabe mit 70 Zeichen und Auswahl von
„Blue“ sind visuell bestätigt. Kontrolle/Session sind freigegeben. Das
akzeptiert die konkrete ASCII-Zeile, kein vollständiges US-International
mit toten Tasten/Unicode. Zeile, Thread und Pluginprüfungen stehen in der
[Reviewvorlage](native-review.md). Kein neuer Build oder WSL-Lauf war nötig.

Die anschließende UI-Gegenprüfung fand im damaligen Release einen bisher
nicht geprüften Klick-/Drag-Unterschied bei nur einem Pixel Mausbewegung.
Quellkorrektur, 17 gezielte Handlerprüfungen, Typecheck und Queue-Tests
bestehen. Nach den zusätzlichen Nutzerfreigaben aus Kommentaren 52 und 56
wurden auf AMBOSS nacheinander genau die beiden genehmigten Full-Builds
`91646c3` und `17bc3bc` ausgeführt: je ein Cargo-Job, BelowNormal, mindestens
vier GiB freier RAM, kein WSL. Beide temporären Aufgaben und alle eigenen
Build-/Transferprozesse sind beendet beziehungsweise entfernt.

Das gemeinsame Release `17bc3bc` enthält zusätzlich den gewünschten
markanteren Browserton. Es besteht mit allen 482 Prüfsummen und Eingabehashes,
vollständiger Vue-Abnahme einschließlich Klickregression, MCP-HTTP/stdio
und Plugin-Interop. Das eingebettete Tonmodul wurde über Pluginbenachrichtigung,
SSE und Vue tatsächlich mit Chromium offline gerendert: sechs Töne über etwa
1,5 Sekunden, stärkerer Pegel ohne Clipping, begrenzte Überlagerung,
Knotenfreigabe und keine historische Wiedergabe nach Neuladen.

Seit 09:14:58 UTC läuft das geprüfte Release im vorhandenen Testdienst auf
ENVY als PID 64316. Vorherige private Sicherung und freie Kontrolle sind
bestätigt; PAT, Geräte-/Pluginkonfiguration und dauerhafte Tokenfelder sind
erhalten. Lokale/LAN-TLS-Prüfungen bestehen. Keine neue physische,
Linux-/macOS-/SCM-/Container- oder Ressourcenabnahme ist damit verbunden.
Artefakthashes und genaue Abgrenzung stehen in der
[Reviewvorlage](native-review.md#ui--mcp-abgleich-nach-der-rückfrage).

## ENVY-Dauerbetrieb nach Nutzerfreigabe

Kommentar 59 gibt am 27. Oktober die Übernahme auf `main` und den dauerhaften
ENVY-Betrieb frei. Das unveränderte geprüfte Release aus `17bc3bc` läuft nun
über die Windows-Aufgabe `KVMHelm` mit S4U unter dem bisherigen Datenbesitzer
in Session 0. Boot-/Anmeldetrigger und ein unbefristeter Minutentrigger sind
aktiv. Der tatsächliche Prozessausfalltest besteht ohne expliziten Start:
Readiness nach 4,23 Sekunden, genau ein neuer Daemon und keine alten eigenen
Helfer. Daten, Secrets, PAT und Konfiguration sind erhalten; lokale und
LAN-TLS-Prüfungen bestehen. Ein Hostneustart oder Windows-SCM-Lauf wurde damit
nicht ausgeführt. Pfade und Wartung stehen in der
[Betriebsanleitung](native-packaging.md#dauerbetrieb-auf-envy).
