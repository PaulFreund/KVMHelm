# KVMHelm [![Implemented with Codex](https://img.shields.io/badge/Implemented%20with-Codex-6A5ACD?logo=openai&logoColor=white)](https://github.com/openai/codex)

Ein lokaler KVM-Daemon für Menschen und Computer-Use-Clients. Der POC unterstützt **PiKVM v4, JetKVM sowie GL.iNet Comet GL-RM1 und GL-RM10** mit Fokus auf **geringe Computer-Use-Latenz**. Weitere Hardware erhält eigene Adapter; PiKVM-Protokollkompatibilität wird nicht vorausgesetzt.

Der Rust-Dienst enthält HTTP/MCP und eine CLI mit stdio-Bridge, eingebetteter Vue-Oberfläche, Persistenz, Autorisierung, Computer-Use-Core, geteilten Medien, Simulator und PiKVM-/Comet-/JetKVM-Adaptern einschließlich Audio. Bestehende Plugins verwenden einen optionalen Node-IPC-Adapter. [Native Pakete](docs/native-packaging.md) bieten Core-, FFmpeg- und vollständige Helferprofile mit gepinnten Downloads und Notices. Windows- und Linux-Laufzeitprüfungen sowie der [Windows-Laufzeitvergleich](docs/native-performance.md) sind dokumentiert; verbleibende Plattform- und Hardwaregrenzen stehen in der [Prüfmatrix](docs/native-platforms.md). Die [Portierungszuordnung](docs/rust-port.md) beschreibt Verträge und bewusste Abweichungen. Das ersetzte TypeScript-Backend existiert ausschließlich als isolierte, gepinnte Testreferenz aus der Git-Historie.

## Start

Ein natives Paket entpacken und mit `verify.ps1` beziehungsweise `verify.sh`
prüfen; siehe [Pakete und Migration](docs/native-packaging.md). Im Paketverzeichnis:

```sh
./kvmhelm token bootstrap
./kvmhelm serve
```

Unter Windows `./kvmhelm.exe` verwenden. Der Corebetrieb benötigt kein Node/npm.
JetKVM-Video braucht FFmpeg (`media`/`full`); vorhandene Plugins brauchen Node
(`full`). Helfer werden neben dem Binary gefunden oder ausdrücklich über
`KVMHELM_FFMPEG`/`KVMHELM_NODE` gewählt.

Für den Bau aus dem Quellcheckout: Git, Node.js **24.18 oder neuer innerhalb
Version 24**, Rust **1.99.1** sowie C/C++ und CMake gemäß
[Buildanleitung](docs/native-packaging.md). Abhängigkeiten sind in Lockfiles gepinnt.

```sh
git clone https://github.com/PaulFreund/KVMHelm.git
cd KVMHelm
npm run setup
npm start
```

Das Bootstrap schreibt den einmaligen Eigentümer-PAT nach `~/.kvmhelm/secrets/cli.pat`. Den Inhalt im Login von **http://127.0.0.1:8765** eingeben. Der Browser speichert ihn dauerhaft im Local Storage dieser Website. Standardmäßig hat der PAT kein Ablaufdatum; Abmelden entfernt die lokale Kopie, ein expliziter Widerruf sperrt den Token. Bootstrap ist ausschließlich eine lokale Recovery-Funktion; nach Bootstrap einen bereits laufenden Daemon neu starten.

Neue Installationen starten mit einer leeren Geräteliste. Es werden keine Demo-KVMs angelegt.

In der Oberfläche einen **PiKVM v4 Mini/Plus, JetKVM oder GL.iNet Comet** mit administrativ festgelegter LAN-Adresse, Login und DE-/US-Ziellayout anlegen. Selbstsignierte Geräte benötigen eine eigene vertrauenswürdige CA oder den administrativ geprüften SHA-256-Fingerprint des vollständigen Gerätezertifikats. KVMHelm verändert weder PiKVM-Firmware noch USB-/EDID-/Mikrofonkonfiguration.

