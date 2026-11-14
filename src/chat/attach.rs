//! What the user attaches to a message: files from this computer.
//!
//! A file is copied into the chat's own folder when it's sent, so the chat
//! has what was said even after the original moves. Then:
//!
//! - Text (code, logs, notes) is put in the message itself, under its name.
//!   Every brain reads that.
//! - A picture or a PDF goes to the brain the way that brain takes one, as
//!   each driver knows how (a content block, a flag). A brain that can't
//!   take one says so, and the message isn't sent.

use std::path::{Path, PathBuf};

use base64::Engine as _;

/// Files with one message.
pub const MOST_FILES: usize = 10;
/// Text that goes into the message whole.
const MOST_TEXT: u64 = 256 * 1024;
const MOST_IMAGE: u64 = 5 * 1024 * 1024;
const MOST_PDF: u64 = 10 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Text,
    /// A picture, with its media type ("image/png").
    Image(&'static str),
    Pdf,
}

/// A file that goes with a message: the chat's own copy of it.
#[derive(Clone, Debug, PartialEq)]
pub struct Attached {
    /// The file's own name, as the user knows it.
    pub name: String,
    pub path: PathBuf,
    pub kind: Kind,
}

impl Attached {
    /// The file, as base64.
    pub fn base64(&self) -> Result<String, String> {
        let bytes = std::fs::read(&self.path).map_err(|e| format!("Couldn't read {}: {e}", self.name))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

}

fn megabytes(bytes: u64) -> String {
    if bytes < 1024 * 1024 { format!("{} KB", bytes.div_ceil(1024)) } else { format!("{:.1} MB", bytes as f64 / (1024. * 1024.)) }
}

/// What kind of file this is, or why it can't be attached: by its name for
/// pictures and PDFs, by what's in it for the rest (text, or not).
pub fn kind(path: &Path) -> Result<Kind, String> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let meta = std::fs::metadata(path).map_err(|_| format!("There's no file {}.", path.display()))?;
    if meta.is_dir() {
        return Err(format!("{name} is a folder. Attach the files in it."));
    }
    let size = meta.len();
    let too_big = |most: u64| format!("{name} is {}, more than the {} a message takes.", megabytes(size), megabytes(most));
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let image = match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    };
    if let Some(media) = image {
        return if size > MOST_IMAGE { Err(too_big(MOST_IMAGE)) } else { Ok(Kind::Image(media)) };
    }
    if ext == "pdf" {
        return if size > MOST_PDF { Err(too_big(MOST_PDF)) } else { Ok(Kind::Pdf) };
    }
    if size > MOST_TEXT {
        return Err(too_big(MOST_TEXT));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Couldn't read {name}: {e}"))?;
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Err(format!("{name} isn't text, a picture (PNG, JPEG, GIF, WebP) or a PDF, so it can't be attached."));
    }
    Ok(Kind::Text)
}

/// Takes the files at `paths` for a message of the chat kept in `folder`:
/// checks each, and copies it to `folder/files/`. `Err` says, for the user,
/// which one can't be attached and why; nothing is kept then.
pub fn take(folder: &Path, paths: &[PathBuf]) -> Result<Vec<Attached>, String> {
    if paths.len() > MOST_FILES {
        return Err(format!("That's {} files. A message takes {MOST_FILES}.", paths.len()));
    }
    let kinds = paths.iter().map(|p| kind(p)).collect::<Result<Vec<Kind>, String>>()?;
    let dir = folder.join("files");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Couldn't make {}: {e}", dir.display()))?;
    let mut taken = Vec::new();
    for (path, kind) in paths.iter().zip(kinds) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        // Two files of one name (or the same name in an earlier message) each keep their own copy.
        let copy = (1..).map(|n| dir.join(if n == 1 { name.clone() } else { format!("{n}-{name}") })).find(|p| !p.exists()).unwrap_or_else(|| dir.join(&name));
        std::fs::copy(path, &copy).map_err(|e| format!("Couldn't copy {name}: {e}"))?;
        taken.push(Attached { name, path: copy, kind });
    }
    Ok(taken)
}

