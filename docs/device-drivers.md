# Gerätetreiber

Der Rust-Daemon erhält dieselben API-/MCP-Verträge für Windows, Linux und macOS. Tatsächlich geprüfte Hosts und verbleibende Grenzen stehen in der [Plattformmatrix](native-platforms.md). Der native Core samt eingebetteter Oberfläche benötigt kein Node/npm. Im Quellcheckout installiert `npm run setup` die gepinnten Buildabhängigkeiten, baut Vue und Rust und legt bei fehlender Eigentümer-PAT-Datei einen Token an. Danach startet `npm start` das native Releasebinary. Ein erneutes Setup erhält Konfiguration und vorhandene Tokens. Es werden keine Geräte angelegt.

Der Quellbau benötigt Git, Node.js 24.18 oder neuer innerhalb Version 24, Rust 1.99.1 und die dokumentierten C/C++-/CMake-Werkzeuge. JetKVM-Video braucht zusätzlich FFmpeg. Die [nativen Paketprofile](native-packaging.md) `media` und `full` enthalten einen gepinnten Helfer für Windows x64, Linux x64 beziehungsweise macOS x64/arm64; `full` ergänzt Node ausschließlich für bestehende Plugins. Ein eigenes FFmpeg kann über `KVMHELM_FFMPEG` gewählt werden; der Pfad darf Leerzeichen enthalten. Downloads gehören zum Paket-/Buildvorgang, nicht zum lokalen Gerätebetrieb. Das normale npm-Setup installiert keinen FFmpeg-Helfer mehr.

## JetKVM

`driver_id: "jetkvm"`, Modell `JetKVM`, Adresse der lokalen Weboberfläche und deren Passwort. Geräte ohne aktiviertes Passwort werden ebenfalls unterstützt; ein Benutzername ist für dieses Protokoll nicht erforderlich. Das initiale Hersteller-Setup muss abgeschlossen sein.

Der Adapter meldet sich über `/auth/login-local` an und verwendet das Sitzungscookie auch beim Wiederverbinden. Signaling läuft über `/webrtc/signaling/client`, Tastatur und Maus über einen dauerhaften, geordneten WebRTC-DataChannel mit den nativen JSON-RPC-HID-Reports. Die lokalen ICE-Verbindungen benötigen weder Cloud noch STUN/TURN. Eine zusätzliche Verbindung über die Herstelleroberfläche kann die aktive Gerätesitzung verdrängen.

H.264-RTP wird im Speicher zusammengesetzt und über Pipes an einen persistenten FFmpeg-Prozess je verbundenem JetKVM gegeben. Dieser erzeugt JPEG-Bilder für die gemeinsamen Medienverträge. Es gibt keine Bilddateien und keinen Browserprozess im Server. Alte Medienausgaben werden bei Überlast verworfen; Paketverluste fordern ein neues Schlüsselbild an. Beobachtungen warten auf ein neu eintreffendes Bild. Die Minimum Action Latency gilt weiter geräteübergreifend über den gemeinsamen Core.

Eingehendes Opus-Audio wird angeboten, wenn das Gerät eine entsprechende Spur meldet. Mikrofonübertragung zum JetKVM ist nicht implementiert und wird als nicht verfügbar gemeldet.

Protokollbasis: [JetKVM-Firmware](https://github.com/jetkvm/kvm/tree/d654a5ae1a45fbc8dc1f670dd4b70a927eece5bd), insbesondere `web.go`, `cloud.go`, `webrtc.go` und `jsonrpc.go`.

## GL.iNet Comet

`driver_id: "glinet-comet"`, Modelle `GL-RM1 (Comet)` und `GL-RM10 (Comet Pro)`. Adresse, lokaler Benutzername (Standard `admin`) und Passwort werden im Gerätedialog hinterlegt.

Der Hersteller-Fork stellt die lokale KVMD-Schnittstelle bereit: authentifiziertes HTTP mit `X-KVMD-User` / `X-KVMD-Passwd`, persistenter HID-WebSocket sowie Snapshot/MJPEG. Der gemeinsame KVMD-Transport wird wiederverwendet; der eigene Treiber und Modellvertrag erlauben spätere herstellerspezifische Erweiterungen. Verfügbarkeit von HID, Video und Audio wird vom Gerät ermittelt. Eine Firmware ohne die erforderlichen lokalen Endpunkte meldet `UNSUPPORTED_FIRMWARE`; KVMHelm verändert keine Gerätekonfiguration.

Protokollbasis: [GL.iNet-KVMD-Fork](https://github.com/gl-inet/glkvm/tree/3e8dd23c4bd638a4650433664cc3b0f3b4d29395) und [Herstellerdokumentation](https://docs.gl-inet.com/kvm/en/).

## Prüfung und Erweiterung

Die lokalen Protokolltests decken beide Comet-Modelle ab. Der JetKVM-Test verwendet einen Chromium-Peer ausschließlich als Testgegenstelle für echtes H.264/Opus-WebRTC, HID und Wiederverbindung. Dafür einmal `npx playwright install chromium` ausführen. Installation und Betrieb der Anwendung benötigen Chromium nicht.

Die Implementierung ist plattformübergreifend ausgelegt; lokal ausgeführt wurden die Tests unter Windows. Physische JetKVM-/Comet-Geräte sowie macOS/Linux wurden hier nicht geprüft. Neue Hersteller erhalten eigene `driver_id`-Adapter; Fähigkeiten und optionale Erweiterungen werden zur Laufzeit gemeldet, ohne die gemeinsamen Computer-Use-Tools zu ändern.
