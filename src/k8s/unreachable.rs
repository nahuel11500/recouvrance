//! Why a connection to the API server failed, in a few words. kube hands on hyper's chain as
//! it is (`ServiceError: client error (Connect): client error (Connect): tcp connect error:
//! Connection refused (os error 111)`): the same step twice, and not where it went.

use std::error::Error as StdError;
use std::net::SocketAddr;

/// What to say when `e`, or an error behind it, is a connection to the API server that failed:
/// refused (by the address that refused it), a host that can't be resolved (with the
/// resolver's reason: no such host and a resolver that can't be reached read apart), a TLS
/// handshake that failed, a timeout, or else its last cause. Worded for whichever hop failed,
/// the API server or a proxy on the way. None for any other error (the API server's answer, a
/// login command...). No URL in it keeps its user and password.
pub(super) fn connect_failure(e: &(dyn StdError + 'static)) -> Option<String> {
    let causes: Vec<&(dyn StdError + 'static)> = std::iter::successors(Some(e), |c| (*c).source()).collect();
    if !causes.iter().any(|c| is_connect(*c)) {
        return None;
    }
    let addr = causes.iter().find_map(|c| address(*c));
    let root = causes.last().map_or_default(ToString::to_string);
    let said = if let Some(tls) = causes.iter().find_map(|c| tls(*c)) {
        match addr {
            Some(a) => format!("TLS handshake failed with {a}: {tls}"),
            None => format!("TLS handshake failed: {tls}"),
        }
    } else if causes.iter().any(|c| io_kind(*c) == Some(std::io::ErrorKind::ConnectionRefused)) {
        match addr {
            Some(a) => format!("Connection refused by {a}"),
            None => "Connection refused".to_string(),
        }
    } else if causes.iter().any(|c| c.to_string() == "dns error") {
        format!("Can't resolve the API server's host: {root}")
    } else if causes.iter().any(|c| io_kind(*c) == Some(std::io::ErrorKind::TimedOut) || timed_out(*c)) {
        match addr {
            Some(a) => format!("The cluster did not answer (timed out connecting to {a})"),
            None => "The cluster did not answer (timed out connecting to the API server)".to_string(),
        }
    } else {
        match addr {
            Some(a) => format!("Can't connect to {a}: {root}"),
            None => format!("Can't connect to the API server: {root}"),
        }
    };
    Some(super::login::without_userinfo(&said).into_owned())
}

/// Whether `c` is the error of hyper's client for a connection it could not open.
pub(super) fn is_connect(c: &(dyn StdError + 'static)) -> bool {
    c.downcast_ref::<hyper_util::client::legacy::Error>().is_some_and(hyper_util::client::legacy::Error::is_connect)
}

/// The address a connection was refused by, as hyper's `ConnectError` keeps it: only its
/// `Debug` says it (`ConnectError("tcp connect error", 127.0.0.1:1, Os { .. })`), and the type
/// is not public. None when it doesn't read as one.
fn address(c: &(dyn StdError + 'static)) -> Option<SocketAddr> {
    let debug = format!("{c:?}");
    let rest = debug.strip_prefix("ConnectError(\"")?;
    let rest = &rest[rest.find("\", ")? + 3..];
    let end = rest.find([',', ')']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

/// The TLS error of a handshake that failed: tokio-rustls hands it on inside an `io::Error`,
/// which the connector may wrap in another, and an `io::Error`'s `source` skips what it holds.
fn tls(c: &(dyn StdError + 'static)) -> Option<String> {
    let mut e = c;
    loop {
        if let Some(tls) = e.downcast_ref::<rustls::Error>() {
            return Some(tls.to_string());
        }
        e = e.downcast_ref::<std::io::Error>()?.get_ref()?;
    }
}

fn io_kind(c: &(dyn StdError + 'static)) -> Option<std::io::ErrorKind> {
    c.downcast_ref::<std::io::Error>().map(std::io::Error::kind)
}

/// A connect deadline that elapsed (kube's `connect_timeout`).
fn timed_out(c: &(dyn StdError + 'static)) -> bool {
    c.to_string().contains("deadline has elapsed")
}

#[cfg(test)]
mod tests {
    use crate::k8s::{error_chain, short_error};

    /// A client of the API server at `url`, without a kubeconfig.
    fn client(url: &str) -> kube::Client {
        kube::Client::try_from(kube::Config::new(url.parse().unwrap())).unwrap()
    }

    /// A port nothing listens on (bound, then let go).
    fn closed_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// A refused connection, as kube hands it on, is said in a few words with the address
    /// that refused it, by the pages' formatter and by the agents' (after what was being done),
    /// never as hyper's chain.
    #[tokio::test]
    async fn a_refused_connection_says_where() {
        let port = closed_port();
        let e = client(&format!("https://127.0.0.1:{port}")).apiserver_version().await.unwrap_err();
        let e = anyhow::Error::from(e).context("discovery failed");
        let said = format!("Connection refused by 127.0.0.1:{port}");
        assert_eq!(short_error(&e), said);
        assert_eq!(error_chain(&e), format!("discovery failed: {said}"));
        // A cause that already prints the next one (kube-runtime's watcher error) is left out
        // with it: hyper's text never comes back through it.
        #[derive(Debug)]
        struct Watcher(kube::Error);
        impl std::fmt::Display for Watcher {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "failed to perform initial object list: {}", self.0)
            }
        }
        impl std::error::Error for Watcher {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let e = client(&format!("https://127.0.0.1:{port}")).apiserver_version().await.unwrap_err();
        let e = anyhow::Error::from(Watcher(e)).context("listing pods");
        assert_eq!(error_chain(&e), format!("listing pods: {said}"));
    }

    /// A server that doesn't speak TLS: the handshake's own error, not hyper's chain.
    #[tokio::test]
    async fn a_failed_handshake_says_tls() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            while let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });
        let e = client(&format!("https://127.0.0.1:{port}")).apiserver_version().await.unwrap_err();
        let said = short_error(&anyhow::Error::from(e));
        // An io::Error held in another, as a connector may hand it on.
        let rustls = std::io::Error::new(std::io::ErrorKind::InvalidData, rustls::Error::DecryptError);
        let wrapped = std::io::Error::other(rustls);
        assert_eq!(super::tls(&wrapped), Some(rustls::Error::DecryptError.to_string()));
        assert!(said.starts_with("TLS handshake failed"), "{said}");
        assert!(said.contains("received corrupt message"), "{said}");
        assert!(!said.contains("client error"), "{said}");
    }

    /// Errors that are not a connection keep their own words.
    #[test]
    fn other_errors_are_left_alone() {
        let other = anyhow::anyhow!("connection refused").context("discovery of apps/v1 failed");
        assert_eq!(error_chain(&other), "discovery of apps/v1 failed: connection refused");
        assert_eq!(super::connect_failure(other.as_ref()), None);
    }

    #[test]
    fn the_address_is_read_from_hypers_error() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct ConnectError(&'static str, std::net::SocketAddr, std::io::Error);
        impl std::fmt::Display for ConnectError {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for ConnectError {}
        for addr in ["127.0.0.1:1", "[::1]:6443"] {
            let e =
                ConnectError("tcp connect error", addr.parse().unwrap(), std::io::ErrorKind::ConnectionRefused.into());
            assert_eq!(super::address(&e), Some(addr.parse().unwrap()));
        }
        assert_eq!(super::address(&std::io::Error::from(std::io::ErrorKind::ConnectionRefused)), None);
    }
}