Die CLI-Beispiele gelten im Paketverzeichnis. Im Quellcheckout stattdessen
`npm run cli -- doctor` beziehungsweise den gewünschten Befehl verwenden.

```sh
./kvmhelm doctor
./kvmhelm device list
./kvmhelm device test DEVICE_ID
./kvmhelm device benchmark DEVICE_ID
```

`--data`, `--secrets`, `--pat-file` und `--url` überschreiben Pfade/Ziel. Alternativ unterstützt die CLI `KVMHELM_DATA`, `KVMHELM_SECRETS`, `KVMHELM_URL`, `KVMHELM_PAT` sowie für Listener `KVMHELM_HOST`, `KVMHELM_PORT`, `KVMHELM_LAN_HOST`, `KVMHELM_LAN_PORT`, `KVMHELM_CERT` und `KVMHELM_KEY`. `npm start` lädt eine vorhandene lokale `.env`. PAT-Werte gehören nicht in Kommandozeilenargumente. Dateien mit Secrets müssen unter Windows per NTFS-ACL auf den Dienstbenutzer beschränkt werden; POSIX-Modi alleine setzen keine Windows-ACL.

## Unterstützte Geräte

| Gerät                     | Treiber        | Eingabe / Video                               |
| ------------------------- | -------------- | --------------------------------------------- |
| PiKVM v4 Mini / Plus      | `pikvm-v4`     | Persistenter KVMD-WebSocket, Snapshot / MJPEG |
| JetKVM                    | `jetkvm`       | Persistenter WebRTC-RPC-Kanal, H.264 → JPEG   |
| GL.iNet Comet GL-RM1      | `glinet-comet` | Lokale KVMD-API des Hersteller-Forks          |
| GL.iNet Comet Pro GL-RM10 | `glinet-comet` | Lokale KVMD-API des Hersteller-Forks          |

JetKVM nutzt natives WebRTC und einen dauerhaften FFmpeg-Prozess für H.264 → JPEG. Native Pakete enthalten den Helfer in den Profilen `media` und `full`. Ein eigenes Binary kann über `KVMHELM_FFMPEG` gewählt werden. PiKVM und Comet benötigen keinen Videodecoder. Details: [Gerätetreiber](docs/device-drivers.md).

## Oberfläche

Monochrome Oberfläche mit Dark- und Light-Mode, kompakte Bedienelemente und unveränderte Originalbilder. Die Reihenfolge lässt sich mit den Pfeilen in der Videowand oder unter Einstellungen ändern; sie wird gemeinsam für Videowand, Tabs und Seitenleiste gespeichert. Quellen und Kachelgröße der Videowand sind einstellbar.

## Entwicklung

```sh
npm run build
npm run build:native
npm run dev
```

Für synthetische Testquellen kann ausdrücklich `npm run cli -- demo` aufgerufen werden. Dies ist ein optionaler Entwicklungsbefehl und kein Installationsschritt.

`npm run build` baut ausschließlich die Vue-Oberfläche. `npm run build:native`
baut das Releasebinary mit eingebetteten Assets; `npm start` und `npm run cli -- …`
verwenden dieses Binary unter `target/release`. `npm run dev` baut und startet
den nativen Debugdienst; nach Rust-Änderungen neu starten. Für Vue-HMR in einem
zweiten Terminal `npx vite` starten und `http://127.0.0.1:5173` verwenden.
Nach UI-Änderungen für den eingebetteten Betrieb zuerst den Browserbundle und
dann Rust neu bauen. `npm start`, `npm run cli` und `npm run setup` laden eine
vorhandene `.env`; direkte Binaryaufrufe und `npm run dev` benötigen exportierte
Variablen oder Flags. npm ist hier ein Entwicklungswerkzeug. Deployed wird das
native Paket ohne `node_modules`.

```sh
npm run check
npx playwright install chromium # nur für die WebRTC-Testgegenstelle
npm test
```

