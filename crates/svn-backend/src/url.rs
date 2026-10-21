//! Repository URLs.
//!
//! A [`RepoUrl`] is stored URI-encoded (as svn prints it in `<url>`), without
//! a trailing slash and without a peg revision. Names coming from
//! `svn list` are unencoded and must be added with [`RepoUrl::join`].

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RepoUrl(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a Subversion URL: {0}")]
pub struct UrlError(pub String);

const SCHEMES: &[&str] = &["http://", "https://", "svn://", "svn+ssh://", "file://"];

impl RepoUrl {
    /// Parse user input or svn output. Accepts encoded or unencoded input
    /// (unsafe characters such as spaces are encoded; existing `%XX` escapes
    /// are kept). Strips trailing slashes.
    pub fn parse(s: &str) -> Result<Self, UrlError> {
        let s = s.trim();
        let scheme = SCHEMES
            .iter()
            .find(|p| s.len() > p.len() && s[..p.len()].eq_ignore_ascii_case(p))
            .ok_or_else(|| UrlError(s.to_owned()))?;
        let rest = &s[scheme.len()..];
        let mut out = String::with_capacity(s.len());
        out.push_str(&scheme.to_ascii_lowercase());
        let bytes = rest.as_bytes();
        // IPv6 literals (`[::1]`) are only legal in the authority part.
        let authority_end = rest.find('/').unwrap_or(rest.len());
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if i < authority_end && (b == b'[' || b == b']') {
                out.push(b as char);
                i += 1;
                continue;
            }
            if b == b'%'
                && i + 2 < bytes.len()
                && bytes[i + 1].is_ascii_hexdigit()
                && bytes[i + 2].is_ascii_hexdigit()
            {
                out.push_str(&rest[i..i + 3]);
                i += 3;
                continue;
            }
            push_encoded(&mut out, b, true);
            i += 1;
        }
        while out.ends_with('/') && out.len() > scheme.len() + 1 {
            out.pop();
        }
        Ok(Self(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Child URL. `name` is an unencoded path segment (or several separated
    /// by `/`, e.g. a repository-relative path).
    pub fn join(&self, name: &str) -> Self {
        let mut out = self.0.clone();
        for seg in name.split('/').filter(|s| !s.is_empty()) {
            out.push('/');
            for &b in seg.as_bytes() {
                push_encoded(&mut out, b, false);
            }
        }
        Self(out)
    }

    /// Parent URL, or `None` at the host root.
    pub fn parent(&self) -> Option<Self> {
        let after_scheme = self.0.find("://")? + 3;
        let idx = self.0.rfind('/')?;
        if idx < after_scheme {
            return None;
        }
        // file:///repo -> parent of "/repo" would be "file://" (empty): stop.
        if idx == after_scheme {
            return None;
        }
        Some(Self(self.0[..idx].to_owned()))
    }

    /// Last path segment, decoded (for display).
    pub fn name(&self) -> String {
        let seg = self.0.rsplit('/').next().unwrap_or("");
        decode(seg)
    }

    /// Decoded repository path of this URL relative to `root`, starting with
    /// `/` (`/` when equal). `None` if this URL is not under `root`.
    pub fn path_from(&self, root: &RepoUrl) -> Option<String> {
        let rest = self.0.strip_prefix(&root.0)?;
        if rest.is_empty() {
            return Some("/".to_owned());
        }
        if !rest.starts_with('/') {
            return None;
        }
        Some(decode(rest))
    }

    /// `scheme://host:port` with the default port filled in (80, 443, 3690,
    /// 22) and the host lower-cased; `file://` for all local repositories.
    /// This is the key for per-host concurrency limits, credentials
    /// ([`crate::AuthPrompt::host_key`]) and certificate trust.
    pub fn host_key(&self) -> String {
        let (scheme, rest) = self.0.split_once("://").unwrap_or(("", &self.0));
        if scheme == "file" {
            return "file://".to_owned();
        }
        let authority = decode(rest.split('/').next().unwrap_or(""));
        let authority = authority.as_str();
        let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, port) = if hostport.starts_with('[') {
            match hostport.find(']') {
                Some(end) => (&hostport[..=end], hostport[end + 1..].strip_prefix(':')),
                None => (hostport, None),
            }
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (hostport, None),
            }
        };
        let default = match scheme {
            "http" => "80",
            "https" => "443",
            "svn" => "3690",
            "svn+ssh" => "22",
            _ => "",
        };
        let port = port.filter(|p| !p.is_empty()).unwrap_or(default);
        format!("{scheme}://{}:{port}", host.to_ascii_lowercase())
    }

    /// `URL@REV` for passing to svn. Always appending a peg makes paths that
    /// themselves contain `@` unambiguous.
    pub fn with_peg(&self, rev: &crate::Rev) -> String {
        format!("{}@{}", self.0, rev.to_svn_arg())
    }
}

impl fmt::Display for RepoUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for RepoUrl {
    type Err = UrlError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn push_encoded(out: &mut String, b: u8, allow_slash: bool) {
    let safe = b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b';'
                | b'='
                | b':'
                | b'@'
        )
        || (allow_slash && b == b'/');
    if safe {
        out.push(b as char);
    } else {
        out.push_str(&format!("%{b:02X}"));
    }
}

/// Percent-decode (lossy for invalid UTF-8).
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rev;

    #[test]
    fn parse_and_join() {
        let u = RepoUrl::parse("https://svn.example.com/repo/trunk/").unwrap();
        assert_eq!(u.as_str(), "https://svn.example.com/repo/trunk");
        let c = u.join("my file@2.txt");
        assert_eq!(c.as_str(), "https://svn.example.com/repo/trunk/my%20file@2.txt");
        assert_eq!(c.name(), "my file@2.txt");
        assert_eq!(c.with_peg(&Rev::Number(5)), format!("{c}@5"));
        assert_eq!(c.parent().unwrap(), u);
    }

    #[test]
    fn parse_keeps_escapes_and_encodes_spaces() {
        let u = RepoUrl::parse("http://h/r/a%20b/c d").unwrap();
        assert_eq!(u.as_str(), "http://h/r/a%20b/c%20d");
    }

    #[test]
    fn path_from_root() {
        let root = RepoUrl::parse("http://h/repo").unwrap();
        let u = root.join("trunk/a b");
        assert_eq!(u.path_from(&root).unwrap(), "/trunk/a b");
        assert_eq!(root.path_from(&root).unwrap(), "/");
        let other = RepoUrl::parse("http://h/repo2").unwrap();
        assert!(other.path_from(&root).is_none());
    }

    #[test]
    fn parent_stops_at_host() {
        let u = RepoUrl::parse("http://h").unwrap();
        assert!(u.parent().is_none());
        let f = RepoUrl::parse("file:///tmp/repo").unwrap();
        assert_eq!(f.parent().unwrap().as_str(), "file:///tmp");
    }

    #[test]
    fn host_keys() {
        let k = |s: &str| RepoUrl::parse(s).unwrap().host_key();
        assert_eq!(k("https://SVN.example.com/repo"), "https://svn.example.com:443");
        assert_eq!(k("http://h:8080/svn/x"), "http://h:8080");
        assert_eq!(k("http://user@h/x"), "http://h:80");
        assert_eq!(k("svn://h"), "svn://h:3690");
        assert_eq!(k("svn+ssh://u@h/r"), "svn+ssh://h:22");
        assert_eq!(k("http://[::1]:8080/r"), "http://[::1]:8080");
        assert_eq!(k("http://[::1]/r"), "http://[::1]:80");
        assert_eq!(k("file:///tmp/repo"), "file://");
    }

    #[test]
    fn ipv6_brackets_kept_in_authority_only() {
        let u = RepoUrl::parse("http://[::1]:8080/r/[x]").unwrap();
        assert_eq!(u.as_str(), "http://[::1]:8080/r/%5Bx%5D");
    }

    #[test]
    fn rejects_non_svn() {
        assert!(RepoUrl::parse("ftp://x").is_err());
        assert!(RepoUrl::parse("trunk").is_err());
    }
}
