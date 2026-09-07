# Gerätetreiber

Windows, macOS und Linux verwenden denselben Node.js-Daemon und dieselben API-/MCP-Verträge. `npm run setup` installiert die gepinnten Abhängigkeiten einschließlich FFmpeg, baut die Anwendung und legt bei fehlender Eigentümer-PAT-Datei einen Token an. Danach startet `npm start` den Server. Ein erneutes Setup erhält Konfiguration und vorhandene Tokens. Es werden keine Geräte angelegt.

Voraussetzung ist Node.js 24.18 oder neuer innerhalb der Version 24 sowie Git für den Checkout. Die vorgefertigten FFmpeg-Binaries unterstützen Windows x64, macOS x64/arm64 und Linux x64/arm64. Für andere Architekturen oder eine eigene FFmpeg-Version kann `KVMHELM_FFMPEG` auf ein ausführbares Binary zeigen. Der Pfad darf Leerzeichen enthalten. Installationsskripte müssen beim Installieren der npm-Abhängigkeiten zugelassen sein. Downloads werden nur bei der Installation benötigt, nicht für den lokalen Gerätebetrieb.

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
