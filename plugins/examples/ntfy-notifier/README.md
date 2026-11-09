# ntfy Notifier

An example tuliprox plugin that delivers notifications to an ntfy topic. It is a source example
only: it is not shipped in the Docker image and not offered in the marketplace. The
built-in `ntfy` messaging channel does the same and is the supported way to use ntfy;
use this plugin as a reference for writing your own notification provider.

The plugin adds a channel `plugin.ntfy-notifier` to the messaging pipeline. Routing,
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
    - id: ntfy-notifier
      settings:
        ext:
          messaging:
            topic: "tuliprox"
            routing:
              min_severity: warn
```

`routing` takes `notify_on`, `min_severity`, `quiet_hours`, `max_per_hour` and
`dedup_window_secs`; the host reads it, the plugin does not. The schema with all fields is in
`manifest.yml`.

## Build and test

```sh
bin/package-plugin.sh plugins/examples/ntfy-notifier   # target/plugin-examples/ntfy-notifier.tlpx
cargo test --manifest-path plugins/examples/ntfy-notifier/Cargo.toml
```
