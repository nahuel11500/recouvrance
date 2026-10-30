//! `browse_files`: a folder's entries or a file's first bytes in a container, read through
//! exec like the app's Files tab (see `k8s::files`).

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, access_for, format_table};
use crate::k8s::files::{Entry, FileKind, FileSource, Opened, Preview, display_name};
use crate::mcp::Verb;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BrowseParams {
    pub context: String,
    pub namespace: String,
    pub pod: String,
    #[schemars(description = "container name; defaults to the pod's default container")]
    pub container: Option<String>,
    #[schemars(description = "an absolute path in the container: a folder is listed, a file read")]
    pub path: String,
    #[schemars(description = "the most of a file to return, in bytes (default 64 KiB, at most 1 MiB)")]
    pub max_bytes: Option<u64>,
}

/// What a file read returns by default, and at most.
const DEFAULT_BYTES: u64 = 64 * 1024;
const MAX_BYTES: u64 = 1024 * 1024;

#[tool_router(router = file_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "List a folder or read a file in a container, like `kubectl exec -- ls -la` or `head -c`: a folder's entries with mode, owner, size, modification time and symlink targets; a text file's first bytes (a binary file: its type only). Runs `sh`, `stat` and `head` in the container, so it needs the exec verb on the pod."
    )]
    async fn browse_files(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<BrowseParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "pod": p.pod, "container": p.container,
            "path": p.path, "max_bytes": p.max_bytes});
        let mut call = self.begin(&parts, "browse_files", args)?;
        if !p.path.starts_with('/') || p.path.contains('\0') {
            return Err(call.fail(format!("path must be absolute (start with /), got {:?}", p.path)));
        }
        let (client, kind) = self.kind(&call, &p.context, "pods").await?;
        let access = access_for(&kind, &p.context, Verb::Exec, Some(p.namespace.clone()), Some(p.pod.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        let max = p.max_bytes.unwrap_or(DEFAULT_BYTES).clamp(1, MAX_BYTES) as usize;
        let source = FileSource::exec(client, p.namespace, p.pod, p.container);
        let opened = source.open(p.path.as_bytes(), max).await;
        call.finish(opened.map(|o| describe(&p.path, source.origin().label(), &o)))
    }
}

/// What an agent reads for `path`: a folder as a table, a file as its text or its type.
fn describe(path: &str, origin: &str, opened: &Opened) -> String {
    match opened {
        Opened::Folder(entries) => {
            let rows: Vec<Vec<String>> = entries.iter().map(row).collect();
            let n = entries.len();
            let table = format_table(&["MODE", "LINKS", "OWNER", "SIZE", "MODIFIED", "NAME"], &rows);
            format!("{path}: {n} entr{} ({origin})\n{table}", if n == 1 { "y" } else { "ies" })
        }
        Opened::File(preview) => file(path, preview),
    }
}

fn row(e: &Entry) -> Vec<String> {
    let modified = jiff::Timestamp::from_second(e.modified).map_or_else(|_| String::new(), |t| t.to_string());
    let mut name = e.display();
    if e.kind == FileKind::Dir {
        name.push('/');
    }
    if let Some(target) = &e.target {
        name.push_str(&format!(" -> {}", display_name(target)));
    }
    vec![e.mode_string(), e.links.to_string(), e.owner(), e.size_text(), modified, name]
}

fn file(path: &str, preview: &Preview) -> String {
    let shown = preview.bytes.len();
    match preview.text() {
        Some(text) if preview.truncated => {
            format!("{text}\n… [cut after {shown} bytes of {path}: ask for more with max_bytes]")
        }
        Some(text) => text.to_string(),
        None => format!(
            "{path} is binary ({}): not shown. Its first {shown} bytes were read{}.",
            preview.kind(),
            if preview.truncated { "; it is longer" } else { ", the whole file" }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::describe;
    use crate::k8s::files::{Entry, FileKind, Opened, Preview};

    fn entry(name: &[u8], kind: FileKind, mode: u32, target: Option<&[u8]>) -> Entry {
        Entry {
            name: name.to_vec(),
            kind,
            mode,
            links: 1,
            uid: 0,
            gid: 0,
            user: "root".into(),
            group: String::new(),
            size: 2048,
            modified: 0,
            device: None,
            target: target.map(<[u8]>::to_vec),
            target_is_dir: false,
        }
    }

    #[test]
    fn folders_read_like_ls() {
        let folder = Opened::Folder(vec![
            entry(b"ssl", FileKind::Dir, 0o755, None),
            entry(b"a\nb", FileKind::File, 0o644, None),
            entry(b"localtime", FileKind::Symlink, 0o777, Some(b"/usr/share/zoneinfo/UTC")),
        ]);
        let text = describe("/etc", "live container", &folder);
        assert!(text.starts_with("/etc: 3 entries (live container)\n"), "{text}");
        assert!(text.contains("drwxr-xr-x") && text.contains("ssl/"), "{text}");
        // One name a line, whatever it holds.
        assert!(text.contains("a\\nb") && text.contains("root:0") && text.contains("2.0 KiB"), "{text}");
        assert!(text.contains("localtime -> /usr/share/zoneinfo/UTC"), "{text}");
        assert!(text.contains("1970-01-01T00:00:00Z"), "{text}");
    }

    #[test]
    fn files_read_as_text_or_their_type() {
        let text =
            |bytes: &[u8], truncated| describe("/f", "", &Opened::File(Preview { bytes: bytes.to_vec(), truncated }));
        assert_eq!(text(b"hello\n", false), "hello\n");
        assert!(text(b"hello", true).contains("cut after 5 bytes of /f"));
        let binary = text(b"\x7fELF\x02\x01\x01\0", false);
        assert!(binary.contains("ELF executable") && binary.contains("the whole file"), "{binary}");
    }
}
