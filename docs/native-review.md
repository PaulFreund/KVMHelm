# Native Portierung – Vorlage für die Nutzerabnahme

TASK-0126 liegt auf `port/task-0126-rust`. Ausgangsstand ist
`b40b2dde246533a25a18148a6b59faced676d9ba`. Der Branch enthält den
integrierten Rust-Daemon und die CLI mit unveränderter Vue-Oberfläche.
Die [Funktionszuordnung](rust-port.md) verbindet die ursprünglichen Quellen
mit Implementierung, Verträgen und ausgeführten Nachweisen.

## Ergebnis und Betrieb

Der native Prozess besitzt HTTP, WebSocket, SSE, MCP/stdio-Anbindung,
SQLite/Secrets, Autorisierung, Sessions/Leases, Eingabequeues und Journale.
PiKVM, beide Comet-Modelle, JetKVM, geteiltes Video und vorhandene
Audio-/Mikrofonfunktionen sind integriert. Die stdio-Bridge nutzt den
bestehenden Daemon. Installation startet mit leerer Geräteliste; `demo`
bleibt ausdrücklich optional und führt keine aktivierten Plugins aus.

Der Core benötigt kein Node/npm. JetKVM-Video verwendet FFmpeg; bestehende
Plugins verwenden Node mit dem kompatiblen Advanced-IPC-Broker einschließlich
echter `Buffer`. Das sind eigene optionale Prozesse. SQLite, TLS und Opus
sind nativ eingebunden. Die Profile `core`, `media` und `full` paketieren
die vorgesehenen Helfer, Hashes, Lizenzen und Betriebsvorlagen.

[README: Start und Entwicklung](../README.md#start) und
[Pakete, Migration und Rollback](native-packaging.md) beschreiben die
ausführbaren Wege. Im eingerichteten Quellcheckout:

```sh
npm ci
npm run build
npm run build:native
npm run cli -- token bootstrap
npm start
```

Bootstrap nur für eine neue Installation ausführen. Bei Migration den alten
Daemon stoppen und Datenbank sowie Secrets einschließlich Masterkey gemeinsam
sichern; vorhandene Tokens weiterverwenden. Die Paketprüfung verifiziert
TypeScript → Rust → TypeScript mit denselben Daten, Secrets, Tokenhashes,
Präferenzen und Pluginrecords. Direkte Binaryaufrufe verwenden Flags oder
Prozessumgebung; die npm-Startwrapper laden zusätzlich `.env`.

## Ausgeführte Nachweise

Windows x64 und Ubuntu 24.04/WSL2 x64 sind funktional geprüft. Die
[Plattformmatrix](native-platforms.md) trennt Prozess-, Paket-, Dienst- und
Hardwareprüfungen. Der saubere Stand `385682d` wurde auf beiden Plattformen
erneut paketiert: alle drei Profile, Prüfsummen und mitgelieferte Prüfer,
Migration/Neustart und zwei bytegleiche Montagen desselben Binaries bestanden.
Die Binaries stimmten mit dem zuvor vollständig geprüften Integrationsstand
überein. Das beweist deterministische Archivmontage bei gleichen Eingaben,
keine identischen Compilerergebnisse auf beliebigen Hosts.

Die vorhandenen Prüfungen umfassen Daten-/Secretkompatibilität, strikte
Schemas, HID/DE/US, Autorisierung, CAS, Rückstau, Abbruch, Replay und ungewisse
Eingaben; lokale KVMD-/Janus-Protokollserver und echte Chromium-WebRTC-Peers;
MCP über HTTP/stdio mit Wiederanlauf; Vue-Verwaltung, Paste-Vorschau, Audio,
Kontroll-/Fokusverlust, Vollbild, Touch und Plugins. Die 38 Originaltests
bleiben unverändert im isolierten Referenzcommit; vier Browser-Queue-Tests
laufen weiterhin gegen den aktuellen Vue-Code.

Der abschließende Vertragsabgleich ergänzt HEAD einschließlich
GET-Metadaten/Berechtigungen und sofortiger Streamfreigabe, die ursprünglichen
OPTIONS-Antworten sowie den Offlinebefehl `demo` ohne Pluginstart.
`scripts/native-admin-contract-interop.mjs` vergleicht beide tatsächlichen
Daemons und CLIs; die drei zusätzlichen Rust-Tests prüfen Inhaltslänge,
Readiness, Berechtigungen, getrennte Ratenlimits und Streambereinigung.

Rustfmt, Clippy mit `-D warnings` und die vollständigen nativen Tests bestehen
mit diesen Korrekturen unter Windows (115 Tests) und Linux (114 Tests), dazu
jeweils alle zehn direkten Vergleiche gegen das Original.

## Laufzeitmessung

Der [Windows-Messbericht](native-performance.md) enthält 36 Lastfenster,
neun Szenarien je Backend in zwei Wiederholungen und 2.562 erfolgreiche
Toolaufrufe. FFmpeg, Broker, Plugins und beobachtete Konsolenprozesse sind
enthalten. Beispielspannen der mittleren Arbeitssätze:

| Szenario | Rust MiB | TypeScript MiB |
| --- | ---: | ---: |
| Idle | 16,8 | 105,9–107,4 |
| Aktives KVMD | 30,9–31,2 | 155,8–157,8 |
| JetKVM mit FFmpeg | 70,7–70,8 | 158,8–177,1 |

Arbeitssatz und privater Commit sind in den gemessenen Fällen niedriger,
unter aktiver Last auch die Prozess-CPU. Die Latenz verbessert sich nicht
durchgehend; kleine Audio-Screenshot-Antworten haben teils höhere native
Mediane. Die Messung gehört zu `24ae930` und seinem im Bericht genannten
Binaryhash; `385682d` war binärgleich. Der anschließende Vertragsabgleich ist
kein neuer Ressourcenmesslauf. Kurze synthetische Fenster auf einem nicht
exklusiven Host belegen keine Langzeit- oder Hardwareleistung.

## Konkrete Restgrenzen

- macOS x64/arm64: Build, Lauf und launchd fehlen mangels passendem Host/SDK.
- Windows-SCM: Einstieg und disposable CI-Prüfung vorhanden; Dienstlauf lokal
  nicht ausgeführt. Linux-systemd wurde tatsächlich mit unprivilegiertem
  Daemon in einer transienten Unit geprüft.
- Container: Dockerfile, Compose und Prüfjob vorhanden; Build/Lauf auf ENVY
  und WSL mangels Engine nicht ausgeführt.
- Physische PiKVM-/Comet-/JetKVM-Geräte, reale Mikrofone und Firmware-/LAN-
  Bedingungen wurden nicht benutzt; alle Eingaben und Audiosignale stammen
  aus eigenen synthetischen Gegenstellen.
- Linuxnachweise gelten für Ubuntu 24.04/WSL2 x64; das gebaute Paket verlangt
  GLIBC_2.39. Keine Linux-/macOS-Ressourcenmessung. Höhere Auflösungen,
  längere Last und kleine Antwortzeiten auf ruhigem Host brauchen eigene Läufe.

Die Nutzerabnahme des Branches und eine spätere Produktivfreigabe bleiben
getrennte Entscheidungen. Es erfolgten kein Push, main-Merge, Dienstwechsel
oder Produktiveinsatz.
