# POC-Prüfstand

Diese Datei trennt ausgeführte synthetische Prüfungen von ausstehenden Hardware-/Hostabnahmen. Der POC ist kein plattformübergreifend abgenommenes Release.

## Ausgeführt am 7. September 2026

Windows, Node.js 24.18.0: Produktionsbuild erfolgreich. Zehn fokussierte Tests erfolgreich, darunter HTTP-/stdio-MCP mit echten PNG-Image-Blöcken und gemeinsamer Lease, PiKVM und beide Comet-Modelle gegen lokale HTTP-/WebSocket-Protokollserver sowie JetKVM mit nativem WebRTC, FFmpeg-Bildern, Audio, HID und Wiederverbindung. Chromium wird für den JetKVM-Test als Gegenstelle verwendet, nicht vom Server. Browserprüfung: Login, Bild, Control-acquire, Klick, Videowand und Wiederherstellung nach Reload erfolgreich; keine JavaScript-Seitenfehler. Screenshots dieser ausschließlich synthetischen QA liegen lokal im ignorierten `.kvmhelm/qa`.

Der geprüfte PiKVM-Quellstand ist `pikvm/kvmd@387846d22fa807f97de09750c32c1c9b26d36c1c`. Das ist ein Protokollreferenzstand, keine behauptete Firmwareversion eines angeschlossenen Geräts.

| Pfad                                      | Prüfverfahren                                                                                   |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------- |
| TypeScript/Vue/Bundle                     | `npm run build`                                                                                 |
| Core/Leases/Request-IDs/Views/Auth/Layout | `npm test`                                                                                      |
| HTTP MCP und stdio                        | Offizieller SDK-Client; echte Image-Content-Blöcke, Action und konkurrierende Lease             |
| Web-UI                                    | Lokaler Browser: Login, Simulatorbild, Kontrolle, Videowand, Präferenzspeicherung               |
| PiKVM                                     | Herstellerprotokoll anhand offizieller Dokumentation/Quellcode; reale v4 Mini/Plus erforderlich |
| Audio                                     | Janus/Opus/PCM-Pfad implementiert; echte Geräte-/Mikrofon-/ICE-Abnahme erforderlich             |
| Linux/macOS                               | Inaktive CI-Vorlage vorhanden; lokal nur Windows geprüft                         |
| Docker                                    | Build-/LAN-/Audio-Abnahme auf Docker Engine/Desktop erforderlich                                |
| Codex/ChatGPT Desktop                     | Reale verfügbare Hostversionen müssen Zufallstext ausschließlich aus dem MCP-Bild erkennen      |

Hardwareprotokoll pro Gerät: Modell, Firmwareversion, Betriebssystem/Architektur des Daemons, Auflösung, Codec, Netzwerk, Snapshot warm/kalt, MJPEG-Intervall, gewählte Strategie, HID-Sendedauer, Frame-Alter/Unsicherheit, Audiofeatures und Reconnect-Verhalten dokumentieren. Gateway-Zusatzlatenz nicht mit Hardware-/Modelllatenz vermischen. `device benchmark` schreibt keine Bilder auf Platte.

1/4/9-Quellen-Lastprofile verwenden getrennt echte Geräte oder deutlich markierte Simulatoren. Ein Testbericht darf aus Simulatoren keine Hardware-p95 behaupten. Es gibt keine feste Produktobergrenze von vier Geräten.

Referenzquellen, geprüft am 7. September 2026:

- [PiKVM API](https://docs.pikvm.org/api/) – Auth, HID-WebSocket, Streamer/Snapshot.
- [PiKVM Audio](https://docs.pikvm.org/audio/) – Richtungen und Voraussetzungen.
- [PiKVM kvmd](https://github.com/pikvm/kvmd) – Janus ustreamer features/watch/start und HID-Eventformen.
- [Offizielles MCP TypeScript SDK](https://github.com/modelcontextprotocol/typescript-sdk) – Transportgrenze und Server/Client.
- [Codex MCP](https://learn.chatgpt.com/docs/extend/mcp?surface=cli) – PAT-Konfigurationsprofil.

Die konkrete externe Host-/Firmwarekompatibilität bleibt bis zum jeweiligen Integrationstest unbestätigt. JetKVM und beide Comet-Modelle sind implementiert; Protokollquellen, Installationsweg und Grenzen stehen unter [Gerätetreiber](device-drivers.md).
