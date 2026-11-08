# REST Notifier

An example tuliprox plugin that delivers notifications to an HTTP endpoint. It is a source example
only: it is not shipped in the Docker image and not offered in the marketplace. The
built-in `rest` messaging channel does the same and is the supported way to use REST;
use this plugin as a reference for writing your own notification provider.

The plugin adds a channel `plugin.rest-notifier` to the messaging pipeline. Routing,
quiet hours, suppression and the retrying outbox are the host's; the plugin formats the
message and speaks to the service.

## Settings

The channel config is a `ui.config_extensions` fragment (`section: messaging`) declared in
`manifest.yml`. The Web UI messaging page renders its fields and the values are stored under
`settings.ext.messaging` of the plugin entry. The plugin has no standalone settings form.

```yaml
plugins:
  enabled: true
  plugins:
    - id: rest-notifier
      settings:
        ext:
          messaging:
            url: "https://hooks.example.org/tuliprox"
            routing:
              min_severity: warn
```

`routing` takes `notify_on`, `min_severity`, `quiet_hours`, `max_per_hour` and
`dedup_window_secs`; the host reads it, the plugin does not. The schema with all fields is in
`manifest.yml`.

## Build and test

```sh
bin/package-plugin.sh plugins/examples/rest-notifier   # target/plugin-examples/rest-notifier.tlpx
cargo test --manifest-path plugins/examples/rest-notifier/Cargo.toml
```
