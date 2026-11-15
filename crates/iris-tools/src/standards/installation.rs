//! What tells one installation from another (`AGENTS.md` rule 9): addresses,
//! local paths, people's mail addresses, hardware addresses, and the names a
//! local list holds.

use super::text;

/// The address ranges a committed file may name: loopback, the unspecified
/// and broadcast addresses, multicast, and the documentation ranges of
/// RFC 5737.
fn is_example(address: [u8; 4]) -> bool {
    matches!(
        address,
        [0 | 127 | 255, ..] | [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _]
    ) || address[0] >= 224
}

/// The ranges only a private network uses: RFC 1918, link-local, and the
/// shared range of RFC 6598.
fn is_private(address: [u8; 4]) -> bool {
    matches!(address, [10, ..] | [192, 168, ..] | [169, 254, ..])
        || (address[0] == 172 && (16..32).contains(&address[1]))
        || (address[0] == 100 && (64..128).contains(&address[1]))
}

/// The dotted quads of `line`, each with whether it stands where a host does:
/// after `//` or `@`, or before a port or a prefix length. A clause number
/// such as 4.4.6.1 stands in neither place.
fn addresses(line: &str) -> Vec<([u8; 4], bool)> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if !bytes[at].is_ascii_digit() {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'.') {
            at += 1;
        }
        let run = line[start..at].trim_end_matches('.');
        let end = start + run.len();
        let parts: Vec<&str> = run.split('.').collect();
        let octets: Vec<u8> = parts.iter().filter_map(|part| part.parse().ok()).collect();
        let (Ok(address), true) = (<[u8; 4]>::try_from(octets), parts.len() == 4) else {
            continue;
        };
        let before = &line[..start];
        let after = &bytes[end..];
        let hosted = before.ends_with("//")
            || before.ends_with('@')
            || (matches!(after.first(), Some(b':' | b'/'))
                && after.get(1).is_some_and(u8::is_ascii_digit));
        let free = !before.ends_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_');
        if free {
            found.push((address, hosted));
        }
    }
    found
}

/// The mail addresses of `line`.
fn mail_addresses(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let local = |byte: u8| byte.is_ascii_alphanumeric() || b"._%+-".contains(&byte);
    let domain = |byte: u8| byte.is_ascii_alphanumeric() || b".-".contains(&byte);
    let mut found = Vec::new();
    for (at, _) in line.match_indices('@') {
        let start = at - bytes[..at].iter().rev().take_while(|b| local(**b)).count();
        let reach = bytes[at + 1..].iter().take_while(|b| domain(**b)).count();
        let whole = line[start..=at + reach].trim_end_matches(['.', '-']);
        let Some((name, host)) = whole.split_once('@') else {
            continue;
        };
        let top = host.rsplit('.').next().unwrap_or("");
        let named = top.len() >= 2 && top.bytes().all(|b| b.is_ascii_alphabetic());
        if !name.is_empty() && host.contains('.') && named {
            found.push(whole);
        }
    }
    found
}

/// Whether a mail address is nobody's: an example domain (RFC 2606) or the
/// address GitHub gives an account in place of its own.
fn is_placeholder(address: &str) -> bool {
    let host = address.rsplit('@').next().unwrap_or("");
    ["example.com", "example.org", "example.net", "example"]
        .iter()
        .any(|example| host == *example || host.ends_with(&format!(".{example}")))
        || host == "users.noreply.github.com"
}

/// Whether `line` holds six colon-separated octets that are neither all zero
/// nor the broadcast address: a network interface's address.
fn has_hardware_address(line: &str) -> bool {
    let groups: Vec<&str> = line
        .split(|ch: char| !(ch.is_ascii_hexdigit() || ch == ':'))
        .collect();
    groups.into_iter().any(|group| {
        let octets: Vec<&str> = group.split(':').collect();
        octets.len() == 6
            && octets.iter().all(|octet| octet.len() == 2)
            && !octets.iter().all(|octet| *octet == "00")
            && !octets.iter().all(|octet| octet.eq_ignore_ascii_case("ff"))
    })
}

/// The local paths of `line`: a home directory or a mount point by name.
fn local_paths(line: &str) -> Vec<&'static str> {
    const ROOTS: [&str; 5] = ["/home/", "/Users/", "/mnt/", "/media/", "/run/media/"];
    let mut found = Vec::new();
    for root in ROOTS {
        for (at, _) in line.match_indices(root) {
            let apart = line[..at]
                .chars()
                .next_back()
                .is_none_or(|ch| !(ch.is_alphanumeric() || "._-/~>}".contains(ch)));
            let named = line[at + root.len()..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
            if apart && named && !found.contains(&root) {
                found.push(root);
            }
        }
    }
    found
}

/// Whether a line may hold the repository's own address: rule 9 lets the
/// badge and workflow links and the manifests' `repository` key hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Own {
    /// The line is in none of those files.
    Refused,
    /// The line is in one of them.
    Allowed,
}

