# Native Portierung – Abnahme und Betrieb

TASK-0126 wurde am 27. Oktober 2026 mit Kommentar 59 zur Übernahme auf
`main` und zum dauerhaften Betrieb auf ENVY freigegeben. Ausgangsstand ist
`b40b2dde246533a25a18148a6b59faced676d9ba`. Der integrierte Rust-Daemon
und die CLI mit der vorhandenen Vue-Oberfläche wurden vom Portierungsbranch
ohne Konflikte auf `main` übernommen.
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

Die vollständige Textkorrektheit war in diesem Lauf noch nicht abgenommen: Das Gerät war
mit `keyboard_layout=de` konfiguriert, die sichtbaren Ersetzungen entsprechen
dagegen US-International (`typing` → `tzping`, `-` → `/`, `äöü` → `';[`).
Diese Zuordnung entspricht der ursprünglichen Implementierung. Die gewünschte
Abstimmung zwischen Geräteprofil und Zielbelegung war im Ticket gefragt;
die unten dokumentierte Nutzerantwort und Folgeprüfung lösen diesen Abgleich.

Der Testdaemon wurde mit vorhandenen Daten/Secrets nach privater Sicherung
aktualisiert. Loopback und LAN-TLS, Readiness, Web und Anmeldung bestehen;
ein bereits geöffneter Browser benötigt neue Assets durch Neuladen.

Der erste Windows-Release-Lauf aus dem sauberen Stand `87be963` wurde auf
AMBOSS mit einem Cargo-Job begonnen. Der Remote-Aufruf
meldete nach rund zwei Minuten `native_deadline_exceeded`. Der anschließende
Abgleich fand keine eigenen Buildprozesse, kein Releasebinary und kein Paket;
dieser Versuch ist kein bestandener Paketnachweis. Es wurde kein automatischer
Ersatzbuild gestartet.

Nach neuer ausdrücklicher Nutzerfreigabe wurde derselbe saubere Stand `87be963`
einmal auf AMBOSS als unabhängige Windows-Aufgabe gebaut: ein Cargo-Job,
BelowNormal-Priorität, mehr als vier GiB freier RAM vor Start, kein WSL.
Das Full-Releasepaket besteht mit allen 482 Prüfsummen sowie der vollständigen
Vue-Abnahme und MCP-HTTP-/stdio-Interop am kopierten Releasebinary mit leerem
PATH. Das umfasst die verzögerte Eingabebestätigung nach Fokusverlust,
Steuerung ohne Anfangsbild und ungewisse Eingabe ohne Replay.
Die temporäre Aufgabe ist entfernt, ihre Buildprozesse sind beendet.

Das Startskript konnte den Prozess-Exitcode nicht auslesen. Der Paketabschluss,
Scheduler-Ergebnis 0, Archiv-/Dateiprüfsummen und die bestandenen Laufzeittests
bestätigen das fertige Ergebnis; es war kein weiterer Build nötig.
Das Archiv liegt lokal unter
`target/packages/approved-final-87be963/kvmhelm-0.1.0-x86_64-pc-windows-msvc-full.tar.gz`
(76.058.185 Bytes, SHA-256
`ece53bbd8f8dc276c8b2123dffef66e7591976ec8c83e56ec0dc68439bc11c41`).
Das Binary ist 25.641.984 Bytes groß, SHA-256
`9da3f3067c1bb6b9b7632be0e869e27874cde3371a4f6485dbe6ccb1b5f1b199`.

Der bestehende Testdaemon verwendete nach diesem ersten Wechsel das Release mit erhaltenen
Daten/Secrets nach privater Sicherung und ohne aktive Kontrolllease beim
Wechsel. Readiness, Web und authentifizierte API bestehen lokal und über
geprüftes LAN-TLS. Die Tastaturbelegung wurde bei diesem Releasewechsel
nicht geändert; die anschließende Nutzerantwort ist im folgenden Lauf berücksichtigt.
Diese Nacharbeit ist kein neuer Ressourcenmesslauf und
keine neue macOS-/Dienst-/Containerabnahme.

