use anyhow::{Context, Result};
use colored::Colorize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::env;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static VERBOSE: OnceLock<bool> = OnceLock::new();

pub fn set_verbose(enabled: bool) {
    VERBOSE.set(enabled).ok();
}

pub fn is_verbose() -> bool {
    VERBOSE.get().copied().unwrap_or(false)
}

pub fn verbose(message: impl std::fmt::Display) {
    if is_verbose() {
        eprintln!("{} {}", "│".dimmed(), message.to_string().dimmed());
    }
}

pub fn status(message: impl std::fmt::Display) {
    eprintln!("│ {message}");
}

pub fn status_info(message: impl std::fmt::Display) {
    eprintln!("{} {}", "│".blue(), message.to_string().blue());
}

pub fn status_success(message: impl std::fmt::Display) {
    eprintln!("{} {}", "│".green(), message.to_string().green());
}

pub fn status_warn(message: impl std::fmt::Display) {
    eprintln!(
        "{} {}",
        "│".bright_yellow(),
        message.to_string().bright_yellow()
    );
}

pub fn status_error(message: impl std::fmt::Display) {
    eprintln!("{} {}", "│".red(), message.to_string().red());
}

pub fn command_header(program: &str, args: &[impl std::fmt::Display]) {
    let args_str: Vec<String> = args.iter().map(ToString::to_string).collect();
    eprintln!(
        "\n{} {} {}",
        ">".bold(),
        program.italic(),
        args_str.join(" ").italic()
    );
    let width = console::Term::stderr().size().1 as usize;
    eprintln!("{}", "─".repeat(width).dimmed());
}

pub fn get_host_env() -> HashMap<String, String> {
    let forwarded_env_keys = [
        "COLORTERM",
        "DESKTOP_SESSION",
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SEAT",
        "XDG_SESSION_DESKTOP",
        "XDG_SESSION_ID",
        "XDG_SESSION_TYPE",
        "XDG_VTNR",
        "AT_SPI_BUS_ADDRESS",
        "LANG",
        "LANGUAGE",
        "LC_ALL",
        "LC_CTYPE",
        "LC_MESSAGES",
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "ftp_proxy",
        "FTP_PROXY",
        "no_proxy",
        "NO_PROXY",
    ];

    let mut env_vars = HashMap::new();

    for key in forwarded_env_keys {
        if let Ok(value) = env::var(key) {
            env_vars.insert(key.to_string(), value);
        }
    }

    env_vars
}

fn parse_a11y_address(address: &str) -> Result<(String, String)> {
    let mut s = address.trim().replace("('", "").replace("',)", "");
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        s = s[1..s.len() - 1].to_string();
    }

    let prefix = "unix:path=";
    if let Some(pos) = s.find(prefix) {
        let rest = &s[pos + prefix.len()..];
        match rest.split_once(',') {
            Some((p, sfx)) if !p.is_empty() => Ok((p.to_string(), format!(",{sfx}"))),
            Some((_p, _sfx)) => anyhow::bail!("Failed to parse a11y bus address"),
            None if !rest.is_empty() => Ok((rest.to_string(), String::new())),
            None => anyhow::bail!("Failed to parse a11y bus address"),
        }
    } else {
        anyhow::bail!("Failed to parse a11y bus address");
    }
}

pub fn get_a11y_bus_args() -> Result<Vec<String>> {
    let address = query_a11y_bus_address()?;
    let (unix_path, suffix) = parse_a11y_address(&address)?;

    Ok(vec![
        format!("--bind-mount=/run/flatpak/at-spi-bus={}", unix_path),
        if suffix.is_empty() {
            "--env=AT_SPI_BUS_ADDRESS=unix:path=/run/flatpak/at-spi-bus".to_string()
        } else {
            format!("--env=AT_SPI_BUS_ADDRESS=unix:path=/run/flatpak/at-spi-bus{suffix}")
        },
    ])
}

fn query_a11y_bus_address() -> Result<String> {
    // Prefer in-process D-Bus (no `gdbus` CLI). Fall back to gdbus if zbus fails
    // (e.g. missing session bus in odd environments).
    match query_a11y_bus_address_zbus() {
        Ok(address) => Ok(address),
        Err(zbus_error) => {
            verbose(format!("zbus a11y query failed ({zbus_error:#}); trying gdbus"));
            query_a11y_bus_address_gdbus()
        }
    }
}

fn query_a11y_bus_address_zbus() -> Result<String> {
    let conn = zbus::blocking::Connection::session()
        .context("Failed to connect to session bus for a11y")?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus",
    )
    .context("Failed to create a11y bus proxy")?;
    let address: String = proxy
        .call("GetAddress", &())
        .context("org.a11y.Bus.GetAddress failed")?;
    Ok(address)
}