Die nativen Tests prüfen Steuerungsarbitrierung, Replay, Berechtigungen,
Persistenz, Layouts, Netzwerkgrenzen und API-Verträge gegen lokale Gegenstellen.
`npm run test:web` erhält die vier vorhandenen Queue-Regressionen gegen den
aktuellen Browsercode; `npm test` führt sie vor den nativen Tests aus.
`node scripts/ui-layout.mjs` verwendet standardmäßig das native Debugbinary;
`KVMHELM_TEST_BINARY` wählt ein anderes Binary. Die weiteren
`native-*-interop.mjs`-Fixtures prüfen MCP, Browser, WebRTC/Audio und Plugins.
Für Vergleiche und Migration einmal `npm run prepare:reference` ausführen:
Dies baut den echten Originalcommit `b40b2dd` in `target/reference` mit eigener
Installation. `npm run test:reference` führt dort alle 38 Originaltests aus;
`cargo test --locked --test legacy_compat -- --ignored` prüft den Sprachvergleich.
Die Referenz wird niemals automatisch als Produktivfallback gestartet.
Prüfstand und Plattformgrenzen stehen in [verification](docs/verification.md)
und der [Prüfmatrix](docs/native-platforms.md). Die CI beschreibt Windows-,
Linux-, macOS- und Dockerprüfungen; ein konfigurierter Job belegt keinen Lauf.

## Computer Use über MCP

Alle sechs Tools sind über Streamable HTTP und die lokale stdio-Bridge identisch verfügbar: `list_computers`, `open_computer`, `computer`, `computer_screenshot`, `computer_control`, `close_computer`. Der Daemon besitzt Geräteverbindungen und Leases. Die Bridge startet keinen eigenen Core.

Codex-Konfiguration, nach der [offiziellen MCP-Dokumentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli):

```toml
[mcp_servers.kvmhelm]
url = "http://127.0.0.1:8765/mcp"
bearer_token_env_var = "KVMHELM_PAT"
```

Alternativ mit explizitem Secretpfad, insbesondere für GUI-Starts ohne Shell-Umgebung:

```toml
[mcp_servers.kvmhelm]
command = "/absolute/path/kvmhelm"
args = ["mcp", "--transport", "stdio", "--pat-file", "/protected/path/codex.pat"]
```

Unter Windows absolute Windows-Pfade entsprechend TOML-escaped eintragen. Startverzeichnis ist für die Bridge unerheblich. Der laufende Daemon wird über authentifiziertes Loopback-HTTP angesprochen; Named Pipes/Unix-Sockets sind kein zweiter implementierter Zugang.

Ein geeigneter begrenzter PAT:

```sh
./kvmhelm token create --name codex --device DEVICE_ID --scope devices:read --scope video:read --scope input:write --out /protected/path/codex.pat
```

Beobachten → Control anfordern → **erneut beobachten** → kurze Aktionen → beobachten. Pixel beziehen sich auf das gelieferte Bild, nicht auf CSS. `reference` enthält `frame_id`, `view_id`, `input_revision`. `request_id` bei unklarer Antwort niemals mit verändertem Inhalt wiederverwenden. Ein physisches HID-ACK beweist keinen Anwendungserfolg. Bildschirmtexte dürfen keine Berechtigungen oder Administrationsziele festlegen.

Die Integration verwendet eigene MCP-Tools, keinen Austausch des internen Computer-Use-Providers und keine OpenAI-API-Agentenschleife. Eine ausschließlich OAuth unterstützende Hostvariante ist mit diesem PAT-Profil nicht automatisch kompatibel. Reale Codex-/ChatGPT-Bildnutzung muss je konkreter Hostversion geprüft werden.

## Niedrige Latenz

Pro KVM gilt eine konfigurierbare Minimum Action Latency (Standard 30 ms) vor der Bildbeobachtung nach Eingaben. Zusätzliche Pausen erfolgen über `wait`; siehe [Aktions-Timing](docs/action-timing.md).

