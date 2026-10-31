# IT5570 firmware reference

This is a developer map of the EC firmware behind the
[tested fan interface](ec.md). It preserves the useful addresses and limits of
the analysis without requiring raw dumps in this repository. These are analysis
labels, not manufacturer symbols or a stable interface for other IT5570 laptops.
The controller neither reads nor flashes EC firmware during installation or use.

## Image identity and coverage

| Property | Established result |
|---|---|
| Controller | ITE IT5570, revision `0x02` |
| Laptop BIOS | `ZW.RP11D.26060101.009` |
| Captured code range | `0x00000–0x1FFFF`, 131072 bytes; two complete reads matched |
| SHA-256 | `d174b8a9d11d64adac62f7144f6bfec674b1e2579420925b7a2542a1e89e9e12` |
| Analysis | Targeted 8051 disassembly and checked branches, principally bank 0; not a complete bank-aware control-flow proof |
| Physical flash capacity | Unknown; the captured range is not a qualified full-chip recovery image |

The EC update payload embedded in the system BIOS is a different image:
`c56a9e2949e6ef508a442bf1d8908222a58d955232b158d08ef17ffed1f0df77`.
It differs from the captured active code at 67873 bytes. Its presence in the
system SPI does not prove which code the EC currently runs. The string
`ITE EC-V14.6` identifies a firmware family/SDK, not a reliable OEM release.
Recheck image identity after a BIOS/EC update; a matching chip ID or BIOS string
alone cannot establish unchanged register behavior.

## Keep the address spaces separate

| Address space | Example | Meaning |
|---|---|---|
| Host EC window | Offset `0x4B` | One byte addressed through the normal ACPI EC protocol |
| Internal XDATA | `0x044B` | The firmware's mapped host duty byte; normal host offsets map to `0x0400 + offset` |
| Internal hardware registers | XDATA `0x1809` | PWM channel 7 duty register, DCR7; not host offset `0x09` |
| Executable CODE | Bank 0 `0x32CA` | Fan-duty table read with 8051 `MOVC`; not writable host RAM |
| Flat image offset | `0x10000` | Byte 65536 in the captured file; not a 16-bit 8051 CODE address |

All CODE locations below refer to bank 0 of this identified image. Do not move
an address between banks or treat a file offset as a CPU address without
establishing the bank mapping.

For example, internal fan-stage index XDATA `0x0693` is unrelated to host EC
offset `0x93`, which maps to XDATA `0x0493` and reports battery percentage.
Normal host reads and writes cannot address arbitrary internal XDATA or CODE.

## Normal host protocol

The normal-runtime dispatcher, rather than only the special flash service,
establishes the mapping used by the fan backend:

| Bank 0 CODE | Role |
|---|---|
| `0x084A / 0x0858` | Receive PMC1 commands/data |
| `0x09C4 / 0x09A2` | Dispatch command and data states; `0x80` reads and `0x81` writes |
| `0x0902–0x0919` | Collect the write offset and data |
| `0x2B52 / 0x2B57` | Bank-call veneers for read/write helpers |
| `0x2800` | Bank trampoline; explains why a direct-call search misses the write helper |
| `0xD49C` | Read XDATA `0x0400 + offset` |
| `0xD4AF–0xD4C5` | Write XDATA `0x0400 + offset`; subsequent special cases cover offsets `0x11`, `0x64`, `0x65` |