fn query_a11y_bus_address_gdbus() -> Result<String> {
    let output = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest=org.a11y.Bus",
            "--object-path=/org/a11y/bus",
            "--method=org.a11y.Bus.GetAddress",
        ])
        .output()
        .context("Failed to execute gdbus")?;

    if !output.status.success() {
        anyhow::bail!("gdbus a11y bus query failed with status: {}", output.status);
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

pub fn build_font_config() -> Result<PathBuf> {
    let home = env::var("HOME").unwrap_or_default();
    let cache = env::var("XDG_CACHE_HOME").unwrap_or_else(|_| format!("{home}/.cache"));
    let data = env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
    let mapped_font_file = format!("{cache}/font-dirs.xml");
    let config_path = PathBuf::from(&mapped_font_file);

    let mut font_dir_content = String::from(
        "<?xml version=\"1.0\"?>\n\
         <!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
         <fontconfig>\n",
    );

    if path_exists("/usr/share/fonts") {
        font_dir_content
            .push_str("\t<remap-dir as-path=\"/usr/share/fonts\">/run/host/fonts</remap-dir>\n");
    }

    if path_exists("/usr/local/share/fonts") {
        font_dir_content.push_str(
            "\t<remap-dir as-path=\"/usr/local/share/fonts\">/run/host/local-fonts</remap-dir>\n",
        );
    }

    for user_font_dir in [format!("{data}/fonts"), format!("{home}/.fonts")] {
        if path_exists(&user_font_dir) {
            writeln!(
                font_dir_content,
                "\t<remap-dir as-path=\"{user_font_dir}\">/run/host/user-fonts</remap-dir>"
            )
            .ok();
        }
    }

    font_dir_content.push_str("</fontconfig>\n");
    std::fs::write(&mapped_font_file, font_dir_content).context("Failed to write font-dirs.xml")?;

    Ok(config_path)
}

pub fn get_fonts_args(font_config_path: &Path) -> Result<Vec<String>> {
    let mut args: Vec<String> = Vec::new();
    let home = env::var("HOME").unwrap_or_default();
    let cache = env::var("XDG_CACHE_HOME").unwrap_or_else(|_| format!("{home}/.cache"));
    let data = env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));

    if path_exists("/usr/share/fonts") {
        args.push("--bind-mount=/run/host/fonts=/usr/share/fonts".to_string());
    }

    if path_exists("/usr/local/share/fonts") {
        args.push("--bind-mount=/run/host/local-fonts=/usr/local/share/fonts".to_string());
    }

    for cache_dir in ["/usr/lib/fontconfig/cache", "/var/cache/fontconfig"] {
        if path_exists(cache_dir) {
            args.push(format!("--bind-mount=/run/host/fonts-cache={cache_dir}"));
            break;
        }
    }

    for user_font_dir in [format!("{data}/fonts"), format!("{home}/.fonts")] {
        if path_exists(&user_font_dir) {
            args.push(format!("--filesystem={user_font_dir}:ro"));
        }
    }

    let user_cache_dir = format!("{cache}/fontconfig");
    if path_exists(&user_cache_dir) {
        args.push(format!("--filesystem={user_cache_dir}:ro"));
        args.push(format!(
            "--bind-mount=/run/host/user-fonts-cache={user_cache_dir}"
        ));
    }

    let font_config_path = font_config_path
        .to_str()
        .context("Font config path contains invalid UTF-8")?;
    args.push(format!(
        "--bind-mount=/run/host/font-dirs.xml={font_config_path}"
    ));

    Ok(args)
}

pub fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .context("Path contains invalid UTF-8 characters")
}

pub fn download_file(url: &str, dest: &Path) -> Result<()> {
    let response = ureq::get(url)
        .call()
        .map_err(|error| anyhow::anyhow!("Failed to download {url}: {error}"))?;
    let mut file = std::fs::File::create(dest)?;
    std::io::copy(&mut response.into_body().into_reader(), &mut file)?;
    Ok(())
}

pub fn verify_sha256_hex(path: &Path, expected: &str) -> Result<()> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {} for hashing", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to read {} for hashing", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let result = hasher.finalize();
    let mut hash = String::with_capacity(64);
    for byte in result {
        write!(&mut hash, "{byte:02x}")?;
    }
    if hash != expected {
        return Err(anyhow::anyhow!(
            "SHA256 mismatch for {}: expected {expected}, got {hash}",
            path.display()
        ));
    }
    Ok(())
}

