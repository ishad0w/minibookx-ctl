//! Optional graphics package advice; no renderer or raw-device probes.
use super::{Missing, OsRelease, Sources, clean, field, note, section, text, warning};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{self, Read},
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PackageBackend {
    Dpkg,
    Rpm,
    Pacman,
}

pub(super) type Inventory = BTreeMap<String, String>;

// Only these names survive parsing. Never publish a machine's full inventory.
const PACKAGES: &[&str] = &[
    "libgl1-mesa-dri",
    "mesa-vulkan-drivers",
    "intel-media-va-driver",
    "intel-media-va-driver-non-free",
    "firmware-intel-graphics",
    "linux-firmware",
    "linux-firmware-intel-graphics",
    "vainfo",
    "mesa-dri-drivers",
    "libva-intel-media-driver",
    "intel-media-driver",
    "intel-gpu-firmware",
    "libva-utils",
    "mesa",
    "vulkan-intel",
    "linux-firmware-intel",
    "ffmpeg-free",
    "ffmpeg",
    "ffmpeg-libs",
    "libavcodec-freeworld",
    "openh264",
    "mozilla-openh264",
    "gstreamer1-plugin-openh264",
];

fn backend(os: &OsRelease) -> Option<PackageBackend> {
    match os.id.as_str() {
        "debian" | "ubuntu" => Some(PackageBackend::Dpkg),
        "fedora" => Some(PackageBackend::Rpm),
        "arch" | "cachyos" => Some(PackageBackend::Pacman),
        _ => None,
    }
}

fn recipe_version(os: &OsRelease) -> &'static str {
    match os.id.as_str() {
        "debian" => "13",
        "fedora" if os.version.parse::<u32>().is_ok_and(|v| v <= 43) => "43",
        "fedora" => "44",
        "ubuntu"
            if os
                .version
                .split('.')
                .next()
                .and_then(|v| v.parse::<u32>().ok())
                .is_some_and(|year| year <= 24) =>
        {
            "24.04"
        }
        "ubuntu" => "26.04",
        _ => "rolling",
    }
}

pub(super) fn query_packages(backend: PackageBackend) -> Result<Inventory, String> {
    let (name, args): (&str, &[&str]) = match backend {
        PackageBackend::Dpkg => (
            "dpkg-query",
            &[
                "--show",
                "--showformat=${Package}\t${Version}\t${Architecture}\t${db:Status-Status}\n",
            ],
        ),
        PackageBackend::Rpm => (
            "rpm",
            &[
                "-qa",
                "--qf",
                "%{NAME}\t%{EPOCHNUM}:%{VERSION}-%{RELEASE}\t%{ARCH}\n",
            ],
        ),
        PackageBackend::Pacman => ("pacman", &["-Q"]),
    };
    let path = crate::platform::system_tool(name)
        .map_err(|_| format!("{name} is unavailable in system bin/sbin directories"))?;
    let mut command = Command::new(path);
    command.args(args);
    let bytes = query_output(command, Duration::from_secs(3), 2 * 1024 * 1024)?;
    let output = std::str::from_utf8(&bytes).map_err(|_| "package query returned invalid text")?;
    parse_inventory(backend, output)
}

fn parse_inventory(backend: PackageBackend, output: &str) -> Result<Inventory, String> {
    let mut inventory = Inventory::new();
    for line in output.lines() {
        let columns: Vec<_> = if backend == PackageBackend::Pacman {
            line.split_whitespace().collect()
        } else {
            line.split('\t').collect()
        };
        let expected = match backend {
            PackageBackend::Dpkg => 4,
            PackageBackend::Rpm => 3,
            PackageBackend::Pacman => 2,
        };
        if columns.len() != expected {
            return Err("package query returned an unrecognized record".into());
        }
        let name = columns[0];
        if !PACKAGES.contains(&name) {
            continue;
        }
        if backend == PackageBackend::Dpkg && columns[3] != "installed" {
            continue;
        }
        let version = columns[1];
        if version.is_empty()
            || version.len() > 128
            || !version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"+:~._-%".contains(&b))
        {
            return Err("package query returned an invalid version".into());
        }
        let key = if backend == PackageBackend::Pacman {
            name.to_owned()
        } else {
            let arch = columns[2];
            if arch.is_empty()
                || arch.len() > 32
                || !arch
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err("package query returned an invalid architecture".into());
            }
            format!("{name}:{arch}")
        };
        inventory.insert(key, version.to_owned());
    }
    Ok(inventory)
}

