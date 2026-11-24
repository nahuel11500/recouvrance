//! A context's login command (its kubeconfig exec plugin) that failed, said without what it
//! was given or printed. kube's error shows the whole command line, arguments and environment
//! included (where a kubeconfig keeps an OIDC client secret), then the raw output (where the
//! token is). Errors reach pages, agents and the audit log as text, by many paths: so each
//! client puts [`LoginFailed`] in that error's place where it comes out, when it is built (the
//! command runs a first time) and on every request (it runs again when the token expires).
//! The program is named from the kubeconfig ([`program`]): kube's text is never read.
//!
//! The user and password of a URL never show either: not in the error of building a client
//! ([`build_error`]), where kube's error for a proxy it can't use prints the proxy's URL whole,
//! nor in the stderr line said of a failed login command.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use kube::client::AuthError;
use kube::config::AuthInfo;

/// The most characters of the command's error output shown.
const MAX_DETAIL: usize = 200;
/// The longest program name shown.
const MAX_NAME: usize = 64;

/// A failed login command, as [`login_failure`] says it.
#[derive(Debug)]
pub struct LoginFailed(String);

impl std::fmt::Display for LoginFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoginFailed {}

/// The file name of the program a client's login command runs, to name it when it fails:
/// kube's error doesn't say it (`AuthExecStart`), or says it among the arguments. It is a gcp
/// auth provider's `cmd-path` when the user has one (kube tries it before the exec plugin),
/// otherwise the exec plugin's `command`. None without one, or when it is not a plain file
/// name: no name is better than a strange one.
pub fn program(auth: &AuthInfo) -> Option<String> {
    let path = match &auth.auth_provider {
        Some(provider) if provider.name == "gcp" => provider.config.get("cmd-path")?,
        // Other providers run no command, and kube doesn't run the exec plugin then.
        Some(_) => return None,
        None => auth.exec.as_ref()?.command.as_ref()?,
    };
    let name = path.rsplit('/').next()?;
    let ok = name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    (ok && !name.is_empty() && name.len() <= MAX_NAME).then(|| name.to_string())
}

