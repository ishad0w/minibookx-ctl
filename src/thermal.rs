use std::{fs, io, path::Path};

/// Inventory only: does not arm trips, change governors, or subscribe to events.
pub fn print_package_capabilities(root: &Path) -> io::Result<()> {
    let mut found = false;
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if fs::read_to_string(path.join("type"))
            .ok()
            .is_none_or(|s| s.trim() != "x86_pkg_temp")
        {
            continue;
        }
        found = true;
        println!("package_thermal_zone={}", path.display());
        for name in [
            "temp",
            "policy",
            "available_policies",
            "trip_point_0_temp",
            "trip_point_0_type",
            "trip_point_1_temp",
            "trip_point_1_type",
        ] {
            println!("{name}={}", fs::read_to_string(path.join(name))?.trim());
        }
    }
    if !found {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no x86_pkg_temp thermal zone",
        ));
    }
    println!("event_delivery=not_tested_by_this_command");
    println!("threshold_-274000_means=disabled_or_invalid_not_a_real_temperature");
    Ok(())
}
