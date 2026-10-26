# Laufzeitvergleich unter Windows

TASK-0126: kontrollierter Vergleich des nativen Releasepakets mit dem
unveränderten TypeScript-Backend. Die Messung verwendet ausschließlich eigene
Loopback-Gegenstellen, synthetische Bilder und Audiotöne. Sie bedient keine
physischen Geräte und verwendet kein echtes Mikrofon.

## Wiederholen

Voraussetzungen: Windows x64, PowerShell 7 (`pwsh.exe`), die vorhandenen
npm-Testabhängigkeiten einschließlich Playwright/Chromium und ein entpacktes,
mit `verify.ps1` geprüftes Full-Paket aus einem sauberen Gitstand.
Der TypeScript-Server wird mit `npm run prepare:reference` aus dem vollständigen
Originalcommit `b40b2dde246533a25a18148a6b59faced676d9ba` in `target/reference`
gebaut. Quellen und kompilierte Ausgaben werden vor Vergleichen auf Änderungen
geprüft; der aktuelle Produktionsquellbaum enthält keinen zweiten Server.

```powershell
npm run prepare:reference
node scripts/check-process-sampler.mjs
node scripts/measure-native-runtime.mjs --binary C:/package/kvmhelm.exe --helpers C:/package --out target/runtime-measurements/my-run --seconds 20 --warmup 5 --rounds 2
```

`--helpers` enthält das manifestierte `package.json`, `node.exe` und
`ffmpeg.exe`. Die Referenz und native Plugins verwenden dasselbe Node-Binary;
beide JetKVM-Pfade dasselbe FFmpeg. Der native Core läuft mit leerem PATH und
startet ohne Plugins kein Node. Alle Daemons erhalten eigene Daten-/Secretpfade,
`NODE_ENV=production` und Standardwerte für Node-/Tokio-Worker.
Geerbte `KVMHELM_*`, Node-Optionen und Workerüberschreibungen werden entfernt.
Die Messdateien enthalten weder PATs, Prozessargumente, Medien noch Eingabetexte.

Der Controller arbeitet sequenziell: Runde 1 Rust, dann TypeScript; Runde 2
TypeScript, dann Rust. Für jede der Gruppen Core, JetKVM und Audio startet ein
frischer Daemon. Innerhalb einer Gruppe laufen die Fälle im selben Prozess;
Allocator-/GC-Rückstände früherer Fälle gehören damit zu den späteren Werten.
Jeder Fall erhält fünf Sekunden Aufwärmlast und ein nominelles 20-Sekunden-
Messfenster. Die tatsächliche Spanne der Ressourcensamples wird mitgespeichert.

`metadata.json` hält Host, Versionen, Binary-/Lockfile-/Harnesshashes und
Einstellungen fest. Einzeldateien enthalten die Szenarien; `results.json` fasst
alle zwölf Gruppenläufe zusammen. Ein unterbrochener Lauf kann im selben
Ausgabeordner nur mit identischen Eingaben fortgesetzt werden. Fehlgeschlagene
Gruppen werden nicht als Ergebnisse übernommen. `target` bleibt lokal und
ignoriert; keine Buildartefakte sind Teil dieses Berichts.

Die gespeicherte Messung unten entstand vor dieser Isolierung. Ihre ursprünglichen
Quell-/Harnesshashes und Messwerte bleiben unverändert. Für eine Wiederholung mit
dem umgestellten Harness einen neuen Ausgabeordner verwenden.

## Last und Zähler

