# Vertrauenswürdige Plugins · kvmhelm.plugin/v1

Der öffentliche Vertrag ermöglicht die getrennte Installation von Erweiterungen ohne Änderungen an MCP oder PiKVM-Treiber.

```json
{
  "id": "local.example",
  "version": "0.1.0",
  "apiVersion": "kvmhelm.plugin/v1",
  "kind": "feature",
  "runtime": "external-process",
  "entry": "index.mjs",
  "permissions": [
    "audio:read:from_target",
    "ui:panel",
    "notifications:publish"
  ],
  "outboundHosts": [],
  "secretRefs": [],
  "uiSlots": ["kvm.sidepanel", "settings.plugins", "overview.badge"]
}
```

Dateiname: `kvmhelm.plugin.json`. Die Version ist explizit; Updates durch erneute bewusste Installation. Kein automatisches Paketupdate. Programme sind vollständig vertrauenswürdig; ein separater Prozess dient Fehlerisolation, nicht der Behauptung einer feindlichen Codesandbox.

Transport ist Node-IPC mit `serialization: advanced`. Medienpayloads sind `Buffer`, keine JSON-Samplearrays. Bei langsamen Empfängern wird das betroffene Abonnement mit Lücken-/Ressourcenmeldung beendet. Keine Inhaltslogs vom Pluginprozess im Core.

Der native Rust-Daemon verwendet dafür einen eingebetteten optionalen Node-Broker. Node wird erst beim Start eines aktivierten Plugins benötigt; die Suche erfolgt über `KVMHELM_NODE`, neben dem Daemonbinary und dann über PATH. `KVMHELM_NODE` muss im Umfeld des laufenden **Daemons** gesetzt sein. Ein fehlendes Programm lässt die Registrierung mit `status: error` und `runtime_error.code: DEPENDENCY_MISSING` sichtbar. Ohne Plugins benötigt der native Core kein Node. Der Broker bleibt nach seiner ersten Verwendung bis zum Daemonende verfügbar. Eigene Modulabhängigkeiten liegen im lokalen Pluginverzeichnis; es erfolgt keine Paketinstallation.

Installation und Verwaltung sind über die HTTP-API und die native Remote-CLI `plugin list|install <directory>|enable <id>|disable <id>|restart <id>|uninstall <id>` möglich. `install` erhält mit `--device ID` die ausdrücklich gewünschten Gerätefreigaben. Die CLI spricht den laufenden Daemon an. Uninstall entfernt die Registrierung und beendet deren Consumer; Programmdateien und eigene Plugindaten bleiben bestehen.

Lifecycle-Nachrichten vom Host: `initialize`, `start`, periodisch `health`, `stop`, `dispose`. `initialize` enthält API-Version, Plugin-ID, ausdrücklich freigegebene Geräte, Permissions, Plugin-Datenpfad, deklarierte Secretreferenzen und Budgets. Das Plugin antwortet mit `{type:"ready"}` und auf Health mit `{type:"health"}`. Fehlende Healthantworten oder Crash deaktivieren die laufenden Consumer. Nach Stop gibt es maximal zwei Sekunden für geordnetes Ende, dann Prozessabbruch.

Nachrichten zum Host:

| Typ            | Inhalt                                                                  |
| -------------- | ----------------------------------------------------------------------- |
| `devices`      | Antwort enthält ausschließlich freigegebene IDs, Namen und Capabilities |
| `subscribe`    | `id`, `device_id`, `source: "audio"` oder `"video"`                     |
| `unsubscribe`  | `id` des eigenen Consumers                                              |
| `panel`        | `panel: {id,title,text,highlights?,device_id?,slot}`                    |
| `notification` | `device_id`, `text`                                                     |

Hostausgabe: `media` mit Abonnement-ID, Geräte-ID und `chunk`. Audio enthält `format=pcm_s16le`, `sample_rate=48000`, `channels=2`, `sequence`, `timestamp_ms`, `connection_generation`, `discontinuity`, `data:Buffer`. Zeitstempel sind lokale monotone Empfangszeiten, keine behaupteten samplegenauen Hardwarezeiten. Video liefert Frame-Metadaten und vollständige PNG/JPEG-Buffer. Ausfälle senden `gap` mit Code.

