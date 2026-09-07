# Robustheit und Betrieb

Die dauerhafte Browseranmeldung mit PAT im Local Storage bleibt unverändert.

## Steuerung und Wiederherstellung

Nur der ausdrücklich fokussierte Bildschirmbereich überträgt Tastaturereignisse. Lokale Formularfelder deaktivieren diesen Modus. Die Browserwarteschlange enthält höchstens 32 wartende Aufträge; benachbarte Scrollaufträge werden innerhalb der Aktionsgrenzen zusammengefasst. Kontrollverlust, Fokusverlust und Zielwechsel verwerfen wartende Eingaben. Bereits versandte Eingaben können ausgeführt worden sein und werden nicht automatisch wiederholt.

Frames behalten die beim Beginn eines Snapshot-Abrufs gültige Eingaberevision. Cachebilder einer früheren Revision werden nach Kontrollübernahme nicht als neue Beobachtung ausgegeben. Empfangszeit bleibt kein Beweis des Hardware-Capture-Zeitpunkts.

Der Browser ersetzt verlorene Sessions durch Beobachtungssessions mit begrenztem Backoff. Kontrolle muss nach einem Ausfall erneut angefordert werden. Nur der geplante Wechsel einer noch kontrollierten Session kurz vor Erschöpfung des 256-Einträge-Journals versucht erneut Kontrolle zu erwerben; konkurrierende Leases bleiben exklusiv. Eine Freigabe bei vollem Journal schließt die Session. Frühere Request-IDs werden nicht aus einem weiterverwendeten Journal entfernt.

Der Eventstream verbindet sich erneut mit Cursor und Daemon-Instanz-ID. Ein Neustart setzt den Cursor zurück. Der sichtbare Gatewaystatus folgt der tatsächlichen Eventverbindung. Die PAT-Speicherung bleibt erhalten.

## Ressourcen und Gesundheit

Sessionplätze werden vor dem Verbindungsaufbau reserviert: global standardmäßig 128, je Token höchstens 32. Medien und normale API-/MCP-Aufrufe erhalten getrennte Budgets pro Token (12.000 beziehungsweise 2.400 Requests/Minute); identische IP-Adressen verschiedener Token teilen kein Budget. Not-Stopp, Eingabefreigabe und Schließen von Sessions bleiben bei erschöpftem Budget erreichbar und erfordern weiterhin ihre üblichen Berechtigungen. HTTP 429 liefert einen strukturierten Fehler und Retry-After.

`GET /healthz` prüft die HTTP-Erreichbarkeit. `GET /readyz` prüft zusätzlich die Datenbankverbindung und antwortet bei Ausfall mit 503. Beide liegen außerhalb der Authentifizierung und der Anfragebudgets; sie enthalten keine Konfiguration. Ein Datenbank-Workerfehler beendet offene und spätere Anfragen mit einem begrenzten, inhaltsfreien Fehler. Start und Aufträge haben eine Zehn-Sekunden-Frist; höchstens 256 Datenbankanfragen warten gleichzeitig. Nach unklarem Schreibfehler den Zustand prüfen, bevor eine Änderung wiederholt wird.

Authentifizierte `/api/v1/diagnostics` zeigen Workerzustand, Sessionanzahl, Warteschlangen, Verbindungsfehlerzähler, Event-Loop-p95 und Fehlercodes. Keine Eingabetexte, Audio- oder Bilddaten werden dafür protokolliert.

## Secrets und Adapter

Gerätelöschung entfernt die zugehörigen Zugangsdaten und CA. Nach einem nachgewiesenen Revisionskonflikt werden neu geschriebene Secretdateien zurückgerollt. Bei unklarer Commit-Antwort bleiben Dateien erhalten, damit ein möglicherweise erfolgreicher Commit seine Zugangsdaten nicht verliert.

Bei gestopptem Daemon bereinigt `node dist/server/cli.js secrets cleanup` ausschließlich gatewaygenerierte, unreferenzierte `.secret`-Dateien, die älter als 24 Stunden sind. Masterkey, PATs, fremde Dateinamen, aktuelle Schreibvorgänge und referenzierte Secrets bleiben erhalten. Daten-/Secretpfade wie bei `serve` übergeben. Vor Dateireparaturen das dokumentierte Backupverfahren verwenden.

PiKVM und JetKVM teilen LAN-/TLS-Transport und HID-Planung über `NetworkHidDriver`; die Endpunkte und Verbindungen bleiben in den jeweiligen Herstelleradaptern. Der Audiovertrag enthält Mikrofonstart/-stopp und PCM-Übertragung. Die Mikrofon-Exklusivität gilt auch bei mehreren HTTP-/HTTPS-Listenern desselben Core. Beide WebRTC-Pfade deaktivieren den impliziten öffentlichen STUN-Standard und akzeptieren ausschließlich lokale ICE-Adressen.

## Prüfung

`npm run build`, danach `npm test` und `node scripts/ui-layout.mjs`. Die Regressionstests verwenden temporäre Verzeichnisse, Simulatoren und lokale Protokollserver. Sie prüfen unter anderem Tastaturfokus, initialen Verbindungsfehler, Core-Neustart bei offenem Browser, Journalwechsel, Sicherheitsrouten bei erschöpften Budgets, Worker-Ausfälle, Revisionen, konkurrierende Sessions, Secretbereinigung, Plugin-Abonnement-IDs und den herstellerneutralen Audiovertrag.

Die aktive CI führt Build und Tests auf Windows, Linux und macOS sowie einen Docker-Build aus. Ihr tatsächliches Ergebnis ist separat vom lokalen Prüfstand zu bewerten. Physische Geräte, Firmwareunterschiede, Audioqualität und Netzwerkausfälle über längere Zeit benötigen weiterhin die Hardwareabnahme gemäß `verification.md`; Simulatorergebnisse ersetzen diese nicht.