/// The message as a brain reads it: what the user wrote, then each text
/// file under its name. Pictures and PDFs aren't here; they go the brain's
/// own way.
pub fn with_text(text: &str, files: &[Attached]) -> String {
    let mut message = text.to_string();
    for file in files.iter().filter(|f| f.kind == Kind::Text) {
        let body = std::fs::read_to_string(&file.path).unwrap_or_default();
        // A fence longer than any run of backticks in the file, so the file can't end it early.
        let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(longest.max(2) + 1);
        message.push_str(&format!("\n\nAttached file: {}\n{fence}\n{}\n{fence}", file.name, body.trim_end_matches('\n')));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxin-attach-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_is_text_a_picture_or_a_pdf_or_it_says_why_not() {
        let d = dir("kind");
        let file = |name: &str, bytes: &[u8]| {
            std::fs::write(d.join(name), bytes).unwrap();
            d.join(name)
        };
        assert_eq!(kind(&file("notes.md", b"# Notes\n")), Ok(Kind::Text));
        assert_eq!(kind(&file("Makefile", b"all:\n\ttrue\n")), Ok(Kind::Text));
        assert_eq!(kind(&file("shot.PNG", b"\x89PNG....")), Ok(Kind::Image("image/png")));
        assert_eq!(kind(&file("photo.jpeg", b"\xff\xd8")), Ok(Kind::Image("image/jpeg")));
        assert_eq!(kind(&file("paper.pdf", b"%PDF-1.7")), Ok(Kind::Pdf));
        // What no brain could read is refused in words, before anything is sent.
        assert_eq!(kind(&file("app.bin", b"\x00\x01\x02")).unwrap_err(), "app.bin isn't text, a picture (PNG, JPEG, GIF, WebP) or a PDF, so it can't be attached.");
        assert!(kind(&file("big.log", &vec![b'a'; 300 * 1024])).unwrap_err().starts_with("big.log is 300 KB, more than the 256 KB"));
        assert!(kind(&d).unwrap_err().ends_with("is a folder. Attach the files in it."));
        assert!(kind(&d.join("gone.txt")).unwrap_err().starts_with("There's no file "));
    }

    #[test]
    fn taking_files_copies_them_into_the_chat_and_keeps_two_of_one_name_apart() {
        let d = dir("take");
        let (a, b) = (d.join("a"), d.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("log.txt"), "one").unwrap();
        std::fs::write(b.join("log.txt"), "two").unwrap();
        let chat = d.join("c-1");
        let taken = take(&chat, &[a.join("log.txt"), b.join("log.txt")]).unwrap();
        assert_eq!(taken.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["log.txt", "log.txt"]);
        assert_eq!((taken[0].path.clone(), taken[1].path.clone()), (chat.join("files/log.txt"), chat.join("files/2-log.txt")));
        assert_eq!(std::fs::read_to_string(&taken[1].path).unwrap(), "two");
        // One that can't be attached stops them all, and says which.
        std::fs::write(a.join("x.bin"), [0u8, 1]).unwrap();
        assert!(take(&chat, &[a.join("log.txt"), a.join("x.bin")]).unwrap_err().starts_with("x.bin isn't text"));
        assert!(take(&chat, &vec![a.join("log.txt"); 11]).unwrap_err().starts_with("That's 11 files."));
    }

    #[test]
    fn text_goes_into_the_message_under_its_name_and_pictures_dont() {
        let d = dir("text");
        std::fs::write(d.join("build.log"), "error: nope\n```\nstill the log\n").unwrap();
        let files = [
            Attached { name: "build.log".into(), path: d.join("build.log"), kind: Kind::Text },
            Attached { name: "shot.png".into(), path: d.join("shot.png"), kind: Kind::Image("image/png") },
        ];
        let message = with_text("Why does this fail?", &files);
        assert_eq!(message, "Why does this fail?\n\nAttached file: build.log\n````\nerror: nope\n```\nstill the log\n````");
        assert_eq!(with_text("hi", &[]), "hi");
        // As base64, for the brains that take one that way.
        std::fs::write(d.join("shot.png"), [1u8, 2, 3]).unwrap();
        assert_eq!(files[1].base64().unwrap(), "AQID");
    }
}
