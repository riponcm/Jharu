//! The names Jharu will not stop, switch off or close: security software,
//! hardware drivers, and parts of Windows and macOS.
//!
//! Shared by the Startup apps tab (`startup.rs`) and the Task killer
//! (`processes.rs`), so both refuse the same things for the same reasons.
//! Matching works on normalized text and whole words, so "eset" protects
//! "ESET Security" but not "Preset Manager".

/// Lowercase with every run of non-alphanumerics as one space, so
/// "Intel(R) Graphics" and "intel r graphics" compare equal.
pub(crate) fn normalize(s: &str) -> String {
    let mut out = String::new();
    let mut gap = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            gap = false;
        } else if !gap {
            out.push(' ');
            gap = true;
        }
    }
    out.trim_end().to_string()
}

/// `needle` begins at the start of a word in `hay`: "eset" matches "ESET
/// Security" but not "Preset Manager" or "Reset Tool".
pub(crate) fn starts_word(hay: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let at = from + i;
        if at == 0 || hay.as_bytes()[at - 1] == b' ' {
            return true;
        }
        from = at + 1;
    }
    false
}

pub(crate) const SECURITY_SOFTWARE: &[&str] = &[
    "securityhealth", "sechealth", "windows security", "windows defender", "windowsdefender", "defender", "msmpeng",
    "mpcmdrun", "antimalware", "avast", "avg", "avira", "kaspersky", "norton", "mcafee", "bitdefender", "malwarebytes",
    "eset", "sophos", "crowdstrike", "sentinelone", "webroot", "trend micro", "f secure", "cylance", "carbon black",
    "comodo", "zonealarm", "emsisoft", "xprotect", "globalprotect", "palo alto", "pangps", "pangpa", "pangpsd",
    "zscaler", "netskope", "forticlient", "anyconnect", "little snitch",
];

pub(crate) const HARDWARE_DRIVERS: &[&str] = &[
    "realtek", "rthdvcpl", "rtkaud", "rtkngui", "nvidia", "nvcpl", "nvbackend", "nvtray", "nvdisplay", "igfx",
    "intel graphics", "intel r graphics", "intel r hd graphics", "intel r uhd graphics", "radeon", "amd software",
    "amdrs", "synaptics", "elantech", "touchpad", "bluetooth", "btmshellex", "maxxaudio", "waves", "dolby",
    "conexant", "hd audio", "audio manager", "audio driver", "sound blaster", "etdctrl", "syntp", "elan", "nahimic",
    "iaanotif", "intel r rapid", "intel r wireless", "wlan",
];

pub(crate) const WINDOWS_COMPONENTS: &[&str] = &["ctfmon", "userinit", "explorer", "sihost", "winlogon", "msctf"];

/// `path_lower` (lowercase, backslashes) is inside the Windows folder or one
/// of the Unix system folders.
pub(crate) fn in_system_dir(target_lower: &str) -> bool {
    let mut roots: Vec<String> = vec!["c:\\windows\\".into()];
    if let Ok(r) = std::env::var("SystemRoot") {
        roots.push(format!("{}\\", r.to_lowercase().trim_end_matches('\\')));
    }
    roots.iter().any(|r| target_lower.starts_with(r.as_str()))
        || ["/system/", "/usr/libexec/", "/usr/sbin/", "/usr/bin/", "/sbin/", "/usr/lib/", "/library/apple/"]
            .iter()
            .any(|p| target_lower.starts_with(p))
}
