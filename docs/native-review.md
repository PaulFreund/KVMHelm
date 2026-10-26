# Native Portierung – Vorlage für die Nutzerabnahme

TASK-0126 liegt auf `port/task-0126-rust`. Ausgangsstand ist
`b40b2dde246533a25a18148a6b59faced676d9ba`. Der Branch enthält den
integrierten Rust-Daemon und die CLI mit der vorhandenen Vue-Oberfläche.
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

Der Zwischenstand `972e028` bestand Rustfmt, Clippy mit `-D warnings`, die
regulären nativen Tests unter Windows (121) und Linux (120) sowie jeweils alle
zehn direkten Vergleiche gegen das Original. Die anschließende Korrektur der
Bedienung ohne Video ist im folgenden Abschnitt getrennt ausgewiesen.

## Nacharbeit am realen PiKVM

Am 26.10.2026 wurde der Nutzerfehler am konfigurierten TestLaptop reproduziert:
KVMD 4.16 liefert HTTP 503, HDMI ist offline und HID wird als offline gemeldet.
Auch der direkte Bildabruf über den Originaltransport scheitert damit. Ohne
ersten Frame scheiterte bisher die Sessionöffnung und der Web-Kontrollknopf
blieb deaktiviert.

Die Zwischenstände `f4f76cc`/`972e028` versuchten einen festen Shift-Aufweckpfad,
verlangten danach aber weiterhin ein Bild. Der Nutzer stellte klar, dass
normale Maus-/Tastatureingaben nach Kontrollübernahme auch ohne Video möglich
sein müssen. `01e8496` entfernt deshalb diesen Sonderpfad samt `wake`-Option.
Session und Kontrolle bleiben bei fehlendem Video verfügbar; der Bildfehler
wird getrennt gemeldet. Normale HID-Eingaben sind auch bei offline gemeldetem
KVMD-USB-Host möglich. Die Oberfläche verwendet normalisierte Mauskoordinaten
und eine Referenz auf Verbindung und Inputrevision. Bestehende MCP-Aufrufe mit
Bildreferenz behalten ihre vollständigen Prüfungen. Kein Bild wird erfunden,
und Kontrollübernahme löst keinen automatischen Aufweck-Tastendruck aus.

