# Native Pakete und Betrieb

TASK-0126 bleibt bis zur Gesamtabnahme offen. Die Pakete enthalten den
Rust-Daemon mit eingebauter Vue-Oberfläche. Der TypeScript-Server bleibt im
Quellbaum als Vergleichsreferenz; er ist kein Bestandteil der Pakete oder
des neuen Container-Runtimes. `npm start` startet weiterhin diese Referenz.

## Bauen

Buildvoraussetzungen: Node 24.18.0/npm, Rust 1.99.1 gemäß
`rust-toolchain.toml`, Git, `tar` und C/C++-/CMake-Werkzeuge für SQLite, Opus
und die WebRTC-Kryptobibliothek. Unter Windows wird MSVC verwendet.
Die Release-Konfiguration aktiviert Thin-LTO und eine Codegen-Einheit;
Windows-Pakete verwenden statisches CRT und `/Brepro`.
`packaging/msvc-static.cmake` aktiviert zusätzlich Opus' eigene
`OPUS_STATIC_RUNTIME`-Option, damit dessen C-Code dieselbe CRT verwendet.
Bei einem älteren dynamischen Opus-Releasecache bereinigt der Packager nur
dieses Cargo-Paket im zuvor geprüften Repository-Targetverzeichnis. Die
Policydatei geht in den Buildfingerprint ein; CRT-Linkerkonflikte brechen ab.

```sh
npm ci
npm run build:web
npm run package:native -- --target x86_64-pc-windows-msvc --profile full
```

Weitere Ziele: `x86_64-unknown-linux-gnu`, `x86_64-apple-darwin` und
`aarch64-apple-darwin`. Auf dem jeweiligen Zielhost bauen; der Packager
installiert keinen Cross-Compiler. Linux verwendet GNU/glibc, macOS ein zur
Architektur passendes SDK. `.github/workflows/native-packages.yml` beschreibt
manuell startbare Zielhostprüfungen. Ein CI-Job ist noch kein Plattformnachweis.

| Profil  | Inhalt                                             | Einsatzgrenze                                                             |
| ------- | -------------------------------------------------- | ------------------------------------------------------------------------- |
| `core`  | Rust-Daemon, Vue-Assets, Notices, Betriebsvorlagen | JetKVM-Video und bestehende Plugins brauchen separate Helfer              |
| `media` | Zusätzlich FFmpeg 6.1.1                            | Bestehende Plugins brauchen separat Node                                  |
| `full`  | Zusätzlich Node 24.18.0                            | Bisheriger Helferumfang; keine npm-Installation oder Pluginabhängigkeiten |

PiKVM-/Comet-Bilder und natives Audio brauchen weder FFmpeg noch Node.
JetKVM-Video verwendet einen dauerhaften FFmpeg-Prozess. Aktivierte
vertrauenswürdige Plugins starten den Node-IPC-Broker und Kindprozesse.
Helfersuche: `KVMHELM_FFMPEG`/`KVMHELM_NODE`, neben dem Daemon, dann `PATH`.
Der Corebetrieb selbst startet kein Node.