Offsets `0x4B` and `0x4F` therefore reach the ordinary runtime fan fields without
entering a firmware update service. Linux `ec_sys` uses the kernel's ACPI EC
transactions. Direct port access would bypass that coordination and is not the
production backend. Transaction serialization does not make a multi-register
read/modify/write sequence atomic; see [ownership and recovery](ec.md#taking-and-releasing-control).

## Fan implementation map

| Bank 0 CODE / XDATA | Role |
|---|---|
| CODE `0xA449–0xA46D`, helper `0xA857` | Read hardware tachometer data, derive speed feedback, then enter fan update |
| CODE `0xA938` | Fan-update entry; XDATA `0x00B0 == 1` is a prerequisite |
| CODE `0xA94D–0xA95B` | In automatic mode, mirror DCR7 into host duty feedback |
| CODE `0xA95C–0xA968` | Manual bit selects host duty and bypasses the ordinary temperature/curve path |
| CODE `0xAADC–0xAAE1` | Copy selected duty into DCR7, XDATA `0x1809` |
| CODE `0xAA05–0xAA3E` | Load the automatic stage's thresholds/duty directly from CODE tables |
| XDATA `0x0693 / 0x069B / 0x069A` | Stage index capped at 6, update phase and confirmation counter |
| XDATA `0x0694 / 0x0695` | Selected automatic duty and previous automatic duty |

The [automatic curve values](ec.md#firmware-fan-curve) have one canonical table
in the interface reference. Their locations are:

| Table | Bank 0 CODE | RAM copy |
|---|---|---|
| Lower thresholds | `0x32AC` | XDATA `0x06A0` |
| Upper thresholds | `0x32BB` | XDATA `0x06B0` |
| Duty | `0x32CA` | XDATA `0x06C0` |
| Additional values | `0x32D9` | XDATA `0x06E0` |

CODE `0xA8DC–0xA937` creates those RAM copies, but the working updater uses
`MOVC` against the CODE addresses. Changing the copies is not an established
autonomous-curve setter. Temporary thresholds/duty are reloaded by later update
phases. The additional table contains `0,19,37,48,48,48,48`, multiplied by 100
into XDATA `0x0696:0x0697`; its meaning is unconfirmed, so it is not labelled an
RPM target.

The manual branch precedes the routine's temperature cutoff, internal debug
override and automatic curve. It copies the requested byte without a software
clamp. No lease/heartbeat or independent emergency fan takeover was established
in that path. The bounded host backend and its independent checks are therefore
necessary; CPU thermal throttling does not prove an EC fan takeover.
The accepted 35% start duty and manual/automatic transition were checked on the
laptop; [features and limits](README.md#features-and-limits) describe
that physical scope separately from static analysis.

Host temperature `0x70` comes from CPU PECI GetTemp: CODE `0xA345` calls `0xE933`,
which reaches transaction code `0xED00`; `0xA348–0xA34F` publishes XDATA `0x9608`
to `0x0470`. It is quantized, sampled CPU telemetry. No guaranteed freshness
interval or host-visible validity flag was established. It is not a measured
heatsink temperature; the live controller uses Linux CPU-temperature readings.

## Other paths and why they are not substitutes

| Candidate | What the analysis establishes |
|---|---|
| Internal debug override | CODE `0xA9B1–0xA9E0` uses XDATA `0x069F == 0xDC` for raw duty or `0xCC` for injected temperature, with data at `0x069E`. This is a separate, lower-priority internal path, not a supported host control. Normal release assumes it is inactive |
| Host offsets `0x63–0x65` | Select/write internal thermal windows `0x0660–0x0667`, through CODE `0xD13B / 0xD173`. The traced consumers generate thermal events; they are not the fan curve |
| Thermal query `0xF1` | An event path exists, but no `_QF1` handler was found in the inspected ACPI tables. Userspace delivery and an event-driven replacement for polling remain unverified |
| BIOS active-trip temperatures/percentages | Feed a separate ACPI policy; they are not a demonstrated editor for these CODE tables. See [BIOS thermal settings](bios.md#fan-and-thermal-settings) |
| IT5570 PWM timeout feature | The documented PWM5 timeout does not establish protection for this fan on PWM7 |
| A different IT5570 laptop's driver | A matching controller does not establish the same host map, fan pin or firmware behavior |

These are bounded findings from selected paths. They do not prove that every
vendor interface elsewhere in the banked firmware has been excluded.

## Active-code capture and exit

Reading XDATA is not reading executable flash. The successful capture used the
EC's separate PMC2 service; the original `0x268/0x26C` pair was enabled in the EC
but was not routed by the host bridge. A pre-existing COMA decode at `0x3F8–0x3FF`
provided a temporary route after checking that both ITE UARTs and host users of
that range were inactive. The four PMC2 address registers were restored afterward;
PCH routing itself was not changed. This routing is specific to the tested board.

The relevant bank 0 locations preserve the distinction between a warm exit and
a hardware reset:

| CODE | Meaning |
|---|---|
| `0xF1BD–0xF1DE` | Command `0xDC` enters the service at `0xD586` |
| `0xD58F–0xD594` | Acknowledge with `0x33` through PMC2 |
| `0xD5CC–0xD65F` | Flash transaction selectors: begin, opcode, address/dummy payload, read byte and finish |
| `0xD666–0xD66F` | Command `0xFC` writes warm-start marker `0x33` to XDATA `0x0045` |
| `0xD696`, then `0x0459–0x0480` | Jump through `0x0400`, remove the service mapping, clear the marker and resume normal handlers |
| `0xD67C–0xD694` | Separate `0xFE` hardware-reset path; not used for this capture |

The capture used only SPI status, ID and fast-read operations (`0x05`, `0x9F`,
`0x0B`), never write-enable, erase or program. The observed ID response
`FF FF FE FF` does not establish flash capacity. The checked exit finished the
transaction selectors, used `0xFC`, then verified ordinary operation and routing
restoration separately from verifying the data bytes.

Read-only flash operations still change the EC's execution state: normal
temperature-policy scheduling was paused during the roughly ten seconds of
capture. This is not a background monitoring interface or a generic safe dump
recipe. No raw reader or firmware image is shipped with the controller.

## References

- [ITE IT5570 V0.3.1 datasheet](https://github.com/minortex/ec_reverse/blob/main/ref/IT5570_A_V0.3.1_U.pdf): address spaces, PWM/TACH, PMC and PECI hardware definitions.
- [Linux ec_sys implementation](https://github.com/torvalds/linux/blob/master/drivers/acpi/ec_sys.c): the normal host debug interface used by the bounded backend.
- [Charging policy and remaining charge-limit work](charging.md): a separate EC subsystem, with its own ownership problem.

Firmware-specific addresses above refer to the identified active image, not to
the different BIOS updater payload. Raw firmware, system NVRAM and private
machine captures remain outside the public source tree.