/// What a site's files may and may not name.
#[derive(Debug, Default, Clone)]
pub struct Site {
    /// Names no committed file may hold, lowercase: people, hosts, devices.
    /// They come from a file git ignores, so a clone without it checks none.
    pub denied: Vec<String>,
    /// Texts a committed file may hold though a pattern refuses them: the
    /// addresses and paths of published sources.
    pub allowed: Vec<String>,
    /// The repository's own address, lowercase, as `owner/name` and the forms
    /// its pages take. The files rule 9 names may hold it.
    pub own: Vec<String>,
}

impl Site {
    /// What `line` says of an installation, one sentence a breach.
    pub fn scan(&self, line: &str, own: Own) -> Vec<String> {
        let mut found = self.names(line, own);
        let mut line = line.to_owned();
        for allowed in &self.allowed {
            line = line.replace(allowed, " ");
        }
        let line = line.as_str();
        for (address, hosted) in addresses(line) {
            let [a, b, c, d] = address;
            if is_private(address) || (hosted && !is_example(address)) {
                found.push(format!(
                    "address {a}.{b}.{c}.{d}: use an RFC 5737 range (192.0.2.0/24)"
                ));
            }
        }
        for root in local_paths(line) {
            found.push(format!("a local path under {root}"));
        }
        for address in mail_addresses(line) {
            if !is_placeholder(address) {
                found.push(format!("mail address {address}"));
            }
        }
        if has_hardware_address(line) {
            found.push("a hardware address".to_owned());
        }
        found
    }

    /// The names of the local list that `line` holds: the one check that
    /// applies to copies of published sources, whose own addresses stay.
    pub fn names(&self, line: &str, own: Own) -> Vec<String> {
        let mut found = Vec::new();
        let mut lower = line.to_lowercase();
        if own == Own::Allowed {
            for own in &self.own {
                lower = lower.replace(own, " ");
            }
        }
        for term in &self.denied {
            if text::has_word(&lower, term) {
                found.push(format!("the name {term:?} from the local list"));
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(line: &str) -> Vec<String> {
        Site::default().scan(line, Own::Refused)
    }

    #[test]
    fn private_addresses_are_refused_and_example_ranges_pass() {
        assert_eq!(scan("camera at 192.168.1.20.").len(), 1);
        assert_eq!(scan("10.0.0.7 and 172.16.4.1 and 169.254.1.1").len(), 3);
        assert_eq!(
            scan("rtsp://192.0.2.10:554/ and 127.0.0.1:8080"),
            Vec::<String>::new()
        );
        assert_eq!(scan("multicast 239.255.255.250:3702"), Vec::<String>::new());
    }

    #[test]
    fn a_public_address_is_refused_only_where_a_host_stands() {
        assert_eq!(scan("https://8.8.8.8/x").len(), 1);
        assert_eq!(scan("listen on 8.8.4.4:53").len(), 1);
        assert_eq!(
            scan("ISO/IEC 14496-12 clause 8.8.4.4 and 6.2.5.2."),
            Vec::<String>::new()
        );
        assert_eq!(scan("version 1.2.3.4.5 and v1.2.3"), Vec::<String>::new());
    }

    #[test]
    fn local_paths_are_refused() {
        assert_eq!(
            scan("see /home/someone/notes"),
            vec!["a local path under /home/"]
        );
        assert_eq!(scan("root=\"/mnt/disk1\"").len(), 1);
        assert_eq!(
            scan("crates/iris-api/media/x and /var/lib/irisd"),
            Vec::<String>::new()
        );
        assert_eq!(scan("a mount such as /mnt/<disk>"), Vec::<String>::new());
    }

    #[test]
    fn mail_addresses_of_people_are_refused() {
        assert_eq!(scan("ask someone@corp.io.").len(), 1);
        assert_eq!(
            scan("user@example.com, 1+a@users.noreply.github.com"),
            Vec::<String>::new()
        );
        assert_eq!(
            scan("rtsp://user:pass@192.0.2.1/ and x @ Foo"),
            Vec::<String>::new()
        );
        let site = Site {
            allowed: vec!["list@project.org".to_owned(), "/media/product/".to_owned()],
            ..Site::default()
        };
        let cited = "write list@project.org about `/media/product/x`";
        assert_eq!(site.scan(cited, Own::Refused), Vec::<String>::new());
    }

    #[test]
    fn hardware_addresses_are_refused() {
        assert_eq!(scan("mac 3c:ef:8c:01:02:03").len(), 1);
        assert_eq!(
            scan("00:00:00:00:00:00 and 12:34:56 and 01:02:03:04:05:06:07"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn listed_names_are_refused_except_in_the_repositorys_address() {
        let site = Site {
            denied: vec!["owner".to_owned(), "acme".to_owned()],
            own: vec!["owner/project".to_owned()],
            ..Site::default()
        };
        assert_eq!(site.scan("an Acme camera", Own::Refused).len(), 1);
        assert_eq!(site.scan("acmes", Own::Refused), Vec::<String>::new());
        assert_eq!(
            site.scan("github.com/Owner/project", Own::Allowed),
            Vec::<String>::new()
        );
        assert_eq!(site.scan("github.com/Owner/project", Own::Refused).len(), 1);
    }
}