Am 27. Oktober bestätigte der Nutzer US-International auf dem Ziel-Laptop.
Das bestehende KVMHelm-Profil wurde ohne aktive Kontrolllease von `de` auf
`us` geändert (Revision 4 → 5), alle anderen Gerätewerte und Secrets blieben
erhalten. Der anschließende Lauf mit `gpt-6.1-sol`/`medium` am obigen
Releasebinary verwendet alle sechs MCP-Tools über Streamable HTTP
(19 Aufrufe, Thread `01a17c0c-f9a0-7771-9e94-3f6b2d40e3d7`). Ohne Anfangsbild
übertragene normale Shift-Eingabe und Mausbewegung führten wieder zum
sichtbaren Sperrbildschirm. Nach autorisiertem Entsperren kam diese Zeile
einmalig in einem neuen Notepad-Dokument exakt mit 70 Zeichen an:

```text
TASK-0126 MCP typing check: Blue fox waves hello. AaAa 1122 yzYZ @ 42.
```

Frische Bilder bestätigen Zeichen, Groß-/Kleinschreibung und die Auswahl
von exakt „Blue“ per Maus. Frühere ungespeicherte Tabs blieben erhalten;
Kontrolle und eigene Session wurden freigegeben. Das ist die Abnahme dieser
kompatiblen ASCII-Zeile, keine vollständige US-International-Unterstützung
für tote Tasten oder Unicode. Die vorhandenen Layouts bleiben `de` und `us`.

## UI-/MCP-Abgleich nach der Rückfrage

Die vorhandene Vue-Oberfläche ist erhalten, aber nicht bytegleich mit dem
Node-Ausgangsstand. Die gezielten Änderungen betreffen die Eingabesteuerung
ohne Bild, normalisierte HID-Koordinaten, Sessionverlust und die verspätete
Bestätigung bereits angenommener Eingaben nach Fokusverlust. Die vorhandenen
Verwaltungs-, Medien-, Plugin- und Bedienungsabläufe sind weiterhin Gegenstand
der Browserabnahme. Alle sechs MCP-Toolnamen und bildbezogenen Aufrufe bleiben
erhalten. Der erneute Metadatenvergleich gegen das echte Original bestätigt
als einzige Vertragsänderung die dokumentierte Erweiterung ohne Video:
optionale `reference.frame_id` und ergänzte Hilfetexte; Ergebnisse liefern
zusätzlich `control_reference` und gegebenenfalls `video_error`.

Die gezielte Gegenprüfung am 27. Oktober fand eine zusätzliche
UI-Regression im damaligen Release aus `87be963`: Nach gedrückter linker
Maustaste und einem CSS-Pixel Bewegung sendet dieses Release `drag`, das
unveränderte Node-Original dagegen `click`. Bei der Umstellung auf
normalisierte HID-Koordinaten war die Vier-Pixel-Grenze für Klick/Drag auf
dieser neuen Skala ausgewertet worden. Die Korrektur im Quellcode wertet
diese Grenze wieder in Bildpixeln aus; ohne Bild verwendet sie die
Bildschirmfläche in CSS-Pixeln. Die gesendeten HID-Koordinaten bleiben
normalisiert. Ein entsprechender Browser-Regressionsfall ist ergänzt.

Die tatsächlichen `coords`-/`move`-Quellfunktionen stimmen in 15 Fällen über
drei Bildskalierungen mit dem Original überein; zwei weitere Fälle prüfen
die Grenze ohne Bild. Vue-Typecheck ohne Ausgabe, vier Queue-Tests und
`git diff --check` bestehen. Die damalige Browserabnahme hatte diesen kleinen
Mausbewegungsfall nicht geprüft. Die unten dokumentierte zusätzliche
Nutzerfreigabe und gemeinsame Release-Abnahme schließen die Nacharbeit ab:
Der ergänzte Browserfall bestätigt jetzt auch am eingebetteten Bundle wieder
`click` bei nur einem Pixel Bewegung.

## Markanter Benachrichtigungston und gemeinsames Release

Der zusätzlich gewünschte lautere Browserton ist in `17bc3bc` umgesetzt:
drei abwechselnde Zweitonfolgen mit insgesamt sechs Impulsen über etwa
1,5 Sekunden, markanter Rechteckklang und Spitzenverstärkung 0,55 statt
0,14. Überlappende Benachrichtigungen teilen den laufenden Alarm; ihre
sichtbaren Meldungen bleiben erhalten. Jeder Ton und sein Gain-Knoten
werden anschließend getrennt. Die bisherige Aktivierung durch eine
Browserinteraktion und das stille Verhalten historischer Meldungen bleiben
erhalten. Das separate Transkriptionsplugin wurde dafür nicht verändert.

