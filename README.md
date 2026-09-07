# KVMHelm [![Implemented with Codex](https://img.shields.io/badge/Implemented%20with-Codex-6A5ACD?logo=openai&logoColor=white)](https://github.com/openai/codex)

Ein lokaler KVM-Daemon für Menschen und Computer-Use-Clients. Der POC unterstützt **PiKVM v4, JetKVM sowie GL.iNet Comet GL-RM1 und GL-RM10** mit Fokus auf **geringe Computer-Use-Latenz**. Weitere Hardware erhält eigene Adapter; PiKVM-Protokollkompatibilität wird nicht vorausgesetzt.

## Start

Windows, macOS und Linux: Voraussetzung sind Git und Node.js **24.18 oder neuer innerhalb Version 24**. Abhängigkeiten sind im Lockfile gepinnt.

```sh
git clone https://github.com/PaulFreund/KVMHelm.git
cd KVMHelm
npm run setup
npm start
```

Das Bootstrap schreibt den einmaligen Eigentümer-PAT nach `~/.kvmhelm/secrets/cli.pat`. Den Inhalt im Login von **http://127.0.0.1:8765** eingeben. Der Browser speichert ihn dauerhaft im Local Storage dieser Website. Standardmäßig hat der PAT kein Ablaufdatum; Abmelden entfernt die lokale Kopie, ein expliziter Widerruf sperrt den Token. Bootstrap ist ausschließlich eine lokale Recovery-Funktion; nach Bootstrap einen bereits laufenden Daemon neu starten.

Neue Installationen starten mit einer leeren Geräteliste. Es werden keine Demo-KVMs angelegt.

In der Oberfläche einen **PiKVM v4 Mini/Plus, JetKVM oder GL.iNet Comet** mit administrativ festgelegter LAN-Adresse, Login und DE-/US-Ziellayout anlegen. Selbstsignierte Geräte benötigen eine eigene vertrauenswürdige CA oder den administrativ geprüften SHA-256-Fingerprint des vollständigen Gerätezertifikats. KVMHelm verändert weder PiKVM-Firmware noch USB-/EDID-/Mikrofonkonfiguration.

```sh
node dist/server/cli.js doctor
node dist/server/cli.js device list
node dist/server/cli.js device test DEVICE_ID
node dist/server/cli.js device benchmark DEVICE_ID
```

`--data`, `--secrets`, `--pat-file` und `--url` überschreiben Pfade/Ziel. Alternativ `KVMHELM_DATA`, `KVMHELM_SECRETS`, `KVMHELM_URL`, `KVMHELM_PAT`. PAT-Werte gehören nicht in Kommandozeilenargumente. Dateien mit Secrets müssen unter Windows per NTFS-ACL auf den Dienstbenutzer beschränkt werden; POSIX-Modi alleine setzen keine Windows-ACL.

## Unterstützte Geräte

| Gerät | Treiber | Eingabe / Video |
|---|---|---|
| PiKVM v4 Mini / Plus | `pikvm-v4` | Persistenter KVMD-WebSocket, Snapshot / MJPEG |
| JetKVM | `jetkvm` | Persistenter WebRTC-RPC-Kanal, H.264 → JPEG |
| GL.iNet Comet GL-RM1 | `glinet-comet` | Lokale KVMD-API des Hersteller-Forks |
| GL.iNet Comet Pro GL-RM10 | `glinet-comet` | Lokale KVMD-API des Hersteller-Forks |

JetKVM nutzt natives WebRTC und einen dauerhaften FFmpeg-Prozess für H.264 → JPEG. Das passende FFmpeg-Binary wird bei der Installation automatisch bereitgestellt. Ein eigenes Binary kann über `KVMHELM_FFMPEG` gewählt werden. PiKVM und Comet benötigen keinen Videodecoder. Details: [Gerätetreiber](docs/device-drivers.md).

## Oberfläche

Monochrome Oberfläche mit Dark- und Light-Mode, kompakte Bedienelemente und unveränderte Originalbilder. Die Reihenfolge lässt sich mit den Pfeilen in der Videowand oder unter Einstellungen ändern; sie wird gemeinsam für Videowand, Tabs und Seitenleiste gespeichert. Quellen und Kachelgröße der Videowand sind einstellbar.

## Entwicklung

```sh
npm run build
npm run dev
```

Für synthetische Testquellen kann ausdrücklich `node dist/server/cli.js demo` aufgerufen werden. Dies ist ein optionaler Entwicklungsbefehl und kein Installationsschritt.

