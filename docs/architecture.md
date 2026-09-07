# Architektur und Verträge

## Komponenten

`shared/contracts.ts` ist die versionierte Grenze für Geräte, Capabilities, Actions, Frames, Audio und Treiber. `server/core.ts` besitzt Sessions, Leases, Generationen und Action-Journale. `server/media.ts` besitzt den neuesten vollständigen Frame und Consumerreferenzen. `server/drivers/pikvm.ts` enthält ausschließlich PiKVM-Semantik. `server/server.ts` und `server/mcp.ts` adaptieren denselben Core. Der stdio-CLI-Modus verbindet sich als MCP-Client zum existierenden Daemon. `server/store-worker.ts` kapselt SQLite. `server/audio-worker.ts` kapselt Janus/WebRTC/Opus.

## Geräteadapter

`KvmDriver` verlangt `connect`, `disconnect`, `capabilities`, `snapshot`, `validate`, `execute`, `releaseAllInputs`. `subscribeVideo`, `subscribeAudio`, `sendAudio` sind capabilityabhängig. Treiber werden über `core.factories` registriert. Ein neuer Treiber darf denselben Media/Core verwenden, ohne PiKVM-Endpunkte zu übernehmen. Ein `device_id` bezeichnet einen logischen Video-/HID-Endpunkt.

Verbindungsart und Identität gehören zur Konfiguration. Eine künftige lokale USB-Implementierung ergänzt eine Transportvariante mit lokaler Identität und Hotplug-Generation; kein USB-Gerätezugriff ist enthalten. Multiport-Geräte benötigen eine ausdrückliche Port-ID sowie eine eigene Portumschalt-Arbitrierung vor Freigabe.

Unbekannte Fähigkeiten sind nicht verfügbar. Die Capabilities enthalten Video, Signal, absolute/relative Maus, Buttons, Scroll, HID/Layouts/Texteingabe, beide Audiorichtungen, Zeitbasis und einen generischen Extensions-Bereich. Power/Reset und Virtual Media sind dort nur zukünftige Verträge. Es existieren keine dazugehörigen ausführbaren Endpunkte oder UI-Aktionen.

## Eingabe und Wiederholungen

Lease-Inhaber = owner + Client + Session + Control-Generation. Browser, HTTP und stdio haben keinen Vorrang. Ein normaler acquire liefert bei belegter Lease `CONTROL_BUSY`. Not-Stopp erhöht die Generation, bricht den laufenden Batch ab, gibt Eingaben frei und suspendiert Kontrolle. Resume vergibt keine Lease.

Jeder Batch wird vollständig strukturell und soweit möglich auf Fähigkeiten/Text geprüft. Aufträge pro Gerät werden serialisiert. Unabhängige Geräte bleiben parallel. Pro Element werden `not_started`, `sent`, `acknowledged`, `unknown` geführt. WebSocket-Sendebestätigung ist nur `sent`, kein Hardware-ACK. Fehler stoppen Folgeaktionen, erlauben jedoch sichere Freigabe. Ein Screenshot-Timeout verändert vorherige Aktionsstatus nicht.

Request-Journale enthalten Hashes normalisierter Aufträge und Ergebnisse im RAM. Gleiche ID/gleicher Hash erzeugt keine neue Eingabe; anderer Hash führt zu `REQUEST_CONFLICT`. Nach Neustart gelten alte Sessions nicht mehr. Keine Exactly-once-Zusage über Abstürze.

Key-Mapping v1: DOM-HID-Namen und dokumentierte Aliase (`CTRL`, `ALTGR`, `ENTER`, `UP`, …). DE vertauscht Y/Z für Texteingabe, unterstützt Umlaute, ß und AltGr; US verwendet US-HID-Chords. Unicode außerhalb des expliziten Layoutmappings wird vor Batchstart zurückgewiesen. Die deutschen toten Tasten für Akzentzeichen werden ausdrücklich mit anschließendem Space aufgelöst. Scrollwerte sind eigene logische 100er-Einheiten pro HID-Tick, positiv rechts/unten; PiKVM-Wheel-Y wird invertiert. Ziel-OS-Einstellungen beeinflussen die sichtbare Scrollstrecke.

## Frames

`FrameInfo` enthält Bild-/Ansichts-ID, Geräte-/Verbindungsgeneration, Eingaberevision, native Bildgröße, MIME, Quell-/Empfangszeit, Unsicherheit, Alter, Freshness-Basis, Signal und Stale-Flag. Transformation v1 ist natives Vollbild ohne Crop/Rotation/Letterbox im Server. Browserkoordinaten werden anhand des tatsächlichen Bildrechtecks in Bildpixel übersetzt. Format-/Auflösungs-/Verbindungswechsel erzeugen eine neue view_id. Ein neuer Frame allein nicht.

MJPEG verarbeitet ausschließlich vollständige JPEGs; kein beliebiges H.264-Paket wird als Screenshot ausgegeben. Alle Medien leben in begrenzten RAM-Puffern. Consumer erhalten aktuelle Frames; langsame Consumer akkumulieren keine unbegrenzte Ausgabeschlange. Ingest und Medienverteilung sind unabhängig davon, ob ein Browser offen ist.

Ein künftiger Recorder ist ein zusätzlicher Consumervertrag mit Geräte-/Zeit-/Generations-/Lückenmetadaten. Seine getrennten Freigaben, Segmentierung, Quota, Verschlüsselung, Retention und Export werden erst im separaten Recorderauftrag implementiert. Dieser POC enthält keinen Medienwriter.

## Transport und Fehler

MCP SDK ist auf 1.30.0 gepinnt; Versionsaushandlung, Session-Header und Streamable HTTP liegen beim offiziellen SDK. Strict Input-Schemas werden aus Zod erzeugt. Erfolgs-/Fehlerenvelopes haben `ok`, optionale fachliche `error`, Aktionsstatus und `frames`. Bilddaten stehen ausschließlich in separaten MCP Image-Blöcken. Web-API-Version ist `/api/v1`.

Zusätzliche POC-Fehler: `INVALID_ARGUMENT`, `INVALID_ADDRESS`, `INVALID_SECRET_REF`, `UNSUPPORTED_DRIVER`, `NOT_FOUND`, `REVISION_CONFLICT`, `STORAGE_ERROR`, `CONFIRM_REQUIRED`. Datenbankfehler enthalten keine SQL-/Secret-/Inhaltsdetails. Für Fehlerstatus und zentrale fachliche Fehler siehe `GatewayError` und MCP-Ergebnisse.

## Grenzen

Ein Gateway-Lock blockiert keine unabhängige Hersteller-UI oder physische Tastatur. Secrettrennung ist keine Sandbox für vertrauenswürdigen lokalen Code. Plugin-Outboundhosts sind deklarativ. Die native OS-/Docker-Supportmatrix wird durch tatsächlich ausgeführte Prüfungen vervollständigt; CI-Konfiguration ist kein vorweggenommener Erfolgsnachweis.