Kommentar 52 erlaubte genau einen zusätzlichen Windows-Full-Build von
`91646c3` auf AMBOSS. Dieser Lauf wurde von 08:43:55 bis 08:49:06 UTC
erfolgreich ausgeführt. Als der Tonwunsch während dieses Laufs eintraf,
blieb dessen Quellstand fest. Kommentar 56 erlaubte ausdrücklich genau
einen weiteren Full-Build aus `17bc3bc`; er lief von 09:00:45 bis 09:05:05 UTC.
Beide Läufe hatten einen Cargo-Job, BelowNormal-Priorität, mindestens vier
GiB freien RAM und kein WSL. Die Einmalbelege erfassen jeweils genau einen
Paket-/Cargo-Aufruf. Keine parallelen oder automatischen Ersatzbuilds.
Die temporären Windows-Aufgaben sind entfernt, eigene Build- und
Transferprozesse beendet. `91646c3` wurde vor der gemeinsamen Abnahme nicht
zwischenzeitlich in den Testdienst übernommen.

Das gemeinsame Windows-x64-Full-Paket aus dem sauberen Stand
`17bc3bc1aa42a05a1e70b0365d83d83a20057ab7` besteht mit allen 482 Prüfsummen,
den abgeglichenen Lock-/Paketierungs-/Helfer-/Webnotizhashes sowie der
vollständigen Vue-Abnahme einschließlich Klickregression, MCP über
HTTP/stdio und vollständiger Plugin-Interop, jeweils ohne weitere
Kompilierung. Auch das tatsächlich eingebettete Tonmodul ist über die
Kette Plugin-IPC → SSE → Vue geprüft: Zwei echte Fixture-Benachrichtigungen
erzeugen zusammen sechs Töne. Chromium rendert diese offline mit einem
Spitzenpegel von 0,5493 und etwa 1,50 Sekunden Dauer, ohne digitale
Übersteuerung; alle zwölf Audio-/Gain-Knoten werden getrennt. Nach Neuladen
bleiben historische Meldungen still. Dieser Nachweis verwendet keine
Lautsprecher- oder Mikrofonaufnahme und bewertet keine physische Raumlautstärke.

Das aktuelle Archiv liegt unter
`target/packages/sound-17bc3bc/kvmhelm-0.1.0-x86_64-pc-windows-msvc-full.tar.gz`
(76.058.289 Bytes, SHA-256
`f36138d2580bc339b5e4eb8dcf0f232822802abd1fc69a0a5b0c03837dced228`).
Binary: 25.641.984 Bytes, SHA-256
`15f420e2d22d0ee29fe98551f89097c940bb9e4a40eab184b31142894cb7acfe`.
Die statischen CRT-Eigenschaften und optionalen Node-/FFmpeg-Helfer bleiben
wie dokumentiert erhalten.

Der bestehende Testdaemon auf ENVY verwendet seit 09:14:58 UTC dieses
geprüfte Release als PID 64316 mit `index-D-B-jGJQ.js`. Der vorherige
Daemon wurde ohne aktive Kontrolllease geordnet beendet; Daten und Secrets
sind vorher privat gesichert. Lokale und LAN-TLS-Readiness, Web und
authentifizierte API bestehen. Gerätekonfiguration, Pluginregistrierung,
PAT und dauerhafte Tokenidentität/Berechtigungen stimmen mit dem vorherigen
Zustand beziehungsweise der Sicherung überein. Das vorhandene Plugin meldet
`running`, TestLaptop bleibt auf `us`. Bereits geöffnete Browserseiten
benötigen Neuladen für den neuen Ton. Dieser reine UI-Nachtrag wiederholt
nicht den physischen MCP-Lauf aus `87be963` und erzeugt keine neuen
Ressourcen-, Linux-, macOS-, Dienst- oder Containernachweise.

## Pluginprüfung am Release