Der Backend-Entwicklungsstart lädt TypeScript; die Worker verwenden die zuletzt gebauten JS-Dateien. Nach Workeränderungen erneut bauen. Für Vue-HMR in einem zweiten Terminal `npx vite` starten und `http://127.0.0.1:5173` verwenden. Produktionsstart liefert ausschließlich den vorab gebauten Browserbundle aus.

```sh
npm run check
npx playwright install chromium # nur für die WebRTC-Testgegenstelle
npm test
```

Die fokussierten Tests prüfen Steuerungsarbitrierung, Wiederholungen, ungültige Ansichten, Berechtigungen, Präferenzrevisionen, Layouts, beide MCP-Transporte und den PiKVM-Treiber gegen einen lokalen Protokollserver. Nach lokalem Bootstrap/Demo/Dienststart lässt sich der Browsercheck mit `node scripts/ui-smoke.mjs` wiederholen; dafür einmal `npx playwright install chromium` ausführen. Der Prüfstand steht in `docs/verification.md`. Der isolierte Check `node scripts/ui-layout.mjs` prüft eine leere Installation, Reihenfolge, Farbschemata und dauerhafte Browseranmeldung. Eine CI-Vorlage liegt unter `.github/ci-template.yml`; für automatische GitHub Actions muss sie mit einer workflow-berechtigten Anmeldung nach `.github/workflows/ci.yml` verschoben werden.

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
command = "node"
args = ["/absolute/path/KVMHelm/dist/server/cli.js", "mcp", "--transport", "stdio", "--pat-file", "/protected/path/codex.pat"]
```

Unter Windows absolute Windows-Pfade entsprechend TOML-escaped eintragen. Startverzeichnis ist für die Bridge unerheblich. Der laufende Daemon wird über authentifiziertes Loopback-HTTP angesprochen; Named Pipes/Unix-Sockets sind kein zweiter implementierter Zugang.

Ein geeigneter begrenzter PAT:

```sh
node dist/server/cli.js token create --name codex --device DEVICE_ID --scope devices:read --scope video:read --scope input:write --out /protected/path/codex.pat
```

Beobachten → Control anfordern → **erneut beobachten** → kurze Aktionen → beobachten. Pixel beziehen sich auf das gelieferte Bild, nicht auf CSS. `reference` enthält `frame_id`, `view_id`, `input_revision`. `request_id` bei unklarer Antwort niemals mit verändertem Inhalt wiederverwenden. Ein physisches HID-ACK beweist keinen Anwendungserfolg. Bildschirmtexte dürfen keine Berechtigungen oder Administrationsziele festlegen.

Die Integration verwendet eigene MCP-Tools, keinen Austausch des internen Computer-Use-Providers und keine OpenAI-API-Agentenschleife. Eine ausschließlich OAuth unterstützende Hostvariante ist mit diesem PAT-Profil nicht automatisch kompatibel. Reale Codex-/ChatGPT-Bildnutzung muss je konkreter Hostversion geprüft werden.

## Niedrige Latenz

Pro KVM gilt eine konfigurierbare Minimum Action Latency (Standard 30 ms) vor der Bildbeobachtung nach Eingaben. Zusätzliche Pausen erfolgen über `wait`; siehe [Aktions-Timing](docs/action-timing.md).

- Ein dauerhafter PiKVM-HID-WebSocket; keine neue HTTP-Verbindung pro Taste.
- Ganze Batches vorab validiert; pro Eingabe erneut Control-Generation geprüft.
- Vollständige JPEG-/PNG-Frames im RAM; keine temporären Bilddateien. PiKVM/Comet liefern JPEG direkt, JetKVM benötigt H.264-Decoding und JPEG-Encoding.
- Geteilte Snapshot-Anfragen und ein optionaler MJPEG-Stream pro Gerät. Der Benchmark vergleicht den warmen Snapshot-Abruf mit dem Empfang vollständiger Streambilder und wählt mit 20 % Hysterese.
- SQLite und PiKVM-Audio in eigenen Workern, JetKVM-Videodecoding in einem eigenen FFmpeg-Prozess. Browserdarstellung wartet nicht auf Datenbankzugriffe.
- Begrenzte HID-Queue, Verbraucher- und Replaypuffer. Überlast verwirft alte Medienausgaben, keine HID-Aufträge stillschweigend.

Ein Empfangszeitstempel ist kein Hardware-Capture-Beweis. PiKVM-Beobachtungen mit unbekanntem Capture-Zeitpunkt melden `receive_time_only` und nach Aktionen `unverified`. Ein unverändertes Bild bleibt eine gültige neue Beobachtung. Diagnose unterscheidet Signalverlust, Verbindungsfehler und gealtertes Bild.

## Audio und Plugins

PiKVM-Audio wird gegen die tatsächlich gemeldeten Janus-`features` freigegeben. Die Implementierung verwendet lokal WebRTC/Opus, verteilt PCM16 im RAM und hält eine gemeinsame Audioquelle pro Gerät. `from_target` ist unabhängig von der HID-Lease. `to_target` verlangt zusätzlich eine Control-Session und ist auf einen Mikrofon-Sender je Gerät begrenzt. Die Oberfläche startet Mikrofon/Wiedergabe ausschließlich nach Aktion, unterstützt Push-to-talk und beendet Übertragung bei Zielwechsel, Blur und Kontrollverlust.

Das Feature hängt von der [PiKVM-Audiokonfiguration](https://docs.pikvm.org/audio/) ab. Ein nicht über HDMI erfasster Ton wird nicht durch KVMHelm ergänzt. Audio wird separat von Bild/HID als unbestätigter Hardwarepfad in der Abnahmematrix geführt.

Generische, vollständig vertrauenswürdige Plugins werden über `kvmhelm.plugin.json` aus lokalen Verzeichnissen installiert. Sie laufen als separate Prozesse mit eigenem Datenpfad und Gerätefreigaben; Details in [Pluginvertrag](docs/plugin-api.md).

## LAN / Docker

Loopback-HTTP ist die ausdrückliche lokale POC-Ausnahme. LAN-Bindung verlangt TLS:

```sh
node dist/server/cli.js serve --host 192.168.1.10 --cert /tls/server.crt --key /tls/server.key
```

Zusätzliche tatsächlich verwendete Origins mit `--origin https://hostname:8765` freigeben. Browser-Mikrofone benötigen einen vertrauenswürdigen sicheren Kontext. Kein öffentliches Publishing, Relay oder eingebauter TURN-Server.

