# Fan and power settings

Edit the installed configuration:

```sh
sudoedit /etc/minibookx-ctl/config.conf
```

Change existing entries rather than adding duplicate keys or sections. Your
release's template is `/usr/local/share/doc/minibookx-ctl/presets.conf`; the
[repository sample](../../config/presets.conf) may contain unreleased changes.
The [configuration reference](../reference/configuration.md) lists accepted values.

## Fan profiles

Select a profile with the top-level `preset` setting:

```ini
preset = quiet
```

| Profile | Behavior |
|---|---|
| `balance` | Default. Waits for sustained heat before starting the fan. |
| `quiet` | Allows more heat and waits longer before starting or increasing cooling. |
| `performance` | Starts cooling earlier and responds sooner to sustained load. |

Brief heat spikes can pass without starting the fan. Once it is running,
cooling delays and a minimum run time prevent repeated starts and stops.
The emergency threshold bypasses normal delays. Check your installed template
for the defaults supplied with your release; your active config can differ.
High-temperature curves require your own load and case-temperature checks.

To customize a profile, edit its existing `curve` line. For example:

```ini
[balance]
curve = 55:40,70:50,82:70,90:80
```

This example requests 40% duty at 55°C, 50% at 70°C, 70% at 82°C and 80% at
90°C, subject to the profile's delays. It is an example, not a shipped preset.
Keep the other settings in that section.
Duty is the fan's electrical PWM setting, **not a percentage of maximum RPM**.
Keep the values within the [accepted ranges](../reference/configuration.md#fan-settings).

## Leave the fan to firmware

Change the existing top-level setting:

```ini
fan_control = false
```

Tablet detection still works, and the firmware controls when the fan runs.
This mode also avoids the need for writable EC access. It does not force the
fan off. For a first installation without fan control, use the
[install-before-starting procedure](install.md#without-fan-control).

Unlike profile changes, changing `fan_control` requires a service restart.

## CPU power limits

The default is `power_control = false`. Even with the Performance profile,
CPU power limits stay as the firmware or other system software set them.

If you deliberately want overrides, review `pl1_w` and `pl2_w` in **each**
profile, including inactive profiles, before enabling `power_control = true`:

| Setting | Meaning |
|---|---|
| `pl1_w` | Sustained CPU package power limit, in watts |
| `pl2_w` | Short-term CPU package power limit, in watts |
| `firmware` | Restore the value captured before this service's first override |

All shipped profiles leave both limits at `firmware`. See
[power-limit bounds and restoration](../reference/configuration.md#power-limits)
and avoid running competing power-limit controllers.

## Apply and check

Validate the complete file as the service user:

```sh
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
```

Profile and curve changes reload automatically. If you changed `fan_control`,
restart after validation:

```sh
sudo systemctl restart minibookx-ctl.service
```

Check the accepted settings with `minibookx-ctl --doctor`. An invalid reload
leaves the previous configuration active; the journal explains the error.