| Fall             | Last im gemessenen Daemon und seinen Helfern                                                                                             |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| Idle             | Leere Installation, kein Gerät, keine API-Last                                                                                           |
| Aktives KVMD     | Ein PiKVM-Adapter, Profil `active` (15 fps), Controlsession                                                                              |
| Videowand        | Vier PiKVM-Adapter, Profil `overview` (je 5 fps), vier gesunde MJPEG-Leser, eine Control- und drei Beobachtungssessions                  |
| Langsame Leser   | Ein Adapter mit 5 fps, ein gesunder und acht nach den HTTP-Headern pausierte MJPEG-Leser                                                 |
| Video-Plugin     | Ein aktiver Adapter, tatsächliches Plugin mit geprüften Advanced-IPC-Video-`Buffer`-Nachrichten                                          |
| JetKVM           | Chromium-WebRTC-Gegenstelle mit 640×480-Canvas/H.264 bei 30 fps, FFmpeg-JPEG, ein MJPEG-Leser; eingehender Audiotrack ohne Audioabnehmer |
| Audio empfangen  | Janus/WebRTC/Opus, erzeugtes Stereo-Zielaudio bei 48 kHz und PCM-WebSocket-Abnehmer                                                      |
| Audio + Mikrofon | Zusätzlich erzeugtes Mono-PCM Richtung Ziel; Gegenstelle prüft tatsächlich nichtstilles Opus                                             |
| Audio-Plugin     | Zielaudio und tatsächliches Audio-`Buffer`-Plugin einschließlich Broker/Kindprozess                                                      |

Die Corefälle liefern immer dasselbe vorab kodierte 1280×720-JPEG. Sein Hash
wird bei Sessioneröffnung geprüft; der Controller verlangt identische
Fixturemetadaten in allen vier Coreläufen. Dadurch werden unterschiedliche
Simulatorrenderer nicht miteinander verglichen. Die vier Videowandgeräte teilen
die kontrollierte HTTP-Gegenstelle, besitzen aber getrennte Adapter/Medienquellen.
Es wird kein vorgeschalteter MJPEG-Stream bereitgestellt: die Quelle liefert
Snapshots. Dies misst den Snapshot-/Verteilungspfad, keine Kamera oder LAN-Latenz.
Die Profilraten sind Sollwerte der Hintergrundquelle; frische API-Abfragen
können zusätzliche Captures und MJPEG-Bilder auslösen.
Die JetKVM-Canvas wechselt zwischen zwei einfarbigen Flächen; dieser kleine,
gut komprimierbare Videostrom vertritt keine bewegte hochauflösende Desktoplast.
Audiopakete werden nominell alle 20 ms erzeugt; die Hostplanung beeinflusst
deren tatsächliche Rate. Mikrofonpaketzähler ab Aktivierung und die auf 128
Blöcke begrenzte PCM-Empfangsliste sind Funktionsbelege, keine Messung von
Audioverzögerung, Paketverlust oder exakt gleicher RTP-Datenrate.

Während aller aktiven Core-/JetKVM-Fälle läuft dieselbe Folge mit höchstens vier
Toolaufrufen pro Sekunde: frisches Screenshot, bis fünf Sekunden gecachtes
Screenshot, Klick mit Abschlussbild. Klicks erhalten frische Referenzen und
einmalige Request-IDs. Die Mindestaktionslatenz ist null. Bei Audio laufen vier
Screenshotaufrufe pro Sekunde gegen ein kleines Fixturebild; es werden keine
Bildgrößen zwischen Audio und Core verglichen. Langsame Antworten erzeugen
keine nachträglichen Lastbursts; verpasste Takte werden gezählt.

Latenz ist die vollständige Loopback-HTTP-Antwort einschließlich JSON-Parsing
im Testclient, gemessen vor zusätzlicher Bildprüfung. Sie enthält weder
Bildschirmdarstellung, LLM-Verarbeitung noch physische Eingabe-/Kameralatenz.
Perzentile verwenden den nächsten Rang, ohne Interpolation.

Der PowerShell-Sampler ermittelt jede Sekunde die Nachkommen des gestarteten
Daemons und liest deren Prozesszähler. FFmpeg, nativer Node-Broker,
Pluginprozesse und beobachtete Windows-`conhost`-Kinder werden mitgezählt;
Testclient, Sampler und Chromium-Gegenstelle gehören nicht zum Anwendungssaldo.
Der Sampler liest keine fremden Kommandozeilen oder Umgebungen. Sein Selbsttest
prüft drei Generationen eigener Prozesse, bewusst belegten Speicher und CPU-Last.

- Arbeitssatz: Summe `WorkingSet64`, resident einschließlich geteilter Seiten;
  gemeinsam geladene Bibliotheksseiten können mehrfach zählen.
- Privater Commit: Summe `PrivateMemorySize64`, zugesicherter privater Speicher;
  dies ist kein privater residenter RAM und kein Rust-Heapzähler.