`65f1e97` behebt zusätzlich die im Browser reproduzierte Kontrollübernahme mit
einer nach Gerätebearbeitung geschlossenen Session: Nur beim ausdrücklich
bestätigten `SESSION_LOST` wird eine neue Kontrollsession geöffnet. Ungewisse
Eingaben und Verbindungsfehler werden nicht wiederholt. Die
[Betriebsbeschreibung](rust-port.md#steuerung-ohne-videobild) erklärt beide
Referenzarten, Vorprüfung, Lease und Request-Journal. Ein bereits geöffneter
Browser muss die neuen Assets durch Neuladen laden.

`0bf98d2` verhindert außerdem, dass die eigene Freigabe während der
Journalrotation den nächsten Auftrag abbricht oder eine eintreffende Lease
ohne bisherige Session als Kontrollverlust gilt. Die Browserprüfung beginnt
ausdrücklich ohne Session und ohne Bild und erzwingt später einen bestätigten
Sessionverlust vor der Kontrollübernahme.

Für den nativen Kern aus `01e8496` bestehen unter Windows Rustfmt, Clippy mit
`-D warnings`, alle 122 regulären Tests und zehn direkte Originalvergleiche.
Die neuen Tests prüfen Eingaben ohne Anfangsbild, beide Video-Fehlerklassen,
offline gemeldetes HID bei PiKVM und beiden Comet-Modellen, Kontrollrechte,
Arbitrierung, vollständige Vorprüfung, Replay und unbekannte Zustellung.
Zurückkehrendes Video unterbricht normalisierte Eingaben nicht. Der separate
MCP-HTTP-/stdio-Test besteht mit kopiertem Binary und leerem PATH. Der
vollständige Browsercheck aus `0bf98d2` besteht einschließlich der ersten
Kontrollübernahme ohne Session/Bild, ungewisser Eingabe ohne Replay,
bestätigtem Sessionverlust und Journalrotation. `e963980` macht dabei die
Sessionverlust-Fixture eindeutig: Sie wartet zunächst auf eine tatsächlich
bestehende Session und bestätigt, dass der injizierte Fehler verbraucht wurde.
Diese letzte Änderung betrifft ausschließlich die Prüfung.

Die abschließende Browserprüfung verzögert zusätzlich die Antwort einer schon
angenommenen Tastatureingabe bis nach dem Fokusverlust. Ihre Bestätigung muss
die Eingabereferenz und das verbleibende Journal für dieselbe Session weiter
aktualisieren. Andernfalls wird die nächste bewusst gesendete Texteingabe als
`STALE_REFERENCE` abgewiesen. Die Oberfläche übernimmt diese Bestätigung ohne
Fokuswiederherstellung, Wiederholung oder Rücksetzen auf eine ältere Session
beziehungsweise Inputrevision.

`4ead283` besteht unter Windows zusätzlich mit allen 17 Netzwerk-/HID-Tests,
Rustfmt, Clippy über alle Ziele mit `-D warnings`, Vue-Build, vier Queue-Tests
und dem vollständigen Browsercheck einschließlich der verzögerten Antwort.
Der reale Zeichenverlust führte außerdem zu fünf Millisekunden Abstand
zwischen PiKVM-Tastaturzuständen. Zwei neue Tests bilden USB-Polling mit
wiederholten Zeichen und wechselnder Groß-/Kleinschreibung sowie Abbruch
während eines gehaltenen Tastenzustands nach.

Unter Ubuntu 24.04/WSL2 bestehen mit UID/GID 65534 ebenfalls Rustfmt, Clippy
mit `-D warnings`, Vue-Build, alle 121 regulären nativen Tests, zehn direkte
Originalvergleiche sowie der vollständige Browser- und MCP-HTTP-/stdio-Check.
Der dort geprüfte native und Web-Code entspricht `0bf98d2`; die abschließende
Browser-Fixture entspricht `e963980`.

Für `4ead283` bestehen unter Linux erneut Vue-Build/Typecheck, Rustfmt und
Clippy mit `-D warnings`. Der anschließende vollständige Testlauf endete
während der Kompilierung mit Exitcode 1 ohne Compilerdiagnose; ein neues
Testsuite-Ergebnis liegt deshalb nicht vor. Nach dem RAM-Notfall sind weitere
WSL-/Linux-Läufe auf ENVY auf Nutzeranweisung gesperrt. Die früheren 121 Tests
und Interopläufe sind kein Nachweis für diese spätere Änderung.

Der frühere reale Agentlauf mit `gpt-6.1-sol`, Reasoning `medium`, über
Streamable HTTP am Debugbinary aus `0bf98d2` hat alle sechs Tools aufgerufen
(19 Aufrufe, Thread `01a1786f-b303-7502-a30c-187d8e349514`). Beobachtungssession
und Kontrollübernahme funktionieren ohne Bild. Eine gewöhnliche Shift-Eingabe
und eine getrennte Mausbewegung nach normalisierten `(16384,16384)` wurden
jeweils als `sent` gemeldet. Zehn Sekunden weitere Bildabrufe erhielten weiter
`NO_SIGNAL`/`FRAME_TIMEOUT`. Der Agent gab Kontrolle und eigene Session frei.
Ein `sent` beweist die Übertragung im Gateway, nicht den Empfang durch das
Zielbetriebssystem oder eine sichtbare Wirkung. PIN, Paint und Notepad wurden
ohne Bild nicht verwendet.

Nach dem Einschalten durch den Nutzer lieferte derselbe Debugstand ein echtes
Bild. Der Agent öffnete Notepad und prüfte die Testzeile visuell: Zeichen
fehlten und Groß-/Kleinschreibung war falsch, obwohl `sent` gemeldet wurde
(12 Aufrufe, Thread `01a1789f-77ee-7a01-8d45-0a7df03f7245`). Die danach
eingebaute zeitliche Trennung der Tastaturzustände ist im folgenden Lauf
geprüft.

Der Lauf am Debugbinary aus `4ead283` verwendete alle sechs Tools über
Streamable HTTP (30 Aufrufe, Thread `01a178c8-af24-7da3-99bd-82e045a286fa`).
Ohne Anfangsbild wurden eine normale Shift-Eingabe und eine normalisierte
Mausbewegung übertragen. Danach erschien der Sperrbildschirm; das autorisierte
Entsperren funktionierte. Eine neue Notepad-Zeile kam vollständig mit 77 Zeichen
an; Groß-/Kleinschreibung und Wiederholungen stimmen. Die Auswahl von exakt
„Blue“ per Maus wurde im Bild bestätigt. Vorherige ungespeicherte Dokumente
blieben erhalten; Kontrolle und eigene Session wurden freigegeben.

Die vollständige Textkorrektheit ist dennoch nicht abgenommen: Das Gerät ist
mit `keyboard_layout=de` konfiguriert, die sichtbaren Ersetzungen entsprechen
dagegen US-International (`typing` → `tzping`, `-` → `/`, `äöü` → `';[`).
Diese Zuordnung entspricht der ursprünglichen Implementierung. Die gewünschte
Abstimmung zwischen Geräteprofil und Zielbelegung ist im Ticket gefragt; sie
wurde noch nicht durch eine Konfigurationsänderung vorweggenommen.

Der Testdaemon wurde mit vorhandenen Daten/Secrets nach privater Sicherung
aktualisiert. Loopback und LAN-TLS, Readiness, Web und Anmeldung bestehen;
ein bereits geöffneter Browser benötigt neue Assets durch Neuladen.

Der zusätzlich einmal freigegebene Windows-Release-Lauf aus dem sauberen
Stand `87be963` wurde auf AMBOSS mit einem Cargo-Job begonnen. Der Remote-Aufruf
meldete nach rund zwei Minuten `native_deadline_exceeded`. Der anschließende
Abgleich fand keine eigenen Buildprozesse, kein Releasebinary und kein Paket;
damit ist der abschließende Releasebau nicht bestanden. Es wurde kein
Ersatzbuild gestartet. Der Testdienst verwendet weiter das geprüfte Debugbinary
aus `4ead283`; sein nativer und Web-Code entspricht `87be963`.
Diese Nacharbeit ist kein neuer Ressourcenmesslauf und
keine neue macOS-/Dienst-/Containerabnahme.

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
- Am physischen PiKVM sind Eingaben ohne Anfangsbild mit anschließendem Video,
  Entsperren, vollständige Notepad-Eingabe und Mausauswahl sichtbar geprüft.
  Exakte Textkorrektheit wartet auf den Abgleich der Tastaturbelegung.
  Comet-, JetKVM-, Audio- und Mikrofonpfade sind weiterhin ausschließlich mit
  synthetischen Gegenstellen geprüft.
- Linuxnachweise gelten für Ubuntu 24.04/WSL2 x64; das gebaute Paket verlangt
  GLIBC_2.39. Keine Linux-/macOS-Ressourcenmessung. Höhere Auflösungen,
  längere Last und kleine Antwortzeiten auf ruhigem Host brauchen eigene Läufe.

Die Nutzerabnahme des Branches und eine spätere Produktivfreigabe bleiben
getrennte Entscheidungen. Der Testdaemon wird auf ausdrücklichen Nutzerwunsch betrieben;
Push, main-Merge, SCM-Dienstinstallation und Produktivfreigabe erfolgten nicht.
