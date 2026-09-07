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

Lifecycle-Nachrichten vom Host: `initialize`, `start`, periodisch `health`, `stop`, `dispose`. `initialize` enthält API-Version, Plugin-ID, ausdrücklich freigegebene Geräte, Permissions, Plugin-Datenpfad, deklarierte Secretreferenzen und Budgets. Das Plugin antwortet mit `{type:"ready"}` und auf Health mit `{type:"health"}`. Fehlende Healthantworten oder Crash deaktivieren die laufenden Consumer. Nach Stop gibt es maximal zwei Sekunden für geordnetes Ende, dann Prozessabbruch.

Nachrichten zum Host:

| Typ            | Inhalt                                                                  |
| -------------- | ----------------------------------------------------------------------- |
| `devices`      | Antwort enthält ausschließlich freigegebene IDs, Namen und Capabilities |
| `subscribe`    | `id`, `device_id`, `source: "audio"` oder `"video"`                     |
| `unsubscribe`  | `id` des eigenen Consumers                                              |
| `panel`        | `panel: {id,title,text,device_id?,slot}`                                |
| `notification` | `device_id`, `text`                                                     |

Hostausgabe: `media` mit Abonnement-ID, Geräte-ID und `chunk`. Audio enthält `format=pcm_s16le`, `sample_rate=48000`, `channels=2`, `sequence`, `timestamp_ms`, `connection_generation`, `discontinuity`, `data:Buffer`. Zeitstempel sind lokale monotone Empfangszeiten, keine behaupteten samplegenauen Hardwarezeiten. Video liefert Frame-Metadaten und vollständige PNG/JPEG-Buffer. Ausfälle senden `gap` mit Code.

Panels und Benachrichtigungen werden als Text gerendert, nie über `v-html`. Alle UI-Updates laufen geordnet durch denselben Eventkanal; ein Plugin muss Inhaltsupdates vor der dazugehörigen Benachrichtigung senden. Events haben `event_id`, Zeit, Typ, Geräte-ID und `data.plugin_id`. Benachrichtigungen bleiben ausschließlich in der Weboberfläche und deren begrenztem RAM-Verlauf.

Ein separates Plugin konfiguriert seine eigene Secretquelle; der Host übermittelt keine Gerätepasswörter, Admin-PATs oder Zugangsdaten externer Dienste. `secretRefs` sind Referenzen, kein allgemeiner Lesezugriff auf den Secretstore. Plugin-eigene optionale Persistenz liegt unter dem eigenen Datenpfad. Der Kern implementiert keinen Medienrecorder.

Stop, Disable und Uninstall beenden alle Consumer des Plugins. Eine Änderung der Gerätefreigabe erfolgt im POC durch erneute Installation mit dem ausdrücklich gewünschten Geräteumfang und stoppt die alte Instanz zuerst. Weitere Treiber werden zunächst als vertrauenswürdige In-Process-Factories über den TypeScript-Adaptervertrag eingebunden; ausführbare Treiber-/Medien-RPC-Plugins sind nicht als freigegebener Runtimepfad behauptet.
