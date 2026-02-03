use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::process::Command;

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

    for (key, value) in env::vars() {
        if forwarded_env_keys.contains(&key.as_str()) {
            env_vars.insert(key, value);
        }
    }

    env_vars
}

pub fn get_a11y_bus_args() -> Vec<String> {
    let output = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest=org.a11y.Bus",
            "--object-path=/org/a11y/bus",
            "--method=org.a11y.Bus.GetAddress",
        ])
        .output();

    let output = match output {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    let address = String::from_utf8_lossy(&output.stdout)
        .trim()
        .replace("('", "")
        .replace("',)", "");

    let re = Regex::new(r"unix:path=([^,]+)(,.*)?").unwrap();
    let caps = match re.captures(&address) {
        Some(caps) => caps,
        None => return Vec::new(),
    };

    let unix_path = caps.get(1).map_or("", |m| m.as_str());
    let suffix = caps.get(2).map_or("", |m| m.as_str());

    vec![
        format!("--bind-mount=/run/flatpak/at-spi-bus={}", unix_path),
        if !suffix.is_empty() {
            format!("--env=AT_SPI_BUS_ADDRESS=unix:path=/run/flatpak/at-spi-bus{suffix}")
        } else {
            "--env=AT_SPI_BUS_ADDRESS=unix:path=/run/flatpak/at-spi-bus".to_string()
        },
    ]
}