/// Move a file, falling back to copy+remove when rename fails across devices (EXDEV).
pub fn move_file(src: &Path, dest: &Path) -> Result<()> {
    match std::fs::rename(src, dest) {
        Ok(()) => Ok(()),
        Err(error)
            if error.raw_os_error() == Some(18) /* EXDEV on Linux */
                || error.kind() == std::io::ErrorKind::CrossesDevices =>
        {
            std::fs::copy(src, dest).with_context(|| {
                format!("Failed to copy {} -> {}", src.display(), dest.display())
            })?;
            std::fs::remove_file(src).ok();
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!("Failed to move {} -> {}", src.display(), dest.display())
        }),
    }
}

/// Recursively copy a directory tree.
pub fn copy_dir_all(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("Failed to create directory {}", dest.display()))?;
    for entry in walkdir::WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?;
        let dest_path = dest.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest_path)?;
        } else {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &dest_path).with_context(|| {
                format!(
                    "Failed to copy {} -> {}",
                    entry.path().display(),
                    dest_path.display()
                )
            })?;
        }
    }
    Ok(())
}

pub fn extract_archive(
    path: &Path,
    archive_type: &str,
    dest: &Path,
    strip_components: usize,
) -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let file = std::fs::File::open(path)?;

    match archive_type {
        "tar" => tar::Archive::new(file).unpack(&temp_dir)?,
        "tar-gzip" => {
            let decoder = flate2::read::GzDecoder::new(file);
            tar::Archive::new(decoder).unpack(&temp_dir)?;
        }
        "tar-xz" => {
            let decoder = xz2::read::XzDecoder::new(file);
            tar::Archive::new(decoder).unpack(&temp_dir)?;
        }
        "zip" => {
            let mut archive = zip::ZipArchive::new(file)?;
            archive.extract(&temp_dir)?;
        }
        other => {
            return Err(anyhow::anyhow!("Unsupported archive type: {other}"));
        }
    }

    std::fs::create_dir_all(dest)?;
    for entry in walkdir::WalkDir::new(&temp_dir).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(&temp_dir)?;
        let components: Vec<_> = rel.components().collect();
        if components.len() <= strip_components {
            continue;
        }
        let stripped: PathBuf = components[strip_components..].iter().collect();
        let dest_path = dest.join(&stripped);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest_path)?;
        } else {
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            move_file(entry.path(), &dest_path)?;
        }
    }

    Ok(())
}

fn ends_with_ignore_case(filename: &str, suffix: &str) -> bool {
    let f_len = filename.len();
    let s_len = suffix.len();
    f_len >= s_len && filename[f_len - s_len..].eq_ignore_ascii_case(suffix)
}

pub fn guess_archive_type(url_or_path: &str) -> String {
    if ends_with_ignore_case(url_or_path, ".tar.gz") || ends_with_ignore_case(url_or_path, ".tgz") {
        "tar-gzip".into()
    } else if ends_with_ignore_case(url_or_path, ".tar.xz")
        || ends_with_ignore_case(url_or_path, ".txz")
    {
        "tar-xz".into()
    } else if ends_with_ignore_case(url_or_path, ".zip") {
        "zip".into()
    } else if ends_with_ignore_case(url_or_path, ".tar") {
        "tar".into()
    } else {
        "tar-gzip".into()
    }
}