/// What to say when `e`, or an error behind it, is a context's login command that failed: its
/// program's file name, its exit code and the last line it wrote to stderr; never its
/// arguments, environment or standard output. None for any other error. The app's clients
/// have put [`LoginFailed`] in its place already, which names the program; kube's own error,
/// from any other client, is said without the name.
pub fn login_failure(e: &(dyn std::error::Error + 'static)) -> Option<String> {
    failure(e, None)
}

/// [`login_failure`] for a client whose login command runs `program` (see [`program`]).
fn failure(e: &(dyn std::error::Error + 'static), program: Option<&str>) -> Option<String> {
    let command = || program.map_or_else(|| "the login command".to_string(), |p| format!("the login command `{p}`"));
    std::iter::successors(Some(e), |e| e.source()).find_map(|e| {
        if let Some(failed) = e.downcast_ref::<LoginFailed>() {
            return Some(failed.0.clone());
        }
        match e.downcast_ref::<AuthError>()? {
            AuthError::AuthExecRun { out, .. } => Some(run_failed(&command(), out)),
            // An `io::Error` from starting it: "No such file or directory" when it is not on the
            // `PATH` the app was started with.
            AuthError::AuthExecStart(e) => Some(format!("{} could not be started: {e}", command())),
            // The parse error quotes the output around where it failed.
            AuthError::AuthExecParse(_) => Some(format!("{}'s output could not be read as a credential", command())),
            _ => None,
        }
    })
}

/// The error of building a client whose login command runs `program` (see [`program`]), with
/// [`LoginFailed`] in its place when it is that command that failed (building runs it a first
/// time), and without the user and password of any URL in it. kube's error for a proxy it
/// can't use prints the proxy's URL whole, from the kubeconfig's `proxy-url` or `HTTPS_PROXY`:
/// that URL is put back without them first, since a text can't tell that
/// `user:password@host:1080` (no scheme) is a URL. `proxy_cut` says that the proxy's text, as
/// written, holds a `#` (see [`host_only`]). Other errors stay as they are.
pub fn build_error(e: kube::Error, program: Option<&str>, proxy_cut: bool) -> anyhow::Error {
    if let Some(said) = failure(&e, program) {
        return LoginFailed(said).into();
    }
    let e = proxy_host_only(e, proxy_cut);
    // Any other URL in what it says. `{e:?}` says the same texts as `{e:#}`.
    match without_userinfo(&format!("{e:#}")) {
        Cow::Owned(said) => anyhow::Error::msg(said),
        Cow::Borrowed(_) => e,
    }
}

/// kube's error for a proxy it can't use, its URL without user and password (in kube's words,
/// with only the scheme when the host can't be told from the password); any other error as it
/// is.
fn proxy_host_only(e: kube::Error, cut: bool) -> anyhow::Error {
    use kube::Error::{ProxyProtocolDisabled, ProxyProtocolUnsupported};
    match e {
        ProxyProtocolUnsupported { proxy_url } => match host_only(*proxy_url, cut) {
            Ok(url) => ProxyProtocolUnsupported { proxy_url: Box::new(url) }.into(),
            Err(shown) => anyhow::anyhow!("configured proxy {shown} uses an unsupported protocol"),
        },
        ProxyProtocolDisabled { proxy_url, protocol_feature } => match host_only(*proxy_url, cut) {
            Ok(url) => ProxyProtocolDisabled { proxy_url: Box::new(url), protocol_feature }.into(),
            Err(shown) => {
                anyhow::anyhow!("configured proxy {shown} requires the disabled feature {protocol_feature:?}")
            }
        },
        e => e.into(),
    }
}

/// `uri` without the user and password before its host, and without a path or query:
/// `socks5h://user:password@host:1080/` gives `socks5h://host:1080/`, `user:password@host:1080`
/// gives `host:1080`. When the host can't be told apart from the password, the text to show in
/// the URL's place instead: its scheme alone (`socks5h://…`).
///
/// `uri` is what kube read from the kubeconfig or `HTTPS_PROXY`, where the password may hold an
/// unescaped `/`, `?` or `#` (#889). `http::Uri` ends the authority there, so the `@` before the
/// host is in its path or query, or gone with the fragment it drops. A proxy's URL has no path
/// or query: everything up to the last `@` of the whole text goes, and what follows the host.
///
/// A `#` leaves a cut password that can read as a host and port (`user:1234` from
/// `user:1234#x@host:1080`, `word:1` from `user:p@word:1#x@host:1080`): only the text as
/// written tells, so `cut` (that text holds a `#`) shows the scheme alone. Without it, the
/// scheme alone is shown too when what is left can't be a proxy's URL: a URL with a scheme
/// whose port is no number, that has a path or query and no user, or has a user and no port.
fn host_only(uri: http::Uri, cut: bool) -> Result<http::Uri, String> {
    let hidden = || uri.scheme_str().map_or_else(|| "…".to_owned(), |s| format!("{s}://…"));
    if cut {
        return Err(hidden());
    }
    let scheme = uri.scheme().is_some();
    let text = uri.to_string();
    let rest = text.split_once("://").map_or(text.as_str(), |(_, rest)| rest);
    let Some((_, after)) = rest.rsplit_once('@') else {
        let bare = uri.path_and_query().is_none_or(|p| matches!(p.as_str(), "" | "/"));
        let port = uri.authority().and_then(port_is_number) != Some(false);
        return if !scheme || (bare && port) { Ok(uri) } else { Err(hidden()) };
    };
    let host = &after[..after.find(['/', '?']).unwrap_or(after.len())];
    // A host that doesn't read on its own (`u[@h]:1`) goes too: better than the password.
    let Ok(authority) = host.parse::<http::uri::Authority>() else {
        return Err(hidden());
    };
    if scheme && port_is_number(&authority) != Some(true) {
        return Err(hidden());
    }
    let mut parts = http::uri::Parts::default();
    parts.scheme = uri.scheme().cloned();
    parts.authority = Some(authority);
    if scheme {
        parts.path_and_query = Some(http::uri::PathAndQuery::from_static("/"));
    }
    http::Uri::from_parts(parts).map_err(|_| hidden())
}

/// Whether the port after `authority`'s host is a number; `None` without one.
fn port_is_number(authority: &http::uri::Authority) -> Option<bool> {
    let text = authority.as_str();
    let host_end = text.rfind('@').map_or(0, |at| at + 1) + authority.host().len();
    text[host_end..].strip_prefix(':').map(|port| port.parse::<u16>().is_ok())
}

/// `text` without the user and password of any URL in it: after each `://`, everything up to
/// [`userinfo_end`]. A text without them comes back as it is.
fn without_userinfo(text: &str) -> Cow<'_, str> {
    let mut said = String::new();
    // How much of `text` is in `said` or left out.
    let mut done = 0;
    for (at, _) in text.match_indices("://") {
        let start = at + "://".len();
        // In what an earlier URL's user and password reached (`a://b://u@h`): that `@` was the
        // last before the same end, so nothing is left for this one.
        if start < done {
            continue;
        }
        if let Some(end) = userinfo_end(&text[start..]) {
            said.push_str(&text[done..start]);
            done = start + end;
        }
    }
    if done == 0 {
        return Cow::Borrowed(text);
    }
    said.push_str(&text[done..]);
    Cow::Owned(said)
}

/// Where the user and password of a URL end in `rest`, the text after its `://`: just past the
/// last `@` before a space or a character a URL can't hold unescaped; `None` without one.
///
/// An `@` past a `/`, `?` or `#` counts too: a password may hold them unescaped (an invalid URL,
/// but a kubeconfig or a login plugin's config can), and ending the userinfo there would show
/// the rest of it (#889, #1000). A path or query that holds an `@` (`host/a@b`) loses its start
/// with them, the safer side.
pub(crate) fn userinfo_end(rest: &str) -> Option<usize> {
    let url = &rest[..rest.find(ends_url).unwrap_or(rest.len())];
    url.rfind('@').map(|at| at + 1)
}

/// Whether `c` ends a URL in a text: a space, or a character a URL can't hold unescaped.
fn ends_url(c: char) -> bool {
    c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}')
}

/// Puts [`LoginFailed`] in place of a failed login command's error on every request of a
/// client: kube runs the command again on whatever request comes after its token expired.
#[derive(Clone)]
pub struct LoginErrorLayer {
    program: Option<Arc<str>>,
}