/// The child has no interactive input, inherited environment, raw descriptors
/// or root identity. Database queries are read-only and have a shared deadline.
fn query_output(
    mut command: Command,
    timeout: Duration,
    maximum: usize,
) -> Result<Vec<u8>, String> {
    command
        .env_clear()
        .env("LC_ALL", "C")
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    // SAFETY: the callback uses only fixed arguments and async-signal-safe
    // syscalls. CLOEXEC retains Command's error pipe until exec finishes.
    unsafe {
        command.pre_exec(|| {
            if libc::syscall(
                libc::SYS_close_range,
                3_u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::geteuid() == 0
                && (libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0) != 0
                    || libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setresgid(65534, 65534, 65534) != 0
                    || libc::setresuid(65534, 65534, 65534) != 0)
            {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|_| "could not start the unprivileged package query")?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    // SAFETY: fcntl operates on this owned pipe and changes only O_NONBLOCK.
    let flags = unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        stop_query(&mut child);
        return Err("could not bound package-query output".into());
    }
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut eof = false;
    loop {
        let mut chunk = [0; 8192];
        loop {
            if Instant::now() >= deadline {
                stop_query(&mut child);
                return Err("package query exceeded its deadline".into());
            }
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) if bytes.len() + n <= maximum => bytes.extend_from_slice(&chunk[..n]),
                Ok(_) => {
                    stop_query(&mut child);
                    return Err("package query exceeded its output limit".into());
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    stop_query(&mut child);
                    return Err("could not read package-query output".into());
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) if eof => {
                return if status.success() {
                    Ok(bytes)
                } else {
                    Err("package database query failed; installed state is unknown".into())
                };
            }
            Ok(_) => (),
            Err(_) => {
                stop_query(&mut child);
                return Err("could not wait for package query".into());
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn stop_query(child: &mut Child) {
    // SAFETY: the child starts its own process group. Include descendants that
    // could otherwise keep stdout open after their parent has exited.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
}

fn package_row(key: &str, label: &str, names: &[&str], inventory: &Inventory) -> Value {
    let mut native = Vec::new();
    let mut foreign = Vec::new();
    for (name, version) in inventory {
        let (base, arch) = name.split_once(':').unwrap_or((name, ""));
        if !names.contains(&base) {
            continue;
        }
        let entry = format!("{name} {version}");
        if ["", "amd64", "all", "x86_64", "noarch"].contains(&arch) {
            native.push(entry);
        } else {
            foreign.push(entry);
        }
    }
    let mut row = if !native.is_empty() {
        native.extend(foreign);
        field(key, label, Ok(native.join(", ")))
    } else if !foreign.is_empty() {
        warning(
            key,
            label,
            &format!(
                "only foreign-architecture packages found: {}; native x86_64 support not confirmed",
                foreign.join(", ")
            ),
        )
    } else {
        warning(
            key,
            label,
            &format!(
                concat!(
                    "optional package not installed: {}; see ",
                    doc_url!("guide/graphics.md")
                ),
                names.join(" or ")
            ),
        )
    };
    row["candidates"] = serde_json::json!(names);
    row
}

fn gpu(sources: &impl Sources, fields: &mut Vec<Value>) {
    let mut found = false;
    if let Ok(entries) = sources.entries("/sys/class/drm") {
        for card in entries
            .iter()
            .filter(|name| {
                name.strip_prefix("card")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
            .take(8)
        {
            let root = format!("/sys/class/drm/{card}/device");
            if text(sources, &format!("{root}/vendor")).as_deref() != Ok("0x8086") {
                continue;
            }
            found = true;
            let driver = sources
                .link_target(&format!("{root}/driver"))
                .and_then(|target| {
                    target
                        .rsplit('/')
                        .next()
                        .filter(|s| !s.is_empty())
                        .map(|s| format!("Intel {card}; bound kernel driver={}", clean(s)))
                        .ok_or(Missing::Invalid)
                });
            fields.push(field(
                &format!("{card}.driver"),
                "Intel GPU kernel driver",
                driver,
            ));
        }
    }
    if !found {
        fields.push(note(
            "gpu",
            "Intel GPU kernel driver",
            "Intel DRM card not confirmed from sysfs; no device opened",
        ));
    }
    fields.push(note("acceleration", "Actual acceleration", "package presence and a bound driver do not prove rendering or video decoding is accelerated; check vainfo and the application's playback diagnostics manually"));
}

pub(super) fn collect(sources: &impl Sources, os: &Result<OsRelease, Missing>) -> Value {
    let mut fields = vec![note(
        "scope",
        "Package advice scope",
        "Optional desktop/media recommendations, not controller-service prerequisites. No package or desktop changes are performed.",
    )];
    gpu(sources, &mut fields);
    let os = match os {
        Ok(os) => os,
        Err(error) => {
            fields.push(field(
                "package_profile",
                "Graphics package profile",
                Err(*error),
            ));
            return section("graphics", "Optional Intel graphics setup", fields);
        }
    };
    let identity = format!(
        "{} {}{}",
        os.id,
        os.version,
        if os.like.is_empty() {
            String::new()
        } else {
            format!(" (ID_LIKE={})", os.like)
        }
    );
    fields.push(field("distribution", "Package distribution", Ok(identity)));
    let Some(backend) = backend(os) else {
        fields.push(note("package_profile", "Graphics package profile", "no package recipe for this distribution ID; check its package documentation manually. ID_LIKE alone is not proof of package compatibility"));
        return section("graphics", "Optional Intel graphics setup", fields);
    };
    let recipe = recipe_version(os);
    if recipe != "rolling" && recipe != os.version {
        fields.push(warning("package_profile", "Graphics package profile", &format!(
            "unverified on {} {}; using package names checked on {} {}. Verify repository candidates before installing or replacing packages",
            os.id, os.version, os.id, recipe
        )));
    } else {
        fields.push(field(
            "package_profile",
            "Graphics package profile",
            Ok(format!("{} {recipe} package recipe", os.id)),
        ));
    }
    let inventory = match sources.packages(backend) {
        Ok(inventory) => inventory,
        Err(error) => {
            fields.push(note(
                "package_inventory",
                "Installed graphics packages",
                &format!(
                    "query unavailable: {}; missing packages cannot be determined",
                    clean(&error).chars().take(120).collect::<String>()
                ),
            ));
            return section("graphics", "Optional Intel graphics setup", fields);
        }
    };
    let names: [&[&str]; 5] = match backend {
        PackageBackend::Dpkg => [
            &["libgl1-mesa-dri"],
            &["mesa-vulkan-drivers"],
            &["intel-media-va-driver", "intel-media-va-driver-non-free"],
            match (os.id.as_str(), recipe) {
                ("debian", _) => &["firmware-intel-graphics"],
                ("ubuntu", "26.04") => &["linux-firmware-intel-graphics", "linux-firmware"],
                _ => &["linux-firmware"],
            },
            &["vainfo"],
        ],
        PackageBackend::Rpm => [
            &["mesa-dri-drivers"],
            &["mesa-vulkan-drivers"],
            &["libva-intel-media-driver", "intel-media-driver"],
            &["intel-gpu-firmware"],
            &["libva-utils"],
        ],
        PackageBackend::Pacman => [
            &["mesa"],
            &["vulkan-intel"],
            &["intel-media-driver"],
            &["linux-firmware-intel"],
            &["libva-utils"],
        ],
    };
    for ((key, label), names) in [
        ("mesa_gl", "Mesa OpenGL"),
        ("mesa_vulkan", "Mesa Vulkan"),
        ("intel_media", "Intel VA-API media driver"),
        ("intel_firmware", "Intel graphics firmware"),
        ("vainfo", "Optional VA-API inspection tool"),
    ]
    .into_iter()
    .zip(names)
    {
        fields.push(package_row(key, label, names, &inventory));
    }
    let missing = fields
        .iter()
        .filter(|row| row["state"] == "warning")
        .count();
    fields.push(if missing == 0 {
        field("package_status", "Optional graphics packages", Ok("native package candidates found for Mesa, Vulkan, VA-API, Intel firmware and vainfo; runtime acceleration still needs an application check".into()))
    } else {
        note("package_status", "Optional graphics packages", &format!("{missing} recommended package groups not confirmed; review the advice below. This does not make the controller service unhealthy"))
    });
    match backend {
        PackageBackend::Dpkg => {
            fields.push(note("media_variants", "Media build variants", "intel-media-va-driver is the Free Kernel build; intel-media-va-driver-non-free is Full Feature with broader codec coverage. Both are separate from Mesa and the kernel driver"));
            fields.push(note("media_choice", "Choosing a media build", "These media variants conflict. Choose one through your distribution's enabled repositories and review APT's proposed replacement; no package is removed by doctor"));
        }
        PackageBackend::Rpm => {
            fields.push(note("media_variants", "Fedora media variants", "Fedora libva-intel-media-driver is the free build. RPM Fusion nonfree intel-media-driver provides the full build. These are VA-API drivers, separate from Mesa"));
            fields.push(note("media_choice", "Choosing a media build", "Fedora 40+ packaging supports coexistence of the free and full Intel media builds. Do not blindly swap Mesa or remove the free driver; follow the current RPM Fusion guide"));
            for (key, label, names) in [
                (
                    "ffmpeg",
                    "Optional FFmpeg codecs",
                    &[
                        "ffmpeg-free",
                        "ffmpeg",
                        "ffmpeg-libs",
                        "libavcodec-freeworld",
                    ][..],
                ),
                (
                    "openh264",
                    "Optional OpenH264 codecs",
                    &["openh264", "mozilla-openh264", "gstreamer1-plugin-openh264"][..],
                ),
            ] {
                let mut row = package_row(key, label, names, &inventory);
                // Optional codecs are advice, never daemon prerequisites.
                if row["state"] == "warning" {
                    row["state"] = Value::String("unknown".into());
                }
                fields.push(row);
            }
            fields.push(note("openh264_scope", "OpenH264 scope", "OpenH264 provides H.264 software codec support, not an Intel GPU driver or general VA-API enablement"));
            fields.push(note("ffmpeg_scope", "FFmpeg choices", "RPM Fusion libavcodec-freeworld complements Fedora codecs. Full ffmpeg conflicts with ffmpeg-free and needs a manually reviewed package transaction"));
            fields.push(note("application_scope", "Application packaging", "Host codec packages may not cover Flatpak or other bundled applications. Check the application's own playback diagnostics"));
        }
        PackageBackend::Pacman => fields.push(note(
            "media_variants",
            "Arch/CachyOS packages",
            "use the distribution repositories; no AUR GPU driver is required for this Intel setup",
        )),
    }
    fields.push(note(
        "settings",
        "Driver settings",
        "no packages, GuC/HuC options, Mesa settings, desktop packages or boot files were changed",
    ));
    section("graphics", "Optional Intel graphics setup", fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(sources: &impl Sources) -> Value {
        super::collect(sources, &super::super::os_release(sources))
    }

    struct Fake {
        os: &'static str,
        inventory: Result<Inventory, String>,
    }
    impl Sources for Fake {
        fn read(&self, path: &str) -> Result<String, Missing> {
            match path {
                "/etc/os-release" => Ok(self.os.into()),
                "/sys/class/drm/card0/device/vendor" => Ok("0x8086\n".into()),
                _ => Err(Missing::Absent),
            }
        }
        fn entries(&self, path: &str) -> Result<Vec<String>, Missing> {
            if path == "/sys/class/drm" {
                Ok(vec!["card0".into(), "card0-DSI-1".into()])
            } else {
                Err(Missing::Absent)
            }
        }
        fn runtime(&self) -> Result<Value, Missing> {
            Err(Missing::Absent)
        }
        fn link_target(&self, path: &str) -> Result<String, Missing> {
            assert_eq!(path, "/sys/class/drm/card0/device/driver");
            Ok("../../../bus/pci/drivers/i915".into())
        }
        fn packages(&self, _: PackageBackend) -> Result<Inventory, String> {
            self.inventory.clone()
        }
    }
    fn row(report: &Value, key: &str) -> Value {
        report["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == key)
            .unwrap()
            .clone()
    }

    #[test]
    fn os_release_is_data_and_unknown_versions_are_explicitly_unverified() {
        for value in [
            "ID=ubuntu\nVERSION_ID=26.04\nID=debian",
            "ID=\"$(touch /tmp/no)\"",
            "ID=ubuntu\nVERSION_ID=\"26.04",
        ] {
            assert_eq!(OsRelease::parse(value).unwrap_err(), Missing::Invalid);
        }
        for os in [
            "ID=ubuntu\nVERSION_ID=22.04",
            "ID=example\nID_LIKE=arch",
            "ID=fedora\nVERSION_ID=42",
        ] {
            let fake = Fake {
                os,
                inventory: Err("must not infer missing packages".into()),
            };
            let report = collect(&fake);
            assert_eq!(
                row(&report, "package_profile")["state"],
                if os.contains("ID=example") {
                    "unknown"
                } else {
                    "warning"
                }
            );
            assert!(
                !report
                    .to_string()
                    .contains("optional package not installed")
            );
        }
    }

    #[test]
    fn newer_same_distribution_uses_a_marked_recipe_without_id_like_guessing() {
        for os in ["ID=fedora\nVERSION_ID=45", "ID=ubuntu\nVERSION_ID=26.10"] {
            let report = collect(&Fake {
                os,
                inventory: Ok(Inventory::new()),
            });
            let profile = row(&report, "package_profile");
            assert_eq!(profile["state"], "warning");
            assert!(
                profile["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("unverified on ")
            );
            assert_eq!(row(&report, "mesa_gl")["state"], "warning");
        }
    }

    #[test]
    fn failed_database_is_unknown_not_a_list_of_missing_packages() {
        let report = collect(&Fake {
            os: "ID=fedora\nVERSION_ID=44",
            inventory: Err("permission denied".into()),
        });
        assert_eq!(row(&report, "package_inventory")["state"], "unknown");
        assert!(
            !report
                .to_string()
                .contains("optional package not installed")
        );
        assert!(
            row(&report, "card0.driver")["value"]
                .as_str()
                .unwrap()
                .contains("i915")
        );
    }

    #[test]
    fn dpkg_status_multiarch_and_variants_are_reported_without_unrelated_names() {
        let inventory = parse_inventory(PackageBackend::Dpkg, "private-app\t1.0\tamd64\tinstalled\nintel-media-va-driver\t25.2.3+dfsg1-1\ti386\tinstalled\nintel-media-va-driver-non-free\t25.2.3+ds1-1\tamd64\tinstalled\nlibgl1-mesa-dri\t25.0.7-2\tamd64\tconfig-files\nlibgl1-mesa-dri\t\tamd64\tnot-installed\n").unwrap();
        assert_eq!(inventory.len(), 2);
        assert!(!inventory.contains_key("private-app:amd64"));
        let report = collect(&Fake {
            os: "ID=debian\nVERSION_ID=13",
            inventory: Ok(inventory),
        });
        let media = row(&report, "intel_media");
        assert_eq!(media["state"], "ok");
        assert!(media["value"].as_str().unwrap().contains("non-free:amd64"));
        assert_eq!(row(&report, "mesa_gl")["state"], "warning");
        assert!(
            row(&report, "media_choice")["message"]
                .as_str()
                .unwrap()
                .contains("conflict")
        );
        let only_foreign = BTreeMap::from([("mesa:i686".into(), "26.0".into())]);
        assert_eq!(
            package_row("gl", "GL", &["mesa"], &only_foreign)["state"],
            "warning"
        );
    }

    #[test]
    fn ubuntu_firmware_split_and_fedora_codec_boundaries_are_explicit() {
        let report = collect(&Fake {
            os: "ID=ubuntu\nVERSION_ID=26.04",
            inventory: Ok(BTreeMap::from([("linux-firmware:all".into(), "1".into())])),
        });
        assert_eq!(row(&report, "intel_firmware")["state"], "ok");
        let report = collect(&Fake {
            os: "ID=ubuntu\nVERSION_ID=26.04",
            inventory: Ok(BTreeMap::new()),
        });
        assert!(
            row(&report, "intel_firmware")["message"]
                .as_str()
                .unwrap()
                .contains("linux-firmware-intel-graphics")
        );
        let inventory = parse_inventory(PackageBackend::Rpm, "intel-media-driver\t0:26.1.5-1.fc44\tx86_64\nlibva-intel-media-driver\t0:25.4.6-2.fc44\tx86_64\n").unwrap();
        let report = collect(&Fake {
            os: "ID=fedora\nVERSION_ID=44",
            inventory: Ok(inventory),
        });
        assert_eq!(row(&report, "intel_media")["state"], "ok");
        assert_eq!(row(&report, "ffmpeg")["state"], "unknown");
        assert!(
            row(&report, "media_choice")["message"]
                .as_str()
                .unwrap()
                .contains("coexistence")
        );
        assert!(
            row(&report, "acceleration")["message"]
                .as_str()
                .unwrap()
                .contains("do not prove")
        );
    }

    #[test]
    fn query_capture_is_bounded_and_nonzero_exit_is_unknown() {
        let mut output = Command::new("/usr/bin/printf");
        output.arg("fixture");
        assert_eq!(
            query_output(output, Duration::from_secs(1), 64).unwrap(),
            b"fixture"
        );
        let mut output = Command::new("/usr/bin/printf");
        output.arg("too much output");
        assert!(
            query_output(output, Duration::from_secs(1), 2)
                .unwrap_err()
                .contains("output limit")
        );
        assert!(
            query_output(Command::new("/usr/bin/false"), Duration::from_secs(1), 64)
                .unwrap_err()
                .contains("unknown")
        );
        let mut wait = Command::new("/usr/bin/sleep");
        wait.arg("5");
        let start = Instant::now();
        assert!(
            query_output(wait, Duration::from_millis(30), 64)
                .unwrap_err()
                .contains("deadline")
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn static_advice_survives_the_report_message_limit() {
        for os in [
            "ID=debian\nVERSION_ID=13",
            "ID=ubuntu\nVERSION_ID=26.04",
            "ID=fedora\nVERSION_ID=44",
            "ID=cachyos",
            "ID=unknown",
        ] {
            let report = collect(&Fake {
                os,
                inventory: Ok(Inventory::new()),
            });
            for row in report["fields"].as_array().unwrap() {
                if let Some(message) = row["message"].as_str() {
                    assert!(message.chars().count() <= 256, "{}: {message}", row["key"]);
                }
            }
        }
    }

    #[test]
    fn a_root_query_child_has_no_root_identity_groups_or_capabilities() {
        // Ordinary CI checks the unprivileged path; a disposable root container
        // can run this same assertion without changing the host's accounts.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let mut command = Command::new("/usr/bin/cat");
        command.arg("/proc/self/status");
        let output = query_output(command, Duration::from_secs(1), 64 * 1024).unwrap();
        let status = std::str::from_utf8(&output).unwrap();
        let value = |key| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .unwrap()
                .trim()
        };
        for key in ["Uid:", "Gid:"] {
            assert_eq!(
                value(key).split_whitespace().collect::<Vec<_>>(),
                ["65534"; 4]
            );
        }
        assert_eq!(value("Groups:"), "");
        assert_eq!(value("NoNewPrivs:"), "1");
        for key in ["CapPrm:", "CapEff:", "CapAmb:"] {
            assert_eq!(u64::from_str_radix(value(key), 16).unwrap(), 0);
        }
    }

    #[test]
    #[ignore = "subprocess fixture for query_deadline_includes_descendant_held_stdout"]
    #[allow(
        clippy::zombie_processes,
        reason = "the child intentionally outlives this fixture; the outer runner terminates the process group"
    )]
    fn query_descendant_fixture() {
        // The test process exits first. This child keeps the capture pipe open
        // in the same process group; no shell or hardware interface is used.
        let _child = Command::new("/usr/bin/sleep")
            .arg("2")
            .stdout(Stdio::inherit())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
    }

    #[test]
    fn query_deadline_includes_descendant_held_stdout() {
        // Linux's self magic link avoids copying an executable while another
        // parallel test can fork with the temporary writer still inherited.
        // It also works after UID drop when the checkout itself is private.
        let mut command = Command::new("/proc/self/exe");
        command.args([
            "--exact",
            "doctor::graphics::tests::query_descendant_fixture",
            "--ignored",
            "--nocapture",
        ]);
        let start = Instant::now();
        let result = query_output(command, Duration::from_millis(250), 64 * 1024);
        let error = result.unwrap_err();
        assert!(error.contains("deadline"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
