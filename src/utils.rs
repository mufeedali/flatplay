use anyhow::{Context, Result};
use colored::Colorize;
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static VERBOSE: OnceLock<bool> = OnceLock::new();

pub fn set_verbose(enabled: bool) {
    let _ = VERBOSE.set(enabled);
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
    let args_str: Vec<String> = args.iter().map(|a| a.to_string()).collect();
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
            Some((p, sfx)) if !p.is_empty() => Ok((p.to_string(), format!(",{}", sfx))),
            Some((_p, _sfx)) => anyhow::bail!("Failed to parse a11y bus address"),
            None if !rest.is_empty() => Ok((rest.to_string(), String::new())),
            None => anyhow::bail!("Failed to parse a11y bus address"),
        }
    } else {
        anyhow::bail!("Failed to parse a11y bus address");
    }
}

pub fn get_a11y_bus_args() -> Result<Vec<String>> {
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

    let address = String::from_utf8_lossy(&output.stdout);
    let (unix_path, suffix) = parse_a11y_address(&address)?;

    Ok(vec![
        format!("--bind-mount=/run/flatpak/at-spi-bus={}", unix_path),
        if !suffix.is_empty() {
            format!(
                "--env=AT_SPI_BUS_ADDRESS=unix:path=/run/flatpak/at-spi-bus{}",
                suffix
            )
        } else {
            "--env=AT_SPI_BUS_ADDRESS=unix:path=/run/flatpak/at-spi-bus".to_string()
        },
    ])
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
            font_dir_content.push_str(&format!(
                "\t<remap-dir as-path=\"{user_font_dir}\">/run/host/user-fonts</remap-dir>\n",
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