impl LoginErrorLayer {
    /// For a client whose login command runs `program` (see [`program`]).
    pub fn new(program: Option<&str>) -> Self {
        Self { program: program.map(Arc::from) }
    }
}

impl<S> tower::Layer<S> for LoginErrorLayer {
    type Service = LoginErrors<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LoginErrors { inner, program: self.program.clone() }
    }
}

#[derive(Clone)]
pub struct LoginErrors<S> {
    inner: S,
    program: Option<Arc<str>>,
}

impl<S, R> tower::Service<R> for LoginErrors<S>
where
    S: tower::Service<R>,
    S::Future: Send + 'static,
    S::Error: Into<tower::BoxError>,
{
    type Response = S::Response;
    type Error = tower::BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, tower::BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(|e| scrub(e.into(), self.program.as_deref()))
    }

    fn call(&mut self, req: R) -> Self::Future {
        let fut = self.inner.call(req);
        let program = self.program.clone();
        Box::pin(async move { fut.await.map_err(|e| scrub(e.into(), program.as_deref())) })
    }
}

/// `e`, or [`LoginFailed`] in its place when it is a failed login command of `program`.
fn scrub(e: tower::BoxError, program: Option<&str>) -> tower::BoxError {
    match failure(&*e, program) {
        Some(said) => Box::new(LoginFailed(said)),
        None => e,
    }
}

/// kube's `AuthExecRun` of `command` ("the login command `kubelogin`"), said by how it ended
/// and its last stderr line.
fn run_failed(command: &str, out: &std::process::Output) -> String {
    let how = ended(out.status);
    match last_line(&out.stderr) {
        Some(line) => format!("{command} failed ({how}): {line}"),
        None => format!("{command} failed ({how})"),
    }
}

/// How the command ended: "exit 1", or "signal 9" when it was killed.
fn ended(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => status.to_string(),
    }
}

/// The last line of `stderr` with text in it, as plain text without the user and password of a
/// URL, at most `MAX_DETAIL` characters.
fn last_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().map(plain).find(|l| !l.is_empty())?;
    // Before the cut, which could leave half a password without the `@` that tells it is one.
    let line = without_userinfo(&line);
    Some(match line.char_indices().nth(MAX_DETAIL) {
        Some((cut, _)) => format!("{}…", line[..cut].trim_end()),
        None => line.into_owned(),
    })
}