Am 27. Oktober besteht `scripts/native-plugin-interop.mjs` zusätzlich am
fertigen Release aus `87be963`, ohne Neukompilierung: optionale Node-Runtime,
echte Advanced-IPC-Buffer, Medien-/Geräte-/Berechtigungsgrenzen, Panels und
Formularaktionen, Benachrichtigungen, Ereignisbudget, ungewisse Aktionen ohne
Replay, CLI-Update, fehlende Healthantworten, langsame Empfänger,
Crash-/Broker-Recovery, erzwungenes Stop und Ende des Prozessbaums.
Diese Prüfung verwendet isolierte synthetische Gegenstellen.

Auch die 20 deterministischen Tests des separat gepflegten lokalen
Transkriptionsplugins aus `1c4e1ea` bestehen mit dem paketierten Node 24
(keine ausgelassenen Tests): Unicode-/Phrasentreffer, Wildcards, Deduplication,
Cooldown, begrenzte Historie, PCM-Konvertierung, parallele Quellen,
Start-/Stop-/Fehlerverhalten, lokale Logpersistenz und das Providerprotokoll
gegen einen Loopback-WebSocket. Das Repository wurde nicht verändert.
Das installierte Plugin meldet im Release `running`; das belegt den
Prozesszustand, keine aktive Transkription. Die vollständige aktuelle Kette
Rust → physisches KVM-Zielaudio → echter OpenAI-Provider → Panel/Alarm ist
nicht neu Ende zu Ende geprüft. Frühere Provider-Livetests des Plugins am
TypeScript-Gateway ersetzen diesen Nachweis nicht.

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
  Entsperren, exakte ASCII-Notepad-Eingabe und Mausauswahl am finalen Release
  sichtbar geprüft. Vollständiges US-International mit toten Tasten/Unicode
  ist damit nicht abgenommen. Die reale Transkriptionskette ist oben abgegrenzt.
  Comet-, JetKVM-, Audio- und Mikrofonpfade sind weiterhin ausschließlich mit
  synthetischen Gegenstellen geprüft.
- Linuxnachweise gelten für Ubuntu 24.04/WSL2 x64; das gebaute Paket verlangt
  GLIBC_2.39. Keine Linux-/macOS-Ressourcenmessung. Höhere Auflösungen,
  längere Last und kleine Antwortzeiten auf ruhigem Host brauchen eigene Läufe.

## Freigegebener Dauerbetrieb auf ENVY

Die ausdrückliche Freigabe aus Kommentar 59 umfasst Commit/Push auf `main`
und den dauerhaften ENVY-Betrieb. Das bereits geprüfte Full-Release aus
`17bc3bc` liegt dafür außerhalb von `target` in einem festen Versionsverzeichnis;
alle 482 Paketprüfsummen wurden nach dem Kopieren erneut geprüft.
Eine weitere Kompilierung war nicht erforderlich.

Die Windows-Aufgabe `KVMHelm` startet den Daemon unter dem bisherigen
Datenbesitzer `ENVY\Agent` mit begrenzten Rechten und S4U-Anmeldung in
Session 0. Boot-/Anmeldetrigger und ein unbefristeter Minutentrigger sorgen
für Autostart ohne interaktive Anmeldung und Wiederanlauf; parallele
Instanzen, Laufzeitlimit und Batterieabbruch sind deaktiviert.
Die [Betriebsanleitung](native-packaging.md#dauerbetrieb-auf-envy) beschreibt
Programm-/Datenpfade, Wartung und Wiederaufnahme.

Der vorherige Konsolenprozess endete geordnet mit Exitcode 0 und ohne aktive
Kontrolllease; Daten und Secrets wurden zusammen privat gesichert. Anschließend
bestand ein tatsächlicher Ausfalltest: Der gezielt beendete Aufgabenprozess
`187048` wurde ohne expliziten Start durch PID `170164` ersetzt. Readiness
kehrte nach 4,23 Sekunden zurück, alle fünf vorherigen eigenen Prozesse waren
beendet, beide Listener gehörten genau einem neuen Daemon in Session 0.
Lokale und LAN-TLS-Readiness, Web und authentifizierte API bestehen erneut.
Geräte-/Pluginkonfiguration, dauerhafte Tokenfelder sowie alle sieben
Secretdateien sind unverändert; das vorhandene Plugin meldet `running`.
Der Ausfalltest prüft den automatischen Prozesswiederanlauf; ein Hostneustart
wurde nicht ausgelöst. Windows-SCM bleibt separat ungeprüft.