pub fn version_less_than(left: &str, right: &str) -> bool {
    let mut left_parts = left.split('.');
    let mut right_parts = right.split('.');
    loop {
        match (left_parts.next(), right_parts.next()) {
            (Some(left_part), Some(right_part)) => {
                let left_num: u32 = left_part.parse().unwrap_or(0);
                let right_num: u32 = right_part.parse().unwrap_or(0);
                match left_num.cmp(&right_num) {
                    std::cmp::Ordering::Less => return true,
                    std::cmp::Ordering::Greater => return false,
                    std::cmp::Ordering::Equal => {}
                }
            }
            (Some(_) | None, None) => return false,
            (None, Some(_)) => return true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[test]
    fn parse_ok() {
        let cases = [
            (
                "('unix:path=/run/user/1000/bus',)",
                "/run/user/1000/bus",
                "",
            ),
            (
                "('unix:path=/run/user/1000/bus,unix:abstract=/tmp/abc',)",
                "/run/user/1000/bus",
                ",unix:abstract=/tmp/abc",
            ),
            (
                "unix:path=/run/user/1000/bus,foo=bar",
                "/run/user/1000/bus",
                ",foo=bar",
            ),
        ];

        for (input, exp_path, exp_suffix) in cases {
            let (path, suffix) = parse_a11y_address(input).unwrap();
            assert_eq!(path, exp_path);
            assert_eq!(suffix, exp_suffix);
        }
    }

    #[test]
    fn parse_err() {
        assert!(parse_a11y_address("no unix path here").is_err());
        assert!(parse_a11y_address("unix:path=,foo=bar").is_err());
    }

    #[test]
    fn test_ends_with_ignore_case() {
        assert!(ends_with_ignore_case("archive.tar.gz", ".tar.gz"));
        assert!(ends_with_ignore_case("archive.TAR.GZ", ".tar.gz"));
        assert!(ends_with_ignore_case("archive.Tar.Gz", ".tar.gz"));
        assert!(!ends_with_ignore_case("archive.tar.gz", ".zip"));
        assert!(!ends_with_ignore_case("archive", ".tar.gz"));
        assert!(!ends_with_ignore_case("", ".tar.gz"));
        assert!(!ends_with_ignore_case("a.tar.gz", "extra_long_suffix"));
        assert!(ends_with_ignore_case("file.txz", ".txz"));
    }

    #[test]
    fn test_guess_archive_type() {
        assert_eq!(
            guess_archive_type("https://example.com/pkg.tar.gz"),
            "tar-gzip"
        );
        assert_eq!(guess_archive_type("pkg.tgz"), "tar-gzip");
        assert_eq!(guess_archive_type("pkg.TGZ"), "tar-gzip");
        assert_eq!(guess_archive_type("pkg.tar.xz"), "tar-xz");
        assert_eq!(guess_archive_type("pkg.txz"), "tar-xz");
        assert_eq!(guess_archive_type("pkg.zip"), "zip");
        assert_eq!(guess_archive_type("pkg.tar"), "tar");
        assert_eq!(guess_archive_type("pkg.unknown"), "tar-gzip");
        assert_eq!(guess_archive_type("noextension"), "tar-gzip");
    }

    #[test]
    fn test_version_less_than() {
        assert!(version_less_than("1.0", "2.0"));
        assert!(version_less_than("1.0.0", "1.0.1"));
        assert!(!version_less_than("2.0", "1.0"));
        assert!(!version_less_than("1.0.0", "1.0.0"));
        assert!(!version_less_than("1.0.0", "1.0"));
        assert!(version_less_than("0.9", "1.0"));
        assert!(!version_less_than("10.0", "2.0"));
        assert!(!version_less_than("1.0", "foo"));
        assert!(version_less_than("foo", "1.0"));
        assert!(version_less_than("1", "1.0"));
        assert!(version_less_than("1.0", "1.0.0"));
    }

    #[test]
    fn test_verify_sha256_hex_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, b"hello world").unwrap();
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert!(verify_sha256_hex(&path, expected).is_ok());
    }

    #[test]
    fn test_verify_sha256_hex_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, b"hello world").unwrap();
        assert!(
            verify_sha256_hex(
                &path,
                "0000000000000000000000000000000000000000000000000000000000000000"
            )
            .is_err()
        );
    }

    #[test]
    fn test_path_to_str() {
        let dir = tempfile::tempdir().unwrap();
        let result = path_to_str(dir.path());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), dir.path().to_str().unwrap());
    }

    #[test]
    fn test_download_and_extract_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("test.zip");
        let extract_dir = dir.path().join("extracted");

        let empty_zip: &[u8] = &[
            0x50, 0x4B, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let mut server = Server::new();
        let mock = server
            .mock("GET", "/test.zip")
            .with_status(200)
            .with_body(empty_zip)
            .create();

        let url = format!("{}/test.zip", server.url());

        download_file(&url, &archive_path).expect("Download failed");
        assert!(archive_path.exists());

        verify_sha256_hex(
            &archive_path,
            "8739c76e681f900923b900c9df0ef75cf421d39cabb54650c4b9ad19b6a76d85",
        )
        .expect("SHA256 mismatch");

        extract_archive(&archive_path, "zip", &extract_dir, 0).expect("Extraction failed");
        assert!(extract_dir.exists());

        mock.assert();
    }

    #[test]
    fn test_download_file_http_error() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("missing.zip");

        let mut server = Server::new();
        let mock = server.mock("GET", "/missing.zip").with_status(404).create();

        let url = format!("{}/missing.zip", server.url());

        assert!(download_file(&url, &archive_path).is_err());
        mock.assert();
    }

    #[test]
    fn test_copy_dir_all() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let nested = src.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("file.txt"), b"hello").unwrap();

        let dest_root = dest.path().join("copy");
        copy_dir_all(src.path(), &dest_root).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest_root.join("a/b/file.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn test_move_file_same_fs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dest = dir.path().join("dest.bin");
        std::fs::write(&src, b"data").unwrap();
        move_file(&src, &dest).unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"data");
    }

    #[test]
    fn test_shell_words_roundtrip_quotes() {
        let argv = shell_words::split(r#"echo "hello world" '--flag=a b'"#).unwrap();
        assert_eq!(argv, ["echo", "hello world", "--flag=a b"]);
    }
}