`Dockerfile` baut einen Nicht-root-Linux-Container. `compose.yaml` benötigt vorbereitete beschreibbare `runtime/data` und `runtime/secrets` für UID 1000 sowie ein vertrauenswürdiges Zertifikat mit `localhost`-SAN in `runtime/tls/server.crt` und `server.key`. Bootstrap im Container vor Start:

```sh
docker compose run --rm kvmhelm token bootstrap
docker compose up -d
```

Die Vorlage veröffentlicht nur auf Host-Loopback. Für bewusst gewünschten LAN-Zugriff Port-Bindung, Zertifikat-SAN und freigegebene Origin gemeinsam konfigurieren. Hardware-HTTP(S), WebSocket und MJPEG verwenden die Geräteports 80/443. Audio benötigt zusätzlich direkte lokale ICE/UDP-Erreichbarkeit zwischen Daemon/Container und PiKVM; Docker Desktop/NAT separat prüfen. Es gibt keinen USB-Passthrough oder privilegierten Container.

## Daten und Betriebsgrenzen

SQLite enthält Gerätekonfiguration, Token-Hashes und gemeinsame UI-Präferenzen. AES-256-GCM-Gerätesecrets und der Masterkey liegen außerhalb des Konfigurationsverzeichnisses. Ein Konfigurationsbackup darf den Secretbereich nicht ungeschützt einschließen. Core speichert keine Medien oder Eingabetexte. RAM-/OS-Swap und Speicherung durch einen MCP-Host sind davon unabhängig.

Für ein konsistentes Backup den Daemon stoppen und den Konfigurationsordner sowie separat verschlüsselt den Secretordner sichern. Restore bei gestopptem Daemon mit derselben Version; abgeleitete Sessions und Leases überleben keinen Neustart. Schema v1 wird geprüft, unbekannte neuere Schemata müssen abgewiesen werden. Künftige Schemaänderungen benötigen vorher ein Backup und einen versionierten Migrationspfad.

Keine feste Grenze von vier Geräten. POC-Ressourcenlimits: standardmäßig 128 Sessions (`KVMHELM_MAX_SESSIONS`), 64 MiB Replaybilder (`KVMHELM_REPLAY_MB`), 8 MiB pro vollständigem Bild, 16 Megapixel, 64 wartende Aufträge pro Gerät, 32 Aktionen/30 Sekunden/4 Bilder pro Batch. Alte Replaybilder dürfen aus dem RAM fallen; der gespeicherte Ausführungsstatus bleibt erhalten und weist auf notwendige neue Beobachtung hin. Journale sind auf 256 IDs pro Session begrenzt; danach neue Session öffnen.

**Nicht implementiert:** weitere Herstelleradapter, USB-KVM, Recorder, Power/Reset, Virtual Media, Dateitransfer, Webcam-Emulation. Die zukünftigen Verträge stehen in [Architektur](docs/architecture.md). Eine Nutzungslizenz ist noch nicht festgelegt; die npm-Veröffentlichung ist deaktiviert.