- CPU: beobachtete Differenz der gesamten Prozess-CPU-Zeiten geteilt durch die
  reale Samplezeit. **100 % entspricht einem logischen Kern**, nicht dem
  gesamten Achtkernhost. Sehr kurze Prozesse zwischen Samples können fehlen.
- Host-CPU: Differenzen von `GetSystemTimes`, auf alle logischen Kerne normiert,
  einschließlich Gegenstelle, Sampler und anderer Hostarbeit. Sie beschreibt
  das Messumfeld, keine dem Daemon zurechenbare Last.

Die erste Ressourcenerfassung startet die gemessene API-Last; CIM-/Startkosten
machen die Ressourcenspanne geringfügig kürzer als das nominelle Lastfenster.
Kurze Fenster mit zwei Wiederholungen sind keine Dauertests, Lecknachweise oder
statistischen Konfidenzintervalle. Der Host ist nicht exklusiv; Hintergrundlast,
Browserplanung und GC können besonders p95 und sehr kleine CPU-Werte verändern.
Eingabeantworten behalten Bilder im bestehenden Request-Replaycache (Standard
64 MiB je Core). Speicheranstieg während der Klickfolge enthält deshalb auch
diesen vorgesehenen Cacheaufbau und darf nicht allein als Leck gewertet werden.

## Gemessener Stand

Messhost am 26.10.2026: ENVY, Windows 11 Pro 10.0.26200 x64,
AMD Ryzen 7 4700U mit acht logischen CPUs, 16.522.207.232 Bytes RAM.
Rust 1.99.1, optimiertes Windows-MSVC-Full-Paket mit statischer CRT,
Thin-LTO und einer Codegen-Einheit.

| Eingang               | Version / SHA-256                                                                 |
| --------------------- | --------------------------------------------------------------------------------- |
| Nativer Quellstand    | `24ae930be3cdfbb0badc68a4cb7fc1e7339e2b48`, Paketmanifest `dirty=false`           |
| Native EXE            | `48f4cace07f9dce3a4fb6731ec7afa0d23b6529934467b16943024d1ee88fa5e`                |
| TypeScript-Quellstand | `b40b2dde246533a25a18148a6b59faced676d9ba`                                        |
| Gebaute Referenz-CLI  | `db6f6c56ed4dfc3270ccb54799108f8e5797c06d59b1a63d95f9f9a40dba2206`                |
| npm-Lockfile          | `bde30bf9b2d9ac483a0f32f431daff66926e3ec3181010c5492fe92c11bb3c75`                |
| Node 24.18.0          | `9a4eb5f1c29c6a2e93852ead46b999e284a6a5ca8bab4d4e241d587d025a52de`                |
| FFmpeg 6.1.1          | `04e1307997530f9cf2fe35cba2ca7e8875ca91da02f89d6c7243df819c94ad00`                |
| Core-JPEG             | 528.067 Bytes, `4d0afa43b872f88a8d83ed8cacaea17a5f44ffc1bdf8ebff323b670217e8b8b6` |

Die vollständige Reihe endete am 26.10.2026 um 00:45 UTC. Alle zwölf
Gruppenläufe beziehungsweise 36 Lastfenster waren erfolgreich. Die erfassten
Ressourcenspannen betrugen 19,594–19,972 Sekunden, die mittlere Host-CPU je
Fenster 28,6–50,3 %. Alle 2.562 Toolaufrufe waren erfolgreich; kein Lasttakt
wurde verpasst. Der kompakte [Zahlenbeleg](measurements/windows-20261026.json)
enthält alle 36 Einzelwerte, Spitzen, privaten Commit, Prozesszahlen,
Latenzverteilungen, Aktivitätsbelege, Provenienz und den Hash der lokalen
vollständigen Ergebnisdatei. Er enthält keine Medien oder Buildartefakte.

Die folgenden Spannen sind Minimum/Maximum der **zwei Fenstermittelwerte**
je Backend, keine Speichergrenzen oder Konfidenzintervalle. MiB = 1.048.576
Bytes; CPU 100 % = ein logischer Kern.

