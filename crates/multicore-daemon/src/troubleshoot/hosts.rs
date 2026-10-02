//! The hosts file: entries other than the stock localhost ones. Read only, never edited.

use std::path::Path;

/// Larger hosts files are cut off (ad-block lists run to megabytes; the first entries are
/// enough to tell the user what is there).
pub const MAX_HOSTS_BYTES: u64 = 1024 * 1024;
const MAX_ENTRY_CHARS: usize = 80;

fn is_localhost_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "localhost"
            | "localhost.localdomain"
            | "localhost4"
            | "localhost4.localdomain4"
            | "localhost6"
            | "localhost6.localdomain6"
            | "ip6-localhost"
            | "ip6-loopback"
            | "ip6-localnet"
            | "ip6-mcastprefix"
            | "ip6-allnodes"
            | "ip6-allrouters"
            | "ip6-allhosts"
            | "broadcasthost"
    )
}

/// Non-comment lines that map anything but the stock localhost names, normalised to
/// `address name...` and shortened for display. The machine's own name on a loopback address
/// (`127.0.1.1 my-pc` on Debian) is stock too.
pub fn custom_entries(contents: &str, machine_name: Option<&str>) -> Vec<String> {
    contents
        .lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or_default().trim();
            let mut fields = line.split_whitespace();
            let address = fields.next()?;
            let names = fields.collect::<Vec<_>>();
            if names.is_empty() {
                return None;
            }
            let loopback = address.starts_with("127.") || address == "::1";
            let stock = names.iter().all(|name| {
                is_localhost_name(name)
                    || (loopback
                        && machine_name.is_some_and(|machine| {
                            name.split('.')
                                .next()
                                .is_some_and(|host| host.eq_ignore_ascii_case(machine))
                        }))
            });
            if stock {
                return None;
            }
            let entry = format!("{address} {}", names.join(" "));
            Some(if entry.chars().count() > MAX_ENTRY_CHARS {
                format!(
                    "{}…",
                    entry.chars().take(MAX_ENTRY_CHARS - 1).collect::<String>()
                )
            } else {
                entry
            })
        })
        .collect()
}

/// Reads at most [`MAX_HOSTS_BYTES`] of `path` (lossy UTF-8); `None` when it cannot be read.
pub fn read_hosts(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_HOSTS_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::custom_entries;

    #[test]
    fn stock_comments_and_localhost_lines_are_not_reported() {
        let windows_default = "# Copyright (c) 1993-2009 Microsoft Corp.\r\n\
            #\r\n\
            #      102.54.94.97     rhino.acme.com          # source server\r\n\
            #\t127.0.0.1       localhost\r\n\
            #\t::1             localhost\r\n";
        assert!(custom_entries(windows_default, None).is_empty());
        let linux_default = "127.0.0.1\tlocalhost\n127.0.1.1\tmy-pc.lan my-pc\n\
            ::1     localhost ip6-localhost ip6-loopback\nff02::1 ip6-allnodes\n\
            ff02::2 ip6-allrouters\n";
        assert!(custom_entries(linux_default, Some("MY-PC")).is_empty());
    }

    #[test]
    fn redirects_and_blocks_are_reported_without_their_comments() {
        let contents = "127.0.0.1 localhost\n\
            0.0.0.0 telemetry.example.com   # blocked\n\
            1.2.3.4\tdiscord.com gateway.discord.gg\n\
            127.0.0.1 localhost evil.example\n\
            just-one-field\n";
        assert_eq!(
            custom_entries(contents, None),
            [
                "0.0.0.0 telemetry.example.com",
                "1.2.3.4 discord.com gateway.discord.gg",
                "127.0.0.1 localhost evil.example",
            ]
        );
        let long = format!("10.0.0.1 {}\n", "a".repeat(200));
        let entry = &custom_entries(&long, None)[0];
        assert_eq!(entry.chars().count(), 80);
        assert!(entry.ends_with('…'));
    }
}
