//! Fixed installation and runtime locations shared by the controller and tools.

pub const RUN_DIR: &str = "/run/minibookx-ctl";
pub const CONFIG: &str = "/etc/minibookx-ctl/config.conf";
pub const BINARY: &str = "/usr/local/bin/minibookx-ctl";
pub const UNIT: &str = "minibookx-ctl.service";
pub const UNIT_FILE: &str = "/etc/systemd/system/minibookx-ctl.service";
pub const DOC_DIR: &str = "/usr/local/share/doc/minibookx-ctl";
pub const RAPL_PACKAGE: &str = "/sys/class/powercap/intel-rapl:0";