- Ein dauerhafter PiKVM-HID-WebSocket; keine neue HTTP-Verbindung pro Taste.
- Ganze Batches vorab validiert; pro Eingabe erneut Control-Generation geprüft.
- Vollständige JPEG-/PNG-Frames im RAM; keine temporären Bilddateien. PiKVM/Comet liefern JPEG direkt, JetKVM benötigt H.264-Decoding und JPEG-Encoding.
- Geteilte Snapshot-Anfragen und ein optionaler MJPEG-Stream pro Gerät. Der Benchmark vergleicht den warmen Snapshot-Abruf mit dem Empfang vollständiger Streambilder und wählt mit 20 % Hysterese.
- SQLite auf einem eigenen Thread, Audio/WebRTC in begrenzten nativen Aufgaben, JetKVM-Videodecoding in einem eigenen FFmpeg-Prozess. Browserdarstellung wartet nicht auf Datenbankzugriffe.
- Begrenzte HID-Queue, Verbraucher- und Replaypuffer. Überlast verwirft alte Medienausgaben, keine HID-Aufträge stillschweigend.

Ein Empfangszeitstempel ist kein Hardware-Capture-Beweis. PiKVM-Beobachtungen mit unbekanntem Capture-Zeitpunkt melden `receive_time_only` und nach Aktionen `unverified`. Ein unverändertes Bild bleibt eine gültige neue Beobachtung. Diagnose unterscheidet Signalverlust, Verbindungsfehler und gealtertes Bild.

## Audio und Plugins

PiKVM-Audio wird gegen die tatsächlich gemeldeten Janus-`features` freigegeben. Die Implementierung verwendet lokal WebRTC/Opus, verteilt PCM16 im RAM und hält eine gemeinsame Audioquelle pro Gerät. `from_target` ist unabhängig von der HID-Lease. `to_target` verlangt zusätzlich eine Control-Session und ist auf einen Mikrofon-Sender je Gerät begrenzt. Die Oberfläche startet Mikrofon/Wiedergabe ausschließlich nach Aktion, unterstützt Push-to-talk und beendet Übertragung bei Zielwechsel, Blur und Kontrollverlust.