/// A line of terminal output as text: color and cursor sequences (`ESC [`, parameters, a final
/// character) and other control characters left out, spaces trimmed.
fn plain(line: &str) -> String {
    let mut text = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else if !c.is_control() {
            text.push(c);
        }
    }
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::{Command, ExitStatus, Output};

    use super::*;

    /// A failed login command as kube reports it: its command line as text, then what it printed.
    fn failed_run(cmd: String, code: i32, stdout: &str, stderr: &str) -> AuthError {
        let out = Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        AuthError::AuthExecRun { cmd, out: Box::new(out) }
    }

    /// kubelogin's command line as kube gives it for an exec plugin (`Command`'s debug form),
    /// with an OIDC client secret in its arguments and another secret in its environment.
    fn kubelogin() -> String {
        let mut cmd = Command::new("/usr/local/bin/kubelogin");
        cmd.args(["get-token", "--oidc-issuer-url=https://issuer.example", "--oidc-client-secret=s3cr3t-arg"]);
        cmd.env("KUBERNETES_EXEC_INFO", r#"{"kind":"ExecCredential","spec":{"interactive":false}}"#);
        cmd.env("LOGIN_ENV", "s3cr3t \"env\" = x");
        cmd.env_remove("S3CR3T_DROPPED");
        format!("{cmd:?}")
    }

    fn expired() -> AuthError {
        let stderr = "warning: no cached token\n\x1b[31merror:\x1b[0m oidc: refresh token expired  \n\n";
        failed_run(kubelogin(), 1, r#"{"status":{"token":"s3cr3t-out"}}"#, stderr)
    }

    const SAID: &str = "the login command `kubelogin` failed (exit 1): error: oidc: refresh token expired";

    /// What the client of a context that logs in with kubelogin says of `e`.
    fn by_kubelogin(e: &(dyn std::error::Error + 'static)) -> Option<String> {
        failure(e, Some("kubelogin"))
    }

    #[test]
    fn a_failed_login_command_says_its_name_exit_code_and_error_only() {
        let e = expired();
        // kube's own text has the arguments, the environment and the output.
        assert!(e.to_string().contains("s3cr3t-arg") && e.to_string().contains("s3cr3t-out"), "{e}");
        assert_eq!(by_kubelogin(&e).as_deref(), Some(SAID));
        // The name comes from the kubeconfig: without it, the rest is said.
        let unnamed = "the login command failed (exit 1): error: oidc: refresh token expired";
        assert_eq!(login_failure(&e).as_deref(), Some(unnamed));
        // Nothing on stderr, killed: said by how it ended.
        let mut quiet = failed_run(kubelogin(), 2, "s3cr3t-out", " \n");
        assert_eq!(by_kubelogin(&quiet).as_deref(), Some("the login command `kubelogin` failed (exit 2)"));
        if let AuthError::AuthExecRun { out, .. } = &mut quiet {
            out.status = ExitStatus::from_raw(9);
        }
        assert_eq!(by_kubelogin(&quiet).as_deref(), Some("the login command `kubelogin` failed (signal 9)"));
        // A long last line is cut.
        let long = failed_run(kubelogin(), 1, "", &"x".repeat(500));
        let said = by_kubelogin(&long).unwrap();
        assert!(said.ends_with(&format!(": {}…", "x".repeat(MAX_DETAIL))), "{said}");
    }

    /// Building the client (`kube::Error::Auth`) gives the few words alone, whichever way it is
    /// printed. A request's error as the client's layer hands it on (in a `Service` error, under
    /// the app's context) is said the same by the pages' and the agents' formatters.
    #[test]
    fn every_path_says_it_without_the_secrets() {
        let built = build_error(kube::Error::Auth(expired()), Some("kubelogin"), false);
        assert!(built.downcast_ref::<LoginFailed>().is_some());
        assert_eq!(format!("{built:#}"), SAID);
        assert!(!format!("{built:?}").contains("s3cr3t"), "{built:?}");
        let sent = kube::Error::Service(scrub(Box::new(expired()), Some("kubelogin")));
        let sent = anyhow::Error::from(sent).context("listing pods");
        for e in [&built, &sent] {
            assert_eq!(crate::k8s::short_error(e), SAID);
            assert_eq!(crate::k8s::error_chain(e), SAID);
        }
        // kube's error as it comes, from a client without the layer: no name, and no secret.
        let raw = anyhow::Error::from(kube::Error::Service(Box::new(expired()))).context("listing pods");
        let unnamed = "the login command failed (exit 1): error: oidc: refresh token expired";
        assert_eq!(crate::k8s::error_chain(&raw), unnamed);
        // Other errors keep what was being done.
        let other = anyhow::anyhow!("connection refused").context("discovery of apps/v1 failed");
        assert_eq!(crate::k8s::error_chain(&other), "discovery of apps/v1 failed: connection refused");
        let refused = build_error(kube::Error::Service("refused".into()), Some("kubelogin"), false);
        assert!(refused.downcast_ref::<LoginFailed>().is_none());
    }

    /// A request whose token refresh runs the login command, which fails with this error.
    struct Refresh(fn() -> AuthError);

    impl tower::Service<()> for Refresh {
        type Response = ();
        type Error = tower::BoxError;
        type Future = std::future::Ready<Result<(), tower::BoxError>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, (): ()) -> Self::Future {
            let e: tower::BoxError = Box::new((self.0)());
            std::future::ready(Err(e))
        }
    }

    /// The error of a request whose token refresh fails with `e`, through the layer of the
    /// client of a context that logs in with kubelogin.
    fn refreshed(e: fn() -> AuthError) -> tower::BoxError {
        use tower::{Layer as _, Service as _};
        let mut client = LoginErrorLayer::new(Some("kubelogin")).layer(Refresh(e));
        futures::executor::block_on(client.call(())).unwrap_err()
    }

    #[test]
    fn the_layer_replaces_the_error_of_every_request() {
        let e = refreshed(expired);
        assert_eq!(e.to_string(), SAID);
        assert!(e.downcast_ref::<LoginFailed>().is_some());
        // As `kube::Client` hands it on.
        let e = kube::Error::Service(e);
        assert_eq!(e.to_string(), format!("ServiceError: {SAID}"));
        assert!(!format!("{e:?}").contains("s3cr3t"), "{e:?}");
        assert_eq!(crate::k8s::short_error(&anyhow::Error::from(e)), SAID);
    }

    #[test]
    fn a_login_command_that_does_not_start_or_prints_no_credential() {
        // Not on the `PATH` the app was started with: named, when the client is built and on a
        // token refresh.
        let not_there = || AuthError::AuthExecStart(std::io::Error::from_raw_os_error(2));
        let built = build_error(kube::Error::Auth(not_there()), Some("kubelogin"), false);
        for said in [format!("{built:#}"), refreshed(not_there).to_string()] {
            assert!(said.starts_with("the login command `kubelogin` could not be started: "), "{said}");
            assert!(said.contains("os error 2"), "{said}");
        }
        let said = login_failure(&not_there()).unwrap();
        assert!(said.starts_with("the login command could not be started: "), "{said}");
        let unreadable = || {
            let e = serde_saphyr::from_str::<std::collections::BTreeMap<String, String>>("token: [s3cr3t").unwrap_err();
            AuthError::AuthExecParse(Box::new(e))
        };
        let built = build_error(kube::Error::Auth(unreadable()), Some("kubelogin"), false);
        assert!(!format!("{built:?}").contains("s3cr3t"), "{built:?}");
        for said in [format!("{built:#}"), refreshed(unreadable).to_string()] {
            assert_eq!(said, "the login command `kubelogin`'s output could not be read as a credential");
        }
        let said = login_failure(&unreadable());
        assert_eq!(said.as_deref(), Some("the login command's output could not be read as a credential"));
        // Other auth errors say what they say.
        assert_eq!(login_failure(&AuthError::MissingCommand), None);
        assert_eq!(login_failure(&kube::Error::Auth(AuthError::ExecPluginFailed)), None);
    }

    #[test]
    fn the_program_is_named_from_the_kubeconfig() {
        let exec = |command: &str| AuthInfo {
            exec: Some(kube::config::ExecConfig { command: Some(command.into()), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(program(&exec("/usr/local/bin/kubelogin")).as_deref(), Some("kubelogin"));
        assert_eq!(program(&exec("aws")).as_deref(), Some("aws"));
        // A gcp auth provider runs its `cmd-path`, whatever its folder and its `cmd-args` hold,
        // which kube's text runs together. kube tries it before the exec plugin.
        for (path, args) in
            [("/Users/John Doe/sdk/bin/gcloud", "config config-helper"), ("/opt/gcloud", r#"--a="b" "c""#)]
        {
            let gcp = kube::config::AuthProviderConfig {
                name: "gcp".into(),
                config: [("cmd-path", path), ("cmd-args", args)].map(|(k, v)| (k.to_string(), v.to_string())).into(),
                ..Default::default()
            };
            let auth = AuthInfo { auth_provider: Some(gcp), ..exec("kubelogin") };
            let e = failed_run(format!("{path} {args}"), 1, "", "");
            assert_eq!(
                failure(&e, program(&auth).as_deref()).as_deref(),
                Some("the login command `gcloud` failed (exit 1)")
            );
        }
        // Another provider runs no command, and kube doesn't run the exec plugin then.
        let oidc = kube::config::AuthProviderConfig { name: "oidc".into(), ..Default::default() };
        assert_eq!(program(&AuthInfo { auth_provider: Some(oidc), ..exec("kubelogin") }), None);
        // A name that is not a plain file name, or no command, names nothing.
        assert_eq!(program(&exec("/bin/a b")), None);
        assert_eq!(program(&exec("/usr/bin/")), None);
        assert_eq!(program(&exec(&"x".repeat(MAX_NAME + 1))), None);
        assert_eq!(program(&exec("")), None);
        assert_eq!(program(&AuthInfo::default()), None);
    }

    #[test]
    fn stderr_is_read_as_plain_text() {
        assert_eq!(plain("\x1b[1;31mfailed\x1b[0m:\tnot\rlogged in "), "failed:notlogged in");
        assert_eq!(last_line(b"first\n\x1b[0m\n  \n"), Some("first".into()));
        assert_eq!(last_line(b"\xffbad\n"), Some("\u{fffd}bad".into()));
        assert_eq!(last_line(b""), None);
        // A URL's user and password are left out, before a cut that would keep half of them (it
        // falls in the password here, and nothing is cut once they are out).
        let said = last_line(b"error: Get https://alice:s3cr3t@issuer.example/token: 401\n");
        assert_eq!(said.as_deref(), Some("error: Get https://issuer.example/token: 401"));
        let said = last_line(b"error: Get https://alice:s3/cr3t@issuer.example/token: 401\n");
        assert_eq!(said.as_deref(), Some("error: Get https://issuer.example/token: 401"));
        let long = format!("{} https://alice:s3cr3t-and-more@issuer.example", "x".repeat(175));
        assert_eq!(last_line(long.as_bytes()), Some(format!("{} https://issuer.example", "x".repeat(175))));
    }

    /// A URL's user and password are left out of a text, wherever it is and whatever is around
    /// it; the rest stays.
    #[test]
    fn a_url_s_user_and_password_are_left_out() {
        let cases = [
            (
                "configured proxy socks5h://alice:s3cr3t@proxy.corp:1080/ uses an unsupported protocol",
                "configured proxy socks5h://proxy.corp:1080/ uses an unsupported protocol",
            ),
            // The last `@` ends them; a quote, a space, a line break or the path ends the host.
            (r#"proxy "https://alice:p@ss@[::1]:3128""#, r#"proxy "https://[::1]:3128""#),
            ("from http://a:b@one to ssh://git@two:22/x?y=z", "from http://one to ssh://two:22/x?y=z"),
            ("socks5://alice@h\nnext@line", "socks5://h\nnext@line"),
            // A `/`, `?` or `#` in the password doesn't end it: the last `@` before a space does
            // (#1000).
            ("connecting to https://alice:pa/ss@api:6443 failed", "connecting to https://api:6443 failed"),
            ("connecting to https://alice:pa?ss@api:6443 failed", "connecting to https://api:6443 failed"),
            ("connecting to https://alice:pa#ss@api:6443 failed", "connecting to https://api:6443 failed"),
            ("https://alice:p/a?s#s@api:6443/x?y#z", "https://api:6443/x?y#z"),
            // Two URLs with nothing between them: the last `@` ends both.
            ("a://b://alice:p/w@api", "a://api"),
            // An `@` in a path or query takes its start along, the safer side.
            ("https://example.com/a@b and https://x?y=z@w", "https://b and https://w"),
        ];
        for (text, said) in cases {
            assert_eq!(without_userinfo(text), said);
        }
        // Without a user or password in a URL, the text itself.
        for text in ["mail alice@example.com about https://example.com/a b@c", "a@b://", "x://", ""] {
            assert!(matches!(without_userinfo(text), Cow::Borrowed(t) if t == text), "{text}");
        }
    }

    /// kube's error for a proxy it can't use prints its URL whole. The error of building the
    /// client names the proxy by its scheme, host and port, never its user and password, however
    /// it is printed; without a scheme too (an `HTTPS_PROXY` that curl reads as http), where only
    /// the URL kube holds tells that it is one (#859).
    #[test]
    fn a_proxy_s_user_and_password_never_show() {
        let unsupported = |shown: &str| format!("configured proxy {shown} uses an unsupported protocol");
        let cases = [
            ("socks5h://alice:s3cr3t@proxy.corp:1080", "socks5h://proxy.corp:1080/"),
            ("alice:s3cr3t@proxy.corp:3128", "proxy.corp:3128"),
            // A proxy has no path or query: they go.
            ("socks4://alice:s3%40cr3t@[::1]:1080/x?y", "socks4://[::1]:1080/"),
            ("socks5h://alice@proxy.corp:1080", "socks5h://proxy.corp:1080/"),
            // An unescaped `/`, `?` or `#` in the password, where `http::Uri` ends the authority
            // (#889): the `@` before the host is in the path or query, or gone with the fragment.
            ("socks5h://alice:s3/cr3t@proxy.corp:1080", "socks5h://proxy.corp:1080/"),
            ("socks5h://alice:s3?cr3t@proxy.corp:1080", "socks5h://proxy.corp:1080/"),
            ("socks5h://alice:s3cr3t#2024@proxy.corp:1080", "socks5h://…"),
            ("socks5h://alice:s3@cr3t#2024@proxy.corp:1080", "socks5h://…"),
            // What a `#` leaves before it, without the `@`: no path or query is shown either.
            ("socks5h://alice:2024/s3cr3t#x@proxy.corp:1080", "socks5h://…"),
            ("socks5h://alice:2024?s3cr3t#x@proxy.corp:1080", "socks5h://…"),
            // With an `@` before the `#`: what follows the host goes.
            ("socks5h://alice:p@h:12/s3cr3t#x@proxy.corp:1080", "socks5h://h:12/"),
            // Without a port after a user, the host may be the end of a password cut at a `#`.
            ("socks5h://alice@proxy.corp", "socks5h://…"),
            // Without a scheme (`HTTPS_PROXY`), the host is shown with or without a port.
            ("alice:s3cr3t@proxy.corp", "proxy.corp"),
        ];
        for (url, shown) in cases {
            let e = kube::Error::ProxyProtocolUnsupported { proxy_url: Box::new(url.parse().unwrap()) };
            assert!(e.to_string().contains("alice"), "{e}");
            let built = build_error(e, Some("kubelogin"), false);
            assert_eq!(crate::k8s::short_error(&built), unsupported(shown));
            for said in
                [format!("{built:#}"), format!("{built:?}"), format!("{built:#?}"), crate::k8s::error_chain(&built)]
            {
                assert!(!said.contains("alice") && !said.contains("cr3t"), "{said}");
            }
        }
        // A password of digits up to a `#` reads as a port: only the text as written tells,
        // and with a `#` in it only the scheme is shown.
        let digits = || Box::new("socks5h://alice:1234#s3cr3t@proxy.corp:1080".parse().unwrap());
        let leaked = build_error(kube::Error::ProxyProtocolUnsupported { proxy_url: digits() }, None, false);
        assert!(format!("{leaked:#}").contains("alice:1234"), "{leaked:#}");
        let built = build_error(kube::Error::ProxyProtocolUnsupported { proxy_url: digits() }, None, true);
        assert_eq!(format!("{built:#}"), unsupported("socks5h://…"));
        let proxy_url = Box::new("socks5://alice:s3cr3t@proxy.corp:1080".parse().unwrap());
        let built =
            build_error(kube::Error::ProxyProtocolDisabled { proxy_url, protocol_feature: "kube/socks5" }, None, false);
        let said = r#"configured proxy socks5://proxy.corp:1080/ requires the disabled feature "kube/socks5""#;
        assert_eq!(format!("{built:#}"), said);
        let proxy_url = Box::new("socks5://alice:s3cr3t#x@proxy.corp:1080".parse().unwrap());
        let built =
            build_error(kube::Error::ProxyProtocolDisabled { proxy_url, protocol_feature: "kube/socks5" }, None, false);
        let said = r#"configured proxy socks5://… requires the disabled feature "kube/socks5""#;
        assert_eq!(format!("{built:#}"), said);
        // Another error that prints a URL: its user and password go, the rest is said, even with
        // a `/`, `?` or `#` in the password (#1000).
        for password in ["s3cr3t", "s3/cr3t", "s3?cr3t", "s3#cr3t"] {
            let text = format!("connecting to https://alice:{password}@api:6443 failed");
            let other = build_error(kube::Error::Service(text.into()), None, false);
            let said = format!("{other:#}");
            assert!(
                said.starts_with("ServiceError: connecting to https://api:6443 failed")
                    && !said.contains("alice")
                    && !said.contains("cr3t"),
                "{said}"
            );
        }
        // Without one, kube's error as it was.
        let refused = build_error(kube::Error::Service("refused".into()), None, false);
        assert!(matches!(refused.downcast_ref::<kube::Error>(), Some(kube::Error::Service(_))));
    }
}

/// Whatever a failed login command was given and printed, what is said of it.
#[cfg(test)]
mod properties {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::process::{Command, ExitStatus, Output};

    use kube::config::{AuthInfo, ExecConfig};
    use proptest::prelude::*;

    use super::{AuthError, MAX_DETAIL, failure, program};

    /// In everything the command was given and printed, never in what is said of it.
    const SECRET: &str = "XSECRETX";

    /// What the app's client says of its login command `path` (the kubeconfig's `command`) when
    /// kube reports it as `cmd`, which exited with `code` after printing this.
    fn say(path: &str, cmd: &Command, code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> String {
        let exec = ExecConfig { command: Some(path.into()), ..Default::default() };
        let auth = AuthInfo { exec: Some(exec), ..Default::default() };
        let out = Output { status: ExitStatus::from_raw(code << 8), stdout, stderr };
        failure(&AuthError::AuthExecRun { cmd: format!("{cmd:?}"), out: Box::new(out) }, program(&auth).as_deref())
            .unwrap()
    }

    proptest! {
        /// The program's folder, its arguments, environment, working folder, `argv[0]` and
        /// standard output never show, whatever they hold (quotes, spaces, `=`, backslashes,
        /// any character); the program's file name and its exit code always do, and so does
        /// the last line of stderr with text in it.
        #[test]
        fn a_failed_login_never_shows_what_it_was_given(
            dir in proptest::option::of("[^\\x00]{0,12}"),
            name in "[a-z][a-z0-9._-]{0,15}",
            args in proptest::collection::vec("[^\\x00]{0,12}", 0..4),
            env in proptest::collection::vec(("[A-Z_]{0,6}", "[^\\x00]{0,12}"), 0..3),
            removed in proptest::collection::vec("[A-Z_]{0,6}", 0..2),
            cwd in proptest::option::of("[^\\x00]{0,12}"),
            clear in any::<bool>(),
            arg0 in proptest::option::of("[^\\x00]{0,12}"),
            code in 1..=255i32,
            stdout in "[^\\x00]{0,20}",
            stderr in proptest::collection::vec("[a-z :]{0,20}", 0..4),
        ) {
            let path = dir.map_or_else(|| name.clone(), |dir| format!("/{SECRET}{dir}/{name}"));
            let mut cmd = Command::new(&path);
            cmd.args(args.iter().map(|a| format!("{SECRET}{a}")));
            if clear {
                cmd.env_clear();
            }
            for key in &removed {
                cmd.env_remove(format!("{SECRET}{key}"));
            }
            for (key, value) in &env {
                cmd.env(format!("{SECRET}{key}"), format!("{SECRET}{value}"));
            }
            if let Some(cwd) = &cwd {
                cmd.current_dir(format!("{SECRET}{cwd}"));
            }
            if let Some(arg0) = &arg0 {
                cmd.arg0(format!("{SECRET}{arg0}"));
            }
            let stdout = format!("{SECRET}{stdout}").into_bytes();
            let said = say(&path, &cmd, code, stdout, stderr.join("\n").into_bytes());
            prop_assert!(!said.contains(SECRET), "{said}\nfrom {cmd:?}");
            prop_assert!(said.contains(&format!("`{name}`")), "{said}\nfrom {cmd:?}");
            prop_assert!(said.contains(&format!("(exit {code})")), "{said}");
            match stderr.iter().rev().map(String::as_str).map(str::trim).find(|l| !l.is_empty()) {
                Some(last) => prop_assert!(said.ends_with(&format!("): {last}")), "{said}"),
                None => prop_assert!(said.ends_with(')'), "{said}"),
            }
        }

        /// Whatever bytes it wrote to stderr, one line of text comes out: no control
        /// characters, at most `MAX_DETAIL` characters and an ellipsis.
        #[test]
        fn stderr_comes_out_as_one_short_line(stderr in proptest::collection::vec(any::<u8>(), 0..600)) {
            let said = say("kubelogin", &Command::new("kubelogin"), 1, Vec::new(), stderr);
            prop_assert!(!said.chars().any(char::is_control), "{said:?}");
            let detail = said.strip_prefix("the login command `kubelogin` failed (exit 1)").unwrap();
            prop_assert!(detail.is_empty() || detail.starts_with(": "), "{said:?}");
            prop_assert!(detail.chars().count() <= ": ".len() + MAX_DETAIL + 1, "{said:?}");
        }
    }
}

/// Whatever a URL's user and password hold and whatever is around them, they never show.
#[cfg(test)]
mod url_properties {
    use std::borrow::Cow;

    use proptest::prelude::*;

    use super::{build_error, without_userinfo};

    /// At the start of every user of a proxy's URL, never in what is said of it.
    const SECRET: &str = "XSECRETX";
    /// In a proxy's password, after the digits and the `/`, `?`, `#` or `@` it may start with.
    const PASSWORD: &str = "XPASSWORDX";

    /// Texts made of URL pieces, so that many hold `://` and `@` in every order, or any text.
    fn texts() -> impl Strategy<Value = String> {
        let piece = prop_oneof![
            Just("://"),
            Just("@"),
            Just(":"),
            Just("/"),
            Just(" "),
            Just("a"),
            Just("?"),
            Just("#"),
            Just("\""),
            Just("["),
            Just("é")
        ];
        prop_oneof![proptest::collection::vec(piece, 0..24).prop_map(|p| p.concat()), "\\PC{0,40}"]
    }

    proptest! {
        /// A URL in a text loses its user and password, whatever they hold (an unescaped `/`,
        /// `?`, `#` or `@` too, #1000) and whatever text is around it; the rest of the text, the URL's scheme, host, port and path included, stays
        /// as it was.
        #[test]
        fn a_url_in_a_text_never_shows_its_user_and_password(
            before in "[^@]{0,20}",
            scheme in "[a-z][a-z0-9+.-]{0,8}",
            user in "[a-zA-Z0-9._~!$&'()*+,;=%-]{0,8}",
            password in proptest::option::of("[a-zA-Z0-9._~!$&'()*+,;=%:@/?#-]{0,8}"),
            host in "[a-z0-9.-]{1,16}|\\[[0-9a-f:]{2,12}\\]",
            port in proptest::option::of(any::<u16>()),
            path in "([/?#][^@]{0,12})?",
            after in "([ \"<>\\n][^@]{0,20})?",
        ) {
            let userinfo = password.map_or_else(|| user.clone(), |p| format!("{user}:{p}"));
            let host = port.map_or_else(|| host.clone(), |p| format!("{host}:{p}"));
            let text = format!("{before}{scheme}://{userinfo}@{host}{path}{after}");
            let said = without_userinfo(&text);
            prop_assert_eq!(said, format!("{before}{scheme}://{host}{path}{after}"));
        }

        /// Whatever the text, what comes back holds no user or password of a URL: read again,
        /// it comes back the same. A text without both a `://` and an `@` comes back as it is.
        /// A URL put anywhere in it never shows its password, whatever it holds (`/`, `?`, `#`
        /// or `@` too, #1000) and whatever comes before or after it.
        #[test]
        fn nothing_is_left_to_leave_out(
            text in texts(),
            (before, after) in (texts(), texts()),
            password in "[a-zA-Z0-9._~!$&'()*+,;=%:@/?#-]{0,8}",
        ) {
            let once = without_userinfo(&text).into_owned();
            prop_assert_eq!(without_userinfo(&once), once.as_str());
            if !(text.contains("://") && text.contains('@')) {
                prop_assert!(matches!(without_userinfo(&text), Cow::Borrowed(t) if t == text), "{text}");
            }
            let text = format!("{before}https://{SECRET}:{password}@{after}");
            let said = without_userinfo(&text);
            prop_assert!(!said.contains(SECRET), "{said}\nfrom {text}");
            prop_assert_eq!(without_userinfo(&said), said.as_ref());
        }

        /// Whatever a proxy's URL holds, with a scheme or without, the error of building a client
        /// with it never shows its user and password, however it is printed (#859), even with an
        /// unescaped `/`, `?` or `#` in the password, after digits that read as a port (#889). It
        /// names the proxy by its host and port, but by its scheme alone when the text holds a
        /// `#`, as the app tells it. Without being told, from the URL kube holds alone, what
        /// follows a `/`, `?` or `#` in the password never shows either.
        #[test]
        fn a_proxy_never_shows_its_user_and_password(
            scheme in proptest::option::of("[a-z][a-z0-9+.-]{0,8}"),
            user in "[a-zA-Z0-9._~!$&'()*+,;=-]{0,8}",
            password in proptest::option::of((
                "[0-9]{0,5}",
                prop_oneof![Just(""), Just("/"), Just("?"), Just("#"), Just("@")],
                "[a-zA-Z0-9._~!$&'()*+,;=%:@/?#-]{0,8}",
            )),
            host in "[a-z0-9][a-z0-9.-]{0,15}|\\[[0-9a-f:]{2,12}\\]",
            port in any::<u16>(),
        ) {
            let sep = password.as_ref().map_or("", |(_, sep, _)| *sep);
            let password = password.map(|(digits, sep, rest)| format!("{digits}{sep}{PASSWORD}{rest}"));
            let cut = password.as_deref().is_some_and(|p| p.contains('#'));
            let userinfo = password.map_or_else(|| format!("{SECRET}{user}"), |p| format!("{SECRET}{user}:{p}"));
            let at = format!("{host}:{port}");
            let url = scheme.as_ref().map_or_else(|| format!("{userinfo}@{at}"), |s| format!("{s}://{userinfo}@{at}"));
            // Not one kube reads (too many colons between brackets): nothing to check.
            let proxy_url = url.parse::<http::Uri>().map_err(|e| TestCaseError::reject(format!("{url}: {e}")))?;
            let unsupported = || kube::Error::ProxyProtocolUnsupported { proxy_url: Box::new(proxy_url.clone()) };
            let built = build_error(unsupported(), None, cut);
            for said in [format!("{built:#}"), format!("{built:?}"), format!("{built:#?}")] {
                prop_assert!(!said.contains(SECRET) && !said.contains(PASSWORD), "{said}\nfrom {url}");
                prop_assert!(cut || said.contains(&at), "{said}\nfrom {url}");
            }
            if let Some(scheme) = &scheme {
                let said = format!("{built:#}");
                let shown = if cut { format!("{scheme}://…") } else { format!("{scheme}://{at}") };
                prop_assert!(said.contains(&shown), "{said}\nfrom {url}");
            }
            // After an `@`, the end of a password cut at a `#` reads as a host: only the text tells.
            if scheme.is_some() && sep != "@" {
                let built = build_error(unsupported(), None, false);
                for said in [format!("{built:#}"), format!("{built:?}"), format!("{built:#?}")] {
                    prop_assert!(!said.contains(PASSWORD), "{said}\nfrom {url}");
                }
            }
        }
    }
}
