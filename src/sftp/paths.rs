use super::*;

pub fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "/" {
        format!("/{name}")
    } else if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent directory, clamped at `/`.
pub fn parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
        // Relative path such as `foo` or `.`.
        None => ".".to_string(),
    }
}

/// Clean up an absolute path reported by the shell.
///
/// OSC 7 always carries a path, but it may have a trailing slash, repeated
/// separators, or `.`/`..` segments (a shell that does not normalise `$PWD`).
/// Returns `None` for anything that is not absolute, since following a
/// relative path would be meaningless here.
pub fn normalize_shell_path(path: &str) -> Option<String> {
    let path = path.trim();
    if !path.starts_with('/') {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Some("/".to_string());
    }
    Some(format!("/{}", parts.join("/")))
}

/// Directories first, then case-insensitive name.
pub fn sort_entries(entries: &mut [RemoteEntry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            })
            .then_with(|| a.name.cmp(&b.name))
    });
}

pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64 / KIB;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn format_age(when: SystemTime) -> String {
    let Ok(elapsed) = SystemTime::now().duration_since(when) else {
        return "in the future".to_string();
    };

    let secs = elapsed.as_secs();
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        86_400..=2_591_999 => format!("{}d ago", secs / 86_400),
        _ => format!("{}y ago", secs / 31_536_000),
    }
}

/// Local timestamp shown on every row.
pub fn format_timestamp(when: Option<SystemTime>) -> String {
    match when {
        Some(when) => {
            let local: chrono::DateTime<chrono::Local> = when.into();
            local.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "unknown".to_string(),
    }
}

/// Owner text for a row.
pub fn format_owner(entry: &RemoteEntry) -> &str {
    if entry.owner.is_empty() {
        "unknown"
    } else {
        &entry.owner
    }
}

/// Reject names that cannot be a single path component.
pub fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Enter a name".to_string());
    }
    if name == "." || name == ".." {
        return Err("That name is reserved".to_string());
    }
    if name.contains('/') {
        return Err("A name cannot contain '/'".to_string());
    }
    if name.contains('\0') {
        return Err("A name cannot contain a NUL byte".to_string());
    }
    Ok(())
}

pub(super) fn copy_target_allowed(
    clipboard: &ClipboardEntry,
    dest_dir: &str,
) -> Result<(), String> {
    if !clipboard.is_dir {
        return Ok(());
    }
    let source = clipboard.path.trim_end_matches('/');
    let destination = dest_dir.trim_end_matches('/');
    if destination == source || destination.starts_with(&format!("{source}/")) {
        return Err(format!(
            "cannot copy folder '{}' into itself",
            clipboard.name
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_dir: bool) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            path: format!("/tmp/{name}"),
            is_dir,
            is_symlink: false,
            size: 0,
            owner: "alice".to_string(),
            modified: None,
        }
    }

    #[test]
    fn joins_remote_paths() {
        assert_eq!(join_path("/home/user", "file"), "/home/user/file");
        assert_eq!(join_path("/home/user/", "file"), "/home/user/file");
        assert_eq!(join_path("/", "etc"), "/etc");
        assert_eq!(join_path("", "etc"), "/etc");
        assert_eq!(join_path(".", "file"), "./file");
    }

    #[test]
    fn walks_to_the_parent_and_stops_at_root() {
        assert_eq!(parent_path("/home/user"), "/home");
        assert_eq!(parent_path("/home/user/"), "/home");
        assert_eq!(parent_path("/home"), "/");
        assert_eq!(parent_path("/"), "/");
        assert_eq!(parent_path(""), "/");
        assert_eq!(parent_path("file"), ".");
        assert_eq!(parent_path("."), ".");
    }

    #[test]
    fn normalises_shell_directories() {
        assert_eq!(
            normalize_shell_path("/home/me").as_deref(),
            Some("/home/me")
        );
        assert_eq!(
            normalize_shell_path("/home/me/").as_deref(),
            Some("/home/me")
        );
        assert_eq!(
            normalize_shell_path("//home///me").as_deref(),
            Some("/home/me")
        );
        assert_eq!(
            normalize_shell_path("/home/me/../you").as_deref(),
            Some("/home/you")
        );
        assert_eq!(normalize_shell_path("/.//").as_deref(), Some("/"));
        assert_eq!(normalize_shell_path("/..").as_deref(), Some("/"));
        assert_eq!(
            normalize_shell_path("  /var/log  ").as_deref(),
            Some("/var/log")
        );
        // Relative paths cannot be followed meaningfully.
        assert_eq!(normalize_shell_path("relative/dir"), None);
        assert_eq!(normalize_shell_path("~/src"), None);
        assert_eq!(normalize_shell_path(""), None);
    }

    #[test]
    fn sorts_directories_first_then_names_case_insensitively() {
        let mut entries = vec![
            entry("zeta.txt", false),
            entry("Beta", true),
            entry("alpha.txt", false),
            entry("alpha", true),
        ];
        sort_entries(&mut entries);

        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Beta", "alpha.txt", "zeta.txt"]);
    }

    #[test]
    fn formats_human_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(10 * 1024), "10 KiB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn formats_relative_ages() {
        let now = SystemTime::now();
        assert_eq!(
            format_age(now - std::time::Duration::from_secs(5)),
            "just now"
        );
        assert_eq!(
            format_age(now - std::time::Duration::from_secs(120)),
            "2m ago"
        );
        assert_eq!(
            format_age(now - std::time::Duration::from_secs(3 * 3600)),
            "3h ago"
        );
        assert_eq!(
            format_age(now - std::time::Duration::from_secs(2 * 86_400)),
            "2d ago"
        );
    }

    #[test]
    fn formats_local_timestamps_and_unknown_values() {
        assert_eq!(format_timestamp(None), "unknown");
        let stamp = format_timestamp(Some(UNIX_EPOCH));
        assert_eq!(stamp.len(), "1970-01-01 00:00".len());
        assert!(stamp.contains('-'));
        assert!(stamp.contains(':'));
    }

    #[test]
    fn validates_single_component_names() {
        assert!(validate_name("report.txt").is_ok());
        assert!(validate_name("with space").is_ok());
        assert!(validate_name("  trimmed  ").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("   ").is_err());
        assert!(validate_name(".").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("nul\0byte").is_err());
    }

    #[test]
    fn refuses_to_copy_a_folder_into_itself() {
        let folder = ClipboardEntry {
            path: "/srv/data".to_string(),
            name: "data".to_string(),
            is_dir: true,
        };
        assert!(copy_target_allowed(&folder, "/srv").is_ok());
        assert!(copy_target_allowed(&folder, "/srv/data").is_err());
        assert!(copy_target_allowed(&folder, "/srv/data/child").is_err());

        let file = ClipboardEntry {
            path: "/srv/data.txt".to_string(),
            name: "data.txt".to_string(),
            is_dir: false,
        };
        assert!(copy_target_allowed(&file, "/srv/data.txt").is_ok());
    }
}