Das Feature hängt von der [PiKVM-Audiokonfiguration](https://docs.pikvm.org/audio/) ab. Ein nicht über HDMI erfasster Ton wird nicht durch KVMHelm ergänzt. Audio wird separat von Bild/HID als unbestätigter Hardwarepfad in der Abnahmematrix geführt.

Generische, vollständig vertrauenswürdige Plugins werden über `kvmhelm.plugin.json` aus lokalen Verzeichnissen installiert. Sie laufen als separate Prozesse mit eigenem Datenpfad und Gerätefreigaben; Details in [Pluginvertrag](docs/plugin-api.md).

## LAN / Docker

Loopback-HTTP ist die ausdrückliche lokale POC-Ausnahme. LAN-Bindung verlangt TLS:

```sh
./kvmhelm serve --host 192.168.1.10 --cert /tls/server.crt --key /tls/server.key
```

Zusätzliche tatsächlich verwendete Origins mit `--origin https://hostname:8765` freigeben. Browser-Mikrofone benötigen einen vertrauenswürdigen sicheren Kontext. Kein öffentliches Publishing, Relay oder eingebauter TURN-Server.

`Dockerfile` baut einen nativen Nicht-root-Linux-Container mit optionalen FFmpeg-/Node-Helfern; der lokale Containerlauf ist noch nicht abgenommen (siehe [Paketierung](docs/native-packaging.md)). `compose.yaml` benötigt vorbereitete beschreibbare `runtime/data` und `runtime/secrets` für UID 1000 sowie ein vertrauenswürdiges Zertifikat mit `localhost`-SAN in `runtime/tls/server.crt` und `server.key`. Bootstrap im Container vor Start:

```sh
docker compose run --rm kvmhelm token bootstrap
docker compose up -d
```

Die Vorlage veröffentlicht nur auf Host-Loopback. Für bewusst gewünschten LAN-Zugriff Port-Bindung, Zertifikat-SAN und freigegebene Origin gemeinsam konfigurieren. Hardware-HTTP(S), WebSocket und MJPEG verwenden die Geräteports 80/443. Audio benötigt zusätzlich direkte lokale ICE/UDP-Erreichbarkeit zwischen Daemon/Container und PiKVM; Docker Desktop/NAT separat prüfen. Es gibt keinen USB-Passthrough oder privilegierten Container.

## Daten und Betriebsgrenzen

### Screenshots in MCP-Chats

`open_computer`, `computer_screenshot` und bildliefernde `computer`-Aufrufe geben echte MCP-ImageContent-Blöcke zurück. Diese sind mit `audience: ["user", "assistant"]` und Priorität 1 markiert. Serveranweisungen, Screenshot-Toolbeschreibungen und ein zusätzlicher Hinweis in jeder Bildantwort fordern den Agenten auf, vom Nutzer angeforderte Bilder ausdrücklich in seine sichtbare Antwort einzubinden. Die JSON-Metadaten bleiben unverändert im ersten Textblock und in `structuredContent`.

Das sind Darstellungshinweise, keine Garantie: Der MCP-Host entscheidet, ob er Toolbilder anzeigt und welche Bildanhänge eine Antwort unterstützt. Nur eine textliche Beschreibung bedeutet nicht, dass der Nutzer das Bild gesehen hat. Unterstützt der Client keine Darstellung, soll der Agent das offen sagen. Es werden dafür keine öffentlichen Bild-URLs, externen Uploads oder serverseitig gespeicherten Screenshots erzeugt. Maßgeblich ist der [MCP-Annotations-Vertrag](https://modelcontextprotocol.io/specification/2025-11-25/schema#annotations).

SQLite enthält Gerätekonfiguration, Token-Hashes und gemeinsame UI-Präferenzen. AES-256-GCM-Gerätesecrets und der Masterkey liegen außerhalb des Konfigurationsverzeichnisses. Ein Konfigurationsbackup darf den Secretbereich nicht ungeschützt einschließen. Core speichert keine Medien oder Eingabetexte. RAM-/OS-Swap und Speicherung durch einen MCP-Host sind davon unabhängig.

Für ein konsistentes Backup den Daemon stoppen und den Konfigurationsordner sowie separat verschlüsselt den Secretordner sichern. Restore bei gestopptem Daemon mit derselben Version; abgeleitete Sessions und Leases überleben keinen Neustart. Schema v1 wird geprüft, unbekannte neuere Schemata müssen abgewiesen werden. Künftige Schemaänderungen benötigen vorher ein Backup und einen versionierten Migrationspfad.

Keine feste Grenze von vier Geräten. POC-Ressourcenlimits: standardmäßig 128 Sessions insgesamt (`KVMHELM_MAX_SESSIONS`), höchstens 32 je Token, 64 MiB Replaybilder (`KVMHELM_REPLAY_MB`), 8 MiB pro vollständigem Bild, 16 Megapixel, 64 wartende Aufträge pro Gerät, 32 Aktionen/30 Sekunden/4 Bilder pro Batch. Alte Replaybilder dürfen aus dem RAM fallen; der gespeicherte Ausführungsstatus bleibt erhalten und weist auf notwendige neue Beobachtung hin. Journale sind auf 256 IDs pro Session begrenzt; danach neue Session öffnen. Die Weboberfläche wechselt vor Erreichen der Grenze geordnet die Session. Eine Freigabe bei vollem Journal schließt die Session sicher.

**Nicht implementiert:** weitere Herstelleradapter, USB-KVM, Recorder, Power/Reset, Virtual Media, Dateitransfer, Webcam-Emulation. Die zukünftigen Verträge stehen in [Architektur](docs/architecture.md). Eine Nutzungslizenz ist noch nicht festgelegt; die npm-Veröffentlichung ist deaktiviert.

Betrieb und Fehlerbehandlung: [Robustheit und Regressionstests](docs/reliability.md).

Bedienung der verbesserten Browserkonsole: [Tastatur, Maus, Paste und Audio](docs/web-console.md).
