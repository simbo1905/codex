fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-ObjC");
    }

    // Embed a build timestamp so fork builds are visually distinct from
    // upstream releases.  Format: YY.MMDD.hhmmss (e.g. 26.0506.143022).
    // The value is available in Rust source as env!("CODEX_BUILD_STAMP").
    // We rerun only when the build script itself changes — the stamp is
    // intentionally frozen per-build, not per-source-change.
    println!("cargo:rerun-if-changed=build.rs");
    let stamp = {
        use std::time::SystemTime;
        use std::time::UNIX_EPOCH;
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Convert Unix timestamp → UTC calendar fields (no external deps).
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let days = secs / 86400; // days since 1970-01-01
        // Gregorian calendar calculation
        let z = days + 719468;
        let era = z / 146097;
        let doe = z % 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let mo = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if mo <= 2 { y + 1 } else { y };
        format!("{:02}.{:02}{:02}.{:02}{:02}{:02}", y % 100, mo, d, h, m, s)
    };
    println!("cargo:rustc-env=CODEX_BUILD_STAMP={stamp}");
}