| Fall               | Rust Arbeitssatz MiB | TS Arbeitssatz MiB | Rust CPU % |    TS CPU % |
| ------------------ | -------------------: | -----------------: | ---------: | ----------: |
| Idle               |                 16,8 |        105,9–107,4 |  0,08–0,63 |        0,63 |
| Aktives KVMD       |            30,9–31,2 |        155,8–157,8 |  1,42–2,68 | 17,69–20,01 |
| Videowand (4)      |            38,4–38,9 |        280,2–289,8 |  2,52–2,75 | 30,09–31,24 |
| Langsame Leser (8) |            38,2–38,3 |        218,9–240,0 |  0,86–1,02 | 11,22–14,57 |
| Video-Plugin       |          122,0–122,3 |        300,0–305,6 |  0,87–1,49 | 10,98–13,43 |
| JetKVM + FFmpeg    |            70,7–70,8 |        158,8–177,1 |  5,82–7,19 |  9,27–12,99 |
| Audio empfangen    |            24,1–24,3 |        152,6–152,7 |  1,18–1,34 |   2,27–3,07 |
| Audio + Mikrofon   |            24,7–24,9 |        156,3–156,9 |  2,23–2,60 |   4,58–5,76 |
| Audio-Plugin       |          114,3–114,6 |        213,7–214,5 |  1,42–1,58 |   2,76–3,70 |

| Fall               | Rust privater Commit MiB | TS privater Commit MiB |
| ------------------ | -----------------------: | ---------------------: |
| Idle               |                      3,2 |            109,7–110,7 |
| Aktives KVMD       |                15,4–15,8 |            192,2–193,7 |
| Videowand (4)      |                23,5–24,3 |            399,5–424,0 |
| Langsame Leser (8) |                22,5–22,8 |            300,2–301,1 |
| Video-Plugin       |                71,0–71,6 |            319,2–330,4 |
| JetKVM + FFmpeg    |                48,2–48,4 |            191,9–196,4 |
| Audio empfangen    |                  4,7–4,9 |            177,6–178,1 |
| Audio + Mikrofon   |                  5,1–5,4 |            179,4–180,0 |
| Audio-Plugin       |                54,5–55,3 |            205,1–206,2 |

Ohne Codec/Plugins wurden jeweils zwei Prozesse beobachtet (Daemon und
`conhost`), bei JetKVM jeweils vier einschließlich FFmpeg/`conhost`.
Pluginfälle umfassen nativ fünf Prozesse einschließlich Node-Broker und
Plugin, in der Referenz drei einschließlich Plugin. Die optionalen Helfer
erhöhen den nativen Arbeitssatz deutlich und sind in allen Zahlen enthalten.
Die gesunden MJPEG-Leser blieben aktiv, auch neben acht pausierten Lesern;
alle Core-Antwortbilder waren 528.067 Bytes groß. Audio-Plugins erhielten
tatsächliche `Buffer`; der synthetische Mikrofonpfad lieferte nichtstilles Opus.

Die Latenzspannen unten sind Minimum/Maximum der **je Fenster separat
berechneten p50 beziehungsweise p95**, jeweils in Millisekunden. N zählt
die tatsächlichen Aufrufe über beide Fenster, Rust/TypeScript. Die
Perzentile werden nicht aus bereits aggregierten Perzentilen neu berechnet.

