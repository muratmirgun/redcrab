//! Editing a value in the user's editor: the text goes to a temporary file
//! that `$VISUAL` or `$EDITOR` (else `vi`, or Notepad on Windows) opens,
//! and comes back when it closes. The app hands it the terminal meanwhile.

use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use anyhow::{Context as _, Result, bail};

use crate::app::App;

/// What to do with the text once the editor closed.
pub type AfterEdit = Box<dyn FnOnce(&mut App, Result<String>) + Send>;

/// What to edit, and what to do with the text once the editor closed.
pub struct EditRequest {
    pub text: String,
    /// The file's extension, so the editor highlights it: `json`, `txt`.
    pub extension: &'static str,
    pub then: AfterEdit,
}

/// The editor command: `$VISUAL`, `$EDITOR`, or the system's.
fn editor() -> String {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        })
}

/// Runs the editor on `text` and returns what it saved. Blocks until it
/// closes; the caller has given it the terminal.
pub fn run(text: &str, extension: &str) -> Result<String> {
    run_with(&editor(), text, extension)
}

/// [`run`] with the editor command given.
fn run_with(command: &str, text: &str, extension: &str) -> Result<String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path: PathBuf = std::env::temp_dir().join(format!(
        "redcrab-{}-{}.{extension}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))?;
    let result = edit_file(command, &path);
    let edited = result.and_then(|()| {
        fs::read_to_string(&path).with_context(|| format!("could not read {}", path.display()))
    });
    let _ = fs::remove_file(&path);
    let edited = edited?;
    // Editors end a file with a line break the value did not have.
    Ok(match (text.ends_with('\n'), edited.strip_suffix('\n')) {
        (false, Some(stripped)) => stripped.strip_suffix('\r').unwrap_or(stripped).to_owned(),
        _ => edited,
    })
}

fn edit_file(command: &str, path: &std::path::Path) -> Result<()> {
    // `code --wait`, `subl -w`: the editor's own arguments come first.
    let mut words = command.split_whitespace();
    let program = words.next().context("the editor command is empty")?;
    let status = Command::new(program)
        .args(words)
        .arg(path)
        .status()
        .with_context(|| format!("could not start the editor ({command})"))?;
    if !status.success() {
        bail!("the editor ({command}) exited with {status}; nothing was saved");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn the_text_goes_through_the_editor() {
        // A stand-in editor that appends a line, as a user would.
        let dir = std::env::temp_dir().join(format!("redcrab-editor-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-editor.sh");
        fs::write(&script, "#!/bin/sh\nprintf 'world\\n' >> \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let script = script.to_str().unwrap();
        // The editor's trailing line break is not the value's.
        assert_eq!(run_with(script, "hello ", "txt").unwrap(), "hello world");
        assert_eq!(run_with(script, "a\n", "txt").unwrap(), "a\nworld\n");
        fs::write(script, "#!/bin/sh\nexit 3\n").unwrap();
        assert!(run_with(script, "x", "txt").is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
