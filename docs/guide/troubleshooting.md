# Troubleshooting

Start with the read-only health report:

```sh
minibookx-ctl --doctor
```

Run it as your ordinary user. If an optional field needs elevated access, the
report will say so. Doctor recommends packages and next steps; it does not
install or repair anything.

## Find the problem

| Symptom | Next step |
|---|---|
| Service does not start | Read its journal below; check the [hardware profile](../hardware/README.md) and [kernel requirements](../reference/kernel-support.md) |
| Kernel lacks EC access | [Use firmware fan control](install.md#without-fan-control); tablet support has separate requirements |
| Config change has no effect | Validate the file; an invalid reload keeps the previous settings. Changing `fan_control` needs a restart |
| Fan stays silent at idle or during a brief spike | Check the [profile threshold and delays](configuration.md#fan-profiles) |
| Folding or rotation is wrong | Follow [tablet mode](tablet-mode.md) |
| Display is corrupted at startup or 60 fps video is uneven | Read [display setup](display.md) |
| Video uses too much CPU | Check [graphics packages and playback](graphics.md) |
| Update or removal refuses to continue | Read [recovery failed](#recovery-failed) |

## Service logs

```sh
systemctl status minibookx-ctl.service --no-pager
sudo journalctl -u minibookx-ctl.service -b
```

For config errors, edit with `sudoedit` and validate:

```sh
sudo -u minibookx-ctl /usr/local/bin/minibookx-ctl \
  --check-config /etc/minibookx-ctl/config.conf
```

`minibookx-ctl --status` shows the last accepted profile and hardware records.
The [field reference](../reference/diagnostics.md) explains values that need
closer interpretation, including requested duty versus observed fan feedback.

## Recovery failed

Stopping the service requests normal input, firmware fan control and restoration
of any CPU limits it changed:

```sh
sudo systemctl stop minibookx-ctl.service
```

If this fails, keep the installed executable and the records under
`/run/minibookx-ctl/`, and read the journal. Do not delete those records to force
an update: they are needed to restore hardware state. With the service stopped,
you can retry recovery using the installed version:

```sh
sudo /usr/local/bin/minibookx-ctl --recover
```

If it still fails, leave the service stopped and report the error.

## Report an issue

Include what happened, how to reproduce it and these reports:

```sh
minibookx-ctl --version
minibookx-ctl doctor --json > doctor.json
minibookx-ctl debug --json > minibookx-debug.json
```

Debug includes hardware references, package versions, sensor and battery data.
It does not probe raw EC/I2C registers. Review the files before sharing and
include the relevant service errors, rather than an entire personal system log.

For startup display corruption, also collect the relevant kernel messages:

```sh
sudo journalctl -k -b --no-pager | grep -Ei 'i915|dsi|vbt'
```

Collect these during the affected boot. A clean log does not rule out visible
panel corruption; describe what the physical screen shows as well.