| Fall               | Aufruf         |     Rust p50 / p95 ms |       TS p50 / p95 ms | N Rust / TS |
| ------------------ | -------------- | --------------------: | --------------------: | ----------: |
| Aktives KVMD       | Frisches Bild  | 26,5–27,5 / 39,0–50,4 | 30,4–31,3 / 43,7–45,4 |     54 / 54 |
| Aktives KVMD       | Gecachtes Bild | 24,1–24,7 / 35,9–36,8 | 26,8–27,4 / 39,3–54,5 |     54 / 54 |
| Aktives KVMD       | Klick + Bild   | 23,1–27,5 / 35,8–55,0 | 30,0–37,1 / 45,5–64,6 |     52 / 52 |
| Videowand (4)      | Frisches Bild  | 25,1–26,0 / 40,8–40,9 | 30,3–31,8 / 43,3–49,3 |     54 / 54 |
| Videowand (4)      | Gecachtes Bild | 23,1–23,5 / 26,4–36,0 | 20,6–24,6 / 35,4–41,4 |     54 / 54 |
| Videowand (4)      | Klick + Bild   | 25,4–26,1 / 41,2–43,5 | 31,1–33,0 / 52,8–60,1 |     52 / 52 |
| Langsame Leser (8) | Frisches Bild  | 24,8–25,3 / 37,8–50,4 |      30,7 / 46,0–56,6 |     54 / 54 |
| Langsame Leser (8) | Gecachtes Bild | 23,4–24,0 / 34,4–45,9 | 26,4–27,0 / 40,5–51,3 |     54 / 54 |
| Langsame Leser (8) | Klick + Bild   | 24,8–25,8 / 35,5–44,2 | 30,6–31,7 / 42,1–55,8 |     52 / 52 |
| Video-Plugin       | Frisches Bild  | 25,5–25,7 / 45,2–52,6 | 30,1–30,9 / 53,3–56,9 |     54 / 54 |
| Video-Plugin       | Gecachtes Bild | 23,2–23,3 / 35,4–38,8 | 25,3–25,5 / 37,7–37,8 |     54 / 54 |
| Video-Plugin       | Klick + Bild   | 26,0–26,7 / 40,6–52,4 | 29,6–32,6 / 53,6–56,8 |     52 / 52 |
| JetKVM + FFmpeg    | Frisches Bild  | 25,6–37,9 / 40,6–49,1 | 37,8–40,2 / 44,3–53,4 |     54 / 54 |
| JetKVM + FFmpeg    | Gecachtes Bild | 17,2–17,7 / 19,6–22,0 | 18,2–18,4 / 19,9–20,2 |     54 / 54 |
| JetKVM + FFmpeg    | Klick + Bild   | 38,3–53,7 / 58,2–86,2 | 54,1–55,4 / 73,5–89,9 |     53 / 52 |
| Audio empfangen    | Screenshot     | 16,7–18,4 / 22,9–24,0 |  7,3–12,0 / 22,8–24,8 |   160 / 160 |
| Audio + Mikrofon   | Screenshot     |  7,2–16,2 / 21,2–24,1 |   8,5–8,7 / 22,1–22,9 |   160 / 160 |
| Audio-Plugin       | Screenshot     |  5,6–11,4 / 19,7–20,6 | 12,0–19,3 / 21,8–24,2 |   161 / 160 |

Der native Arbeitssatz und private Commit sind in allen gemessenen Fällen
niedriger. Unter aktiver Last ist auch die beobachtete Prozess-CPU niedriger;
Idle schwankt bei sehr kleinen Werten. **Eine durchgehende Latenzverbesserung
ist nicht belegt.** Beispielsweise sind die Audio-Screenshot-Mediane beim
reinen Empfang nativ höher, und mehrere p95-Spannen überschneiden sich.
Größere Bildantworten und Klicks fallen meist zugunsten Rust aus; das erlaubt
bei dieser Hintergrundlast und zwei kurzen Wiederholungen keine allgemeine
Latenzgarantie. Ein ruhiger Host und längere, gezielte Läufe bleiben für die
Einordnung kleiner Antwortzeiten sinnvoll.

Zusätzlich bestanden nach den Messungen erneut die vollständigen normalen
JetKVM- und Janus-/Vue-Audioregressionen mit diesem Releasebinary und dem
Audio-Plugin. Ihr normaler Modus behält Probe-Retry, Fehler-/Reconnectfälle,
Opus/PCM, USB-Reports, Request-Replay/Unknown, Mikrofon-/Fokus-/Kontrollverlust
und Bereinigung bei. Der Messmodus verwendet gesunde Gegenstellen und ersetzt
diese Fehlerprüfungen nicht. Der Prozesssampler-Selbsttest und das erneute
Kompilieren der unveränderten TypeScript-Referenz bestanden ebenfalls.

Linux/macOS, reale Dienste, Container, physische Geräte, andere Bildinhalte,
höhere Auflösungen und längere Laufzeiten brauchen eigene Messungen. Ein
Windows-Funktions-/Ressourcennachweis wird nicht auf diese Grenzen übertragen.