Archive und externe SHA-256-Dateien landen unter `target/packages`, Downloads
im nach SHA-256 adressierten `target/runtime-cache`. Downloads werden gegen
`packaging/runtime-lock.json` geprüft. Quellen sind die offiziellen
[Node-Prüfsummen](https://nodejs.org/dist/v24.18.0/SHASUMS256.txt) und
[FFmpeg-Releaseassets](https://github.com/eugeneware/ffmpeg-static/releases/tag/b6.1.1).
Abweichungen brechen den Build ab. Paketbau installiert keinen Dienst.

`package.json` im Paket hält Ziel, Profil, Compiler, Gitrevision/Dirty-Zustand,
Lockfilehashes, Helferversionen, PE-Importe und Dateihashes fest.
`--binary DATEI` kennzeichnet ein vorhandenes Binary als `supplied-binary`;
es bestätigt keinen neuen Compilerlauf. Windows prüft auf separate
VCRUNTIME-/MSVCP-Imports und weist sie zurück.

Sortierte Tar-Dateien, Eigentümer 0, feste Modi, normalisierte gzip-Metadaten
und `SOURCE_DATE_EPOCH` ermöglichen identische Archivmontage aus identischen
Eingaben. Standard ist der Zeitpunkt des letzten Gitcommits. Der Integrationstest
montiert dasselbe Binary zweimal und vergleicht Archivhashes. Das beweist keine
identischen Compilerergebnisse aus frischen Builds oder auf anderen Hosts.
Ohne `.git` sind `KVMHELM_BUILD_REVISION` (40 Hexzeichen oder `unversioned`)
und `SOURCE_DATE_EPOCH` möglich; die Provenienz bleibt als dirty gekennzeichnet.

`THIRD-PARTY.json` und `licenses/` bewahren Notices der gebündelten
Browsermodule, des Cargo-Normal-/Buildgraphen, vendorter Bibliotheken und
Helfer. Fehlende Cratetexte werden aus exakten upstream Commits ergänzt;
Quelle/Hash stehen in `packaging/license-sources.json`. Die fehlende
Anwendungslizenz wird dadurch nicht ersetzt oder neu festgelegt.

## Installation und Migration

In ein neues Versionsverzeichnis entpacken. Den externen Archivhash über einen
vertrauenswürdigen Kanal vergleichen, dann `verify.ps1` unter Windows oder
`./verify.sh` unter Linux/macOS ausführen. Prüfsummen erkennen Dateiänderungen;
sie sind keine Signatur und kein eigener Vertrauensanker.

Nur bei einer neuen Installation:

```sh
kvmhelm token bootstrap --data /protected/data --secrets /protected/secrets
kvmhelm serve --data /protected/data --secrets /protected/secrets
```

Unter Windows absolute NTFS-Pfade verwenden. Die private Datei
`SECRET_DIRECTORY/cli.pat` dient zur Anmeldung auf `http://127.0.0.1:8765`.
Vor Migration den alten Daemon stoppen und Daten sowie Secrets einschließlich
`master.key` zusammen sichern. SQLite-v1-Dokumente, AES-GCM-Dateien, Tokenhashes,
Geräte-IDs, Revisionen, Pluginrecords und Präferenzen bleiben erhalten.
Bestehende Tokens brauchen kein zweites Bootstrap. Nichtleere ältere Schemata
erhalten vor Übernahme ein Backup; unbekannte neuere Schemata werden abgelehnt.

Nativ wird keine `.env` geladen. Bisherige `KVMHELM_*`-Werte in die
Prozess-/Dienstumgebung oder Flags übertragen. Daten und Secrets ausdrücklich
setzen. Der Account muss ihre privaten ACLs/POSIX-Rechte besitzen; LocalSystem
oder ein anderer Dienstaccount erbt diese Berechtigung nicht. Pluginprogramme
müssen für denselben Account erreichbar sein.

`healthcheck --url URL [--ca CA_DATEI]` prüft `/readyz` ohne PAT oder lokalen
Core. `doctor` verwendet die Anmeldung am bestehenden Daemon. Eine CA-Datei
ergänzt Vertrauen und erhält die Hostnamenprüfung. LAN verlangt TLS und
passende Origins; siehe [LAN-Betrieb](lan-access.md).

## Start, Stop, Update und Rollback

Ctrl+C beendet Listener, akzeptierte Arbeit, Geräte und eigene Helfer.
Unter Unix wird auch SIGTERM geordnet verarbeitet.

- Windows: `kvmhelm.exe service run` ist ein SCM-Einstieg mit denselben
  `serve`-Optionen. Konto und Registrierung legt der Betreiber fest.
  Außerhalb von SCM schlägt er vor dem Öffnen von Core/State fehl.
  Running folgt der Bereitschaft, Stopped der Core-/Store-Bereinigung.
  Der SCM-Integrationstest ist auf disposable GitHub-Runner begrenzt;
  auf ENVY wurde kein Dienst angelegt.
- Linux: `kvmhelm.service` ist eine systemd-Vorlage für einen zuvor angelegten
  Benutzer `kvmhelm`, `/opt/kvmhelm/current` und `/var/lib/kvmhelm`. Bootstrap
  unter diesem Account ausführen. `ProtectHome` verlangt Pluginprogramme
  außerhalb geschützter Homeverzeichnisse oder eine bewusste Anpassung.
- macOS: `local.kvmhelm.plist` ist eine LaunchAgent-Vorlage für den Datenbesitzer.
  `__PACKAGE_ROOT__` und `__HOME__` durch absolute XML-escaped Pfade ersetzen.

Für Updates stoppen, konsistente private Backups erstellen, ein geprüftes
neues Geschwisterverzeichnis entpacken und den ausführbaren Pfad im gestoppten
Dienst umstellen. Zustand außerhalb der Paketverzeichnisse halten. Nach Start
Readiness, Doctor und Beobachtung prüfen. Für Rollback wieder stoppen und alten
Programmstand samt passendem Backup wählen. Niemals beide Versionen auf
demselben Store starten. Sessions und Controlleases überleben keinen Neustart;
Eingaben werden nicht nachgesendet. Extraktion registriert keinen Dienst.

## Container

Der Dockerfile baut Browserassets und nativen Daemon in getrennten Stufen.
Das finale Nicht-root-Image startet `/app/kvmhelm`; es enthält keinen
TypeScript-Server oder npm. `PROFILE=core|media|full` steuert die Helfer.
Basisimages sind per OCI-Digest, Buildwerkzeuge über einen festen Debian-
Snapshot gepinnt. Die C++-Runtime für Node wird samt Notices kopiert.
Der Containerbau ist derzeit auf Linux x64 beschränkt.

```sh
docker build --build-arg PROFILE=full --build-arg SOURCE_REVISION=COMMIT_SHA --build-arg SOURCE_DATE_EPOCH=EPOCH -t kvmhelm:native .
docker compose config --quiet
docker compose run --rm kvmhelm token bootstrap
docker compose up -d
```

COMMIT_SHA durch 40 Hexzeichen und EPOCH durch einen Unixzeitpunkt ersetzen;
ohne Angaben bleibt der Build `unversioned`. Vorher `runtime/data` und
`runtime/secrets` für UID/GID 1000 sowie private TLS-Dateien vorbereiten.
Compose behält Read-only-Root, Loopbackfreigabe und explizites TLS bei.
Der native Healthcheck vertraut der angegebenen CA-Datei.
ICE/UDP und Docker-NAT brauchen eine eigene Geräteabnahme.

Die CI prüft Readiness, native Helfer, SIGTERM, Exitstatus und Wiederöffnung
derselben Volumes. Lokal ist kein Docker-Engine verfügbar; Containerbau und
Containerlauf bleiben bis zu ausgeführter CI offen.

## Prüfumfang

`scripts/test-native-package.mjs` prüft ein entpacktes Full-Paket, Hashes,
Manipulationserkennung, identische Archivmontage, Migration vom ursprünglichen
TypeScript-Core mit vorhandenem PAT/Secret/Gerät/Präferenzen/Pluginrecord,
zwei Programmverzeichnisse und Rücklesbarkeit nativer Änderungen im alten Core.
Das Plugin verwendet beigefügtes Node mit leerem PATH und echte Advanced-IPC-
Video-Buffer. Der Windows-Prozessabbruch prüft WAL-/Jobobjekt-Recovery;
er ersetzt keinen SCM-Stoptest. Der JetKVM-Check nutzt beigefügtes FFmpeg
gegen einen tatsächlichen lokalen Chromium-WebRTC-Peer.

Am 26.10.2026 auf ENVY (Windows x64, Rust 1.99.1) bestanden:

- Rustfmt, Clippy mit `-D warnings`, 112 native Prüfungen und zehn ausdrücklich
  ausgeführte Vergleiche gegen das ursprüngliche TypeScript-Backend.
- Tatsächlicher optimierter Full-Paketbau mit statischer Opus-/Rust-CRT,
  zurückgewiesenen CRT-Linkerkonflikten und PE-Importprüfung. Das Binary ist
  25.603.584 Bytes groß. Die Archive belegen ungefähr 9,8 MiB (`core`),
  38,7 MiB (`media`) und 72,5 MiB (`full`); das sind Paketgrößen,
  keine Laufzeit-RAM-Messungen.
- Alle drei Profile entpackt und mit dem mitgelieferten Windows-Prüfer
  verifiziert; identische native EXE und die jeweils vorgesehenen Helfer.
  Full enthält 483 Dateien, Notices für 252 Cargo-Pakete und zehn Browsermodule.
- Full-Paketmigration mit vorhandenen Tokens/Secrets/Präferenzen/Plugins,
  zwei Programmverzeichnissen, Helferbereinigung nach Prozessende,
  Rücklesen nativer Änderungen und erneutes Öffnen der ursprünglichen Sicherung
  im alten Core. Zwei erneute Montagen desselben Binaries waren bytegleich.
- Beigefügtes Node 24.18.0 bei leerem PATH mit echten Advanced-IPC-Buffern;
  beigefügtes FFmpeg 6.1.1 gegen den JetKVM-Chromiumpeer einschließlich
  H.264/JPEG, Opus/PCM, USB-Reports, Reconnect und ungewisser RPC-Antworten.
  Die Releasefixture wartet auf unabhängig eintreffende Audio-RTP-Bereitschaft.
- Das optimierte Binary besteht zusätzlich den Janus-/Vue-Audiocheck mit
  Stereo-Opus-Decoding, Mono-Mikrofonencoding, Plugin-PCM, Kontroll-/Fokusverlust
  und Bereinigung sowie die Vue-Abnahme mit Geräte-CAS, sicherer Paste-Vorschau,
  Vollbild, Journalrotation, Restart/SSE ohne neue Controllease, Tokenwiderruf
  und mobilem Touch/Drag.
- `verify.sh` unter WSL gegen ein entpacktes Paket, die PowerShell-Parserprüfungen
  und XML-Lesen der launchd-Vorlage. Das ist kein nativer Linux-Daemonlauf
  beziehungsweise kein launchd-Lauf.

Linux/macOS-Laufzeit, SCM/systemd/launchd, Container und physische Geräte
werden getrennt vom lokalen Windows-Prozessbetrieb ausgewiesen. Repräsentative
RAM-/CPU-/Latenzmessungen einschließlich FFmpeg, Broker und Pluginprozessen
gegen die TypeScript-Referenz bleiben vor Abschluss erforderlich.