Panels und Benachrichtigungen werden als Text gerendert, nie über `v-html`. Optionale `highlights` sind begrenzte Start-/Endbereiche im Paneltext und werden als sichere `mark`-Elemente dargestellt. Alle UI-Updates laufen geordnet durch denselben Eventkanal; ein Plugin muss Inhaltsupdates vor der dazugehörigen Benachrichtigung senden. Events haben `event_id`, Zeit, Typ, Geräte-ID und `data.plugin_id`. Neue Live-Benachrichtigungen bleiben ausschließlich in der Weboberfläche und deren begrenztem RAM-Verlauf; nach der ersten Browserinteraktion begleitet sie ein kurzer Signalton.

Ein separates Plugin konfiguriert seine eigene Secretquelle; der Host übermittelt keine Gerätepasswörter, Admin-PATs oder Zugangsdaten externer Dienste. `secretRefs` sind Referenzen, kein allgemeiner Lesezugriff auf den Secretstore. Plugin-eigene optionale Persistenz liegt unter dem eigenen Datenpfad. Der Kern implementiert keinen Medienrecorder.

Stop, Disable und Uninstall beenden alle Consumer des Plugins. Eine Änderung der Gerätefreigabe erfolgt durch erneute Installation mit dem ausdrücklich gewünschten Geräteumfang und stoppt die alte Instanz zuerst. `kind: driver|media` ist Metadatum des bestehenden Manifests; ein ausführbares Treiber-/Medien-RPC-Protokoll gehört bisher nicht zum Runtimevertrag.

Der native Host begrenzt Registrierungen auf 128, Manifeste auf 256 KiB und Medienabos auf 64 je Plugin beziehungsweise 256 insgesamt. Seine Transportqueue hält höchstens 128 ausstehende Nachrichten und 16 MiB angenommene Datenbytes bis zur tatsächlichen Sendebestätigung oder zum Prozessende. Für Prozessverwaltung bleiben 16 zusätzliche Steuerplätze. Die Healthabfrage läuft alle zehn Sekunden; länger als 30 Sekunden ohne Antwort beendet der Host den Lauf. Windows besitzt Broker und Forks in einem Jobobjekt, Unix in einer eigenen Prozessgruppe. Der Windows-Pfad ist lokal geprüft; Linux-/macOS-Ausführung bleibt offen. Diese Transportgrenzen beschränken nicht den gesamten Speicher des vertrauenswürdigen Plugincodes.

## Optionale Formularaktionen und Seitenpanels

Ein Panel kann zusätzlich `fields` und `actions` deklarieren. Felder besitzen `id`, `label`, `type: "string-list" | "number" | "multi-select"` und `value`. Multi-Select benötigt `options: [{value,label}]` und erlaubt ausschließlich diese Werte. Aktionen besitzen `id`, `label` und optional `disabled`. Die UI rendert Formularfelder ohne Plugin-Skripte oder HTML-Injektion. Gerätebezogene Seitenpanels lassen sich pro Bildschirm einblenden und stehen bei ausreichender Breite daneben. Die Anzeigeeinstellung steuert keine laufenden Plugin-Consumer.

`POST /api/v1/plugins/:id/panels/:panel/actions` erwartet `{action,values}`. Der Host benötigt `plugins:manage` und überprüft zusätzlich die Gerätefreigabe des Panels. Er akzeptiert nur deklarierte Aktionen und Felder und sendet `{type:"action",request_id,panel_id,action,device_id,values}`. Der native Host prüft Autorisierung und Laufgeneration nochmals unmittelbar vor dem Transportversand. Das Plugin antwortet `{type:"action.result",request_id,ok}`. Zeitlimit zehn Sekunden; ungewisse Aktionen werden nicht automatisch wiederholt. Maximal 32 ausstehende Aktionen insgesamt und 256 aktuelle Panels je Plugin.

Benachrichtigungen dürfen optional `reference: {panel_id,item_id,revision}` auf ein eigenes Panel desselben Gerätes verweisen. Die Referenz bleibt im Event, die In-App-Meldung zeigt den Text. Historische SSE-Ereignisse erscheinen nicht erneut als Toast. Auslieferung in einen geschlossenen Browser ist nicht zugesagt.
